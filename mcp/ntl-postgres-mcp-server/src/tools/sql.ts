/**
 * SQL execution tools.
 *
 * The security-critical pair. `ntl_execute_sql` runs arbitrary SQL, and
 * `ntl_apply_migration` changes schema. Both rely on the database's own
 * read-only enforcement rather than inspecting the SQL — see `safety.ts` for
 * why any blocklist approach is bypassable.
 */

import { z } from "zod";

import { DEFAULT_MAX_ROWS } from "../constants.js";
import { failFromError, jsonSafe, respond, rowsToMarkdown } from "../format.js";
import type { ToolOutput } from "../format.js";
import {
  explainMultiStatementRefusal,
  explainReadOnlyViolation,
  isReadOnlyViolation,
  isSyntaxError,
  looksMultiStatement,
  quoteIdent,
} from "../safety.js";
import type { SqlExecutor } from "../types.js";

export const executeSqlInput = {
  query: z
    .string()
    .min(1)
    .max(100_000)
    .describe(
      "The SQL to run. Use $1, $2 … placeholders and pass values in `params` " +
        "rather than interpolating them into the string.",
    ),
  params: z
    .array(z.union([z.string(), z.number(), z.boolean(), z.null()]))
    .default([])
    .describe("Values bound to $1, $2 … placeholders in `query`"),
  limit: z
    .number()
    .int()
    .min(1)
    .max(10_000)
    .default(DEFAULT_MAX_ROWS)
    .describe("Maximum rows to return"),
  response_format: z.enum(["markdown", "json"]).default("markdown"),
};

/**
 * Run a SQL query.
 *
 * When the server is read-only the statement runs inside
 * `BEGIN TRANSACTION READ ONLY`, so Postgres itself rejects any write. That is
 * the whole safety mechanism: no SQL parsing, no keyword blocklist, nothing
 * for a cleverly-phrased statement to slip past.
 *
 * `params` exists so values never need interpolating. An agent that builds SQL
 * by string concatenation will eventually build an injection, and the fix is to
 * make the safe path the convenient one.
 */
export async function executeSql(
  db: SqlExecutor,
  allowWrites: boolean,
  args: {
    query: string;
    params: (string | number | boolean | null)[];
    limit: number;
    response_format: "markdown" | "json";
  },
): Promise<ToolOutput> {
  const run = async (tx: SqlExecutor): Promise<ToolOutput> => {
    // A parameterised statement must go through the extended protocol, which
    // accepts exactly one command. Without params an agent may reasonably
    // paste several, so those take the simple protocol instead —
    // **but only when writes are enabled**.
    //
    // The simple query protocol honours transaction control. In read-only mode
    // the safety boundary *is* the enclosing `BEGIN TRANSACTION READ ONLY`, so
    // a script whose first statement is `COMMIT;` would end that transaction
    // and run everything after it read-write, auto-committing as it went — and
    // postgres.js' own trailing `commit` would then raise only a "no
    // transaction in progress" notice, which is swallowed, so the tool would
    // report success. Read-only therefore never takes this path; it goes
    // through the extended protocol, which accepts exactly one command.
    const looksLikeScript =
      allowWrites &&
      args.params.length === 0 &&
      /;\s*\S/.test(args.query.trim().replace(/;\s*$/, ""));

    if (looksLikeScript) {
      await tx.exec(args.query);
      return respond(
        args.response_format,
        "Script completed. Multi-statement input returns no rows — run a " +
          "single statement to see results.",
        { rows: [], row_count: 0, returned: 0, truncated: false, script: true },
      );
    }

    const result = await tx.query(args.query, args.params);
    const truncated = result.rows.length > args.limit;
    const rows = truncated ? result.rows.slice(0, args.limit) : result.rows;

    const structured = {
      rows: jsonSafe(rows),
      row_count: result.rowCount,
      returned: rows.length,
      truncated,
      read_only: !allowWrites,
      ...(result.fields ? { columns: result.fields } : {}),
    };

    const parts: string[] = [];
    if (rows.length === 0) {
      parts.push(
        result.rowCount > 0
          ? `Statement affected ${result.rowCount} row(s). No rows returned.`
          : "Statement completed. No rows returned.",
      );
    } else {
      parts.push(rowsToMarkdown(rows, result.fields));
      parts.push("", `_${rows.length} row(s) shown._`);
      if (truncated) {
        parts.push(
          `_Result truncated at limit ${args.limit}; there are more rows. ` +
            `Raise \`limit\` or add a WHERE clause._`,
        );
      }
    }
    return respond(args.response_format, parts.join("\n"), structured);
  };

  try {
    return allowWrites ? await db.transaction(run) : await db.readOnly(run);
  } catch (error) {
    if (isReadOnlyViolation(error)) {
      return {
        content: [{ type: "text", text: explainReadOnlyViolation() }],
        isError: true,
      };
    }
    // Postgres' own wording for this is "cannot insert multiple commands into
    // a prepared statement", which tells an agent nothing about what to do.
    // Purely a message improvement: the refusal itself came from the database.
    if (!allowWrites && isSyntaxError(error) && looksMultiStatement(args.query)) {
      return {
        content: [{ type: "text", text: explainMultiStatementRefusal() }],
        isError: true,
      };
    }
    return failFromError(error, "Query failed");
  }
}

export const applyMigrationInput = {
  name: z
    .string()
    .min(1)
    .max(200)
    .regex(
      /^[a-z0-9_]+$/,
      "Use lowercase letters, digits and underscores, e.g. add_synapse_region",
    )
    .describe("Migration name in snake_case, e.g. add_synapse_region"),
  sql: z
    .string()
    .min(1)
    .max(1_000_000)
    .describe(
      "The migration SQL. May contain multiple statements; all run in one " +
        "transaction and roll back together if any fails.",
    ),
};

/**
 * Apply a schema migration, recording it in a ledger.
 *
 * Two properties make this different from calling `ntl_execute_sql` with DDL:
 *
 * 1. **All-or-nothing.** Every statement runs in one transaction. A migration
 *    that half-applied would leave a schema no version number describes, which
 *    is worse than one that failed cleanly.
 * 2. **Recorded.** The ledger is what lets `ntl_list_migrations` tell an
 *    operator what state the database is actually in.
 *
 * Postgres supports transactional DDL, which is what makes (1) possible at
 * all — the same guarantee is unavailable on MySQL, and a template copied to
 * that engine must not silently assume it.
 */
export async function applyMigration(
  db: SqlExecutor,
  schema: string,
  args: { name: string; sql: string },
): Promise<ToolOutput> {
  const quoted = quoteIdent(schema);

  try {
    const result = await db.transaction(async (tx) => {
      await tx.query(`CREATE SCHEMA IF NOT EXISTS ${quoted}`);
      await tx.query(
        `CREATE TABLE IF NOT EXISTS ${quoted}.migrations (
           version         BIGINT PRIMARY KEY,
           name            TEXT NOT NULL,
           applied_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
           statement_count INTEGER NOT NULL DEFAULT 0
         )`,
      );

      // exec, not query: a migration may contain several statements, and the
      // extended protocol accepts only one.
      await tx.exec(args.sql);

      // A timestamp-derived version keeps migrations ordered without needing a
      // sequence that a parallel deploy could hand out twice.
      const version = Date.now();
      const statementCount = args.sql
        .split(";")
        .filter((s) => s.trim().length > 0).length;

      await tx.query(
        `INSERT INTO ${quoted}.migrations (version, name, statement_count)
         VALUES ($1, $2, $3)
         ON CONFLICT (version) DO UPDATE
           SET name = EXCLUDED.name, statement_count = EXCLUDED.statement_count`,
        [version, args.name, statementCount],
      );

      return { version, statementCount };
    });

    return respond(
      "markdown",
      [
        `Applied migration \`${args.name}\`.`,
        "",
        `- version: \`${result.version}\``,
        `- statements: ${result.statementCount}`,
        `- schema: \`${schema}\``,
      ].join("\n"),
      {
        applied: true,
        name: args.name,
        version: result.version,
        statement_count: result.statementCount,
        schema,
      },
    );
  } catch (error) {
    return failFromError(
      error,
      `Migration \`${args.name}\` failed and was rolled back. The database is ` +
        `unchanged`,
    );
  }
}

export const initSchemaInput = {
  confirm: z
    .boolean()
    .describe(
      "Must be true. Creates the openNTL schema and its tables. Idempotent, " +
        "so re-running is safe, but it is a write and asks explicitly.",
    ),
};

/**
 * Create the openNTL schema.
 *
 * Idempotent: every statement is `IF NOT EXISTS`, so an operator can run it
 * against a partially-created schema to finish the job.
 */
export async function initSchema(
  db: SqlExecutor,
  schema: string,
  schemaSql: string,
  args: { confirm: boolean },
): Promise<ToolOutput> {
  if (!args.confirm) {
    return {
      content: [
        {
          type: "text",
          text:
            "Not applied. Call again with `confirm: true` to create the " +
            `openNTL schema in \`${schema}\`.`,
        },
      ],
      isError: true,
    };
  }

  // The one place an identifier reaches SQL by interpolation. quoteIdent
  // validates the shape first; see safety.ts.
  const sql = schemaSql.replaceAll("{{SCHEMA}}", quoteIdent(schema));

  try {
    await db.transaction(async (tx) => {
      await tx.exec(sql);
    });

    const tables = await db.query(
      `SELECT table_name FROM information_schema.tables
        WHERE table_schema = $1 ORDER BY table_name`,
      [schema],
    );

    return respond(
      "markdown",
      [
        `openNTL schema created in \`${schema}\`.`,
        "",
        `Tables (${tables.rows.length}):`,
        "",
        ...tables.rows.map((r) => `- \`${String(r["table_name"])}\``),
      ].join("\n"),
      {
        created: true,
        schema,
        tables: tables.rows.map((r) => String(r["table_name"])),
      },
    );
  } catch (error) {
    return failFromError(
      error,
      `Creating the openNTL schema in \`${schema}\` failed and was rolled back`,
    );
  }
}
