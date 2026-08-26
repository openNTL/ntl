/**
 * Schema introspection tools.
 *
 * These mirror the Supabase MCP's shape (`list_tables`, `list_extensions`,
 * `list_migrations`, `generate_typescript_types`) so an agent that knows one
 * knows the other.
 */

import { z } from "zod";

import { DEFAULT_LIMIT, MAX_LIMIT, SYSTEM_SCHEMAS } from "../constants.js";
import { jsonSafe, respond, rowsToMarkdown } from "../format.js";
import type { ToolOutput } from "../format.js";
import { quoteIdent } from "../safety.js";
import type { Row, SqlExecutor } from "../types.js";

const formatArg = z
  .enum(["markdown", "json"])
  .default("markdown")
  .describe("Output format: 'markdown' to read, 'json' to process");

export const listTablesInput = {
  schemas: z
    .array(z.string())
    .optional()
    .describe(
      "Schemas to inspect. Omit for every non-system schema. Example: ['ntl', 'public']",
    ),
  include_columns: z
    .boolean()
    .default(true)
    .describe("Include column definitions. Set false for a compact overview."),
  response_format: formatArg,
};

/**
 * List tables, with row estimates and optionally columns.
 *
 * Row counts come from `pg_class.reltuples`, the planner's estimate, rather
 * than `COUNT(*)`. An exact count means a sequential scan of every table,
 * which on a large database turns an overview into an outage.
 */
export async function listTables(
  db: SqlExecutor,
  args: {
    schemas?: string[];
    include_columns: boolean;
    response_format: "markdown" | "json";
  },
): Promise<ToolOutput> {
  const schemaFilter = args.schemas ?? null;

  const tables = await db.query(
    `SELECT n.nspname                         AS schema,
            c.relname                         AS name,
            CASE c.relkind WHEN 'r' THEN 'table'
                           WHEN 'v' THEN 'view'
                           WHEN 'm' THEN 'materialized view'
                           WHEN 'p' THEN 'partitioned table'
                           WHEN 'f' THEN 'foreign table'
                           ELSE c.relkind::text END AS kind,
            GREATEST(c.reltuples, 0)::bigint  AS estimated_rows,
            pg_size_pretty(pg_total_relation_size(c.oid)) AS total_size,
            obj_description(c.oid, 'pg_class') AS comment
       FROM pg_class c
       JOIN pg_namespace n ON n.oid = c.relnamespace
      WHERE c.relkind IN ('r','v','m','p','f')
        AND NOT (n.nspname = ANY($1::text[]))
        AND ($2::text[] IS NULL OR n.nspname = ANY($2::text[]))
      ORDER BY n.nspname, c.relname`,
    [SYSTEM_SCHEMAS, schemaFilter],
  );

  let columns: Row[] = [];
  if (args.include_columns && tables.rows.length > 0) {
    const result = await db.query(
      `SELECT c.table_schema AS schema,
              c.table_name   AS "table",
              c.column_name  AS name,
              c.data_type    AS type,
              c.is_nullable = 'YES' AS nullable,
              c.column_default AS "default"
         FROM information_schema.columns c
        WHERE NOT (c.table_schema = ANY($1::text[]))
          AND ($2::text[] IS NULL OR c.table_schema = ANY($2::text[]))
        ORDER BY c.table_schema, c.table_name, c.ordinal_position`,
      [SYSTEM_SCHEMAS, schemaFilter],
    );
    columns = result.rows;
  }

  const structured = {
    tables: jsonSafe(tables.rows),
    columns: jsonSafe(columns),
    table_count: tables.rows.length,
  };

  if (tables.rows.length === 0) {
    const scope = schemaFilter ? ` in ${schemaFilter.join(", ")}` : "";
    return respond(
      args.response_format,
      `No tables found${scope}.\n\nIf this is a fresh database, run ` +
        `\`ntl_init_schema\` to create the openNTL schema.`,
      structured,
    );
  }

  const lines = [`## Tables (${tables.rows.length})`, "", rowsToMarkdown(tables.rows)];

  if (args.include_columns) {
    const byTable = new Map<string, Row[]>();
    for (const col of columns) {
      const key = `${String(col["schema"])}.${String(col["table"])}`;
      const list = byTable.get(key);
      if (list) list.push(col);
      else byTable.set(key, [col]);
    }
    lines.push("", "## Columns");
    for (const [table, cols] of byTable) {
      lines.push("", `### ${table}`, "");
      lines.push(
        rowsToMarkdown(
          cols.map((c) => ({
            name: c["name"],
            type: c["type"],
            nullable: c["nullable"],
            default: c["default"],
          })),
        ),
      );
    }
  }

  return respond(args.response_format, lines.join("\n"), structured);
}

export const listExtensionsInput = {
  response_format: formatArg,
};

/** List installed extensions, and which are available but not installed. */
export async function listExtensions(
  db: SqlExecutor,
  args: { response_format: "markdown" | "json" },
): Promise<ToolOutput> {
  const installed = await db.query(
    `SELECT e.extname AS name,
            e.extversion AS version,
            n.nspname AS schema
       FROM pg_extension e
       JOIN pg_namespace n ON n.oid = e.extnamespace
      ORDER BY e.extname`,
  );

  const available = await db.query(
    `SELECT name, default_version AS available_version, comment
       FROM pg_available_extensions
      WHERE installed_version IS NULL
      ORDER BY name
      LIMIT 100`,
  );

  const structured = {
    installed: jsonSafe(installed.rows),
    available: jsonSafe(available.rows),
  };

  const markdown = [
    `## Installed extensions (${installed.rows.length})`,
    "",
    rowsToMarkdown(installed.rows),
    "",
    `## Available, not installed (${available.rows.length})`,
    "",
    rowsToMarkdown(available.rows),
  ].join("\n");

  return respond(args.response_format, markdown, structured);
}

export const listMigrationsInput = {
  limit: z
    .number()
    .int()
    .min(1)
    .max(MAX_LIMIT)
    .default(DEFAULT_LIMIT)
    .describe("Maximum migrations to return"),
  response_format: formatArg,
};

/**
 * List applied migrations.
 *
 * Reads the `ntl.migrations` ledger that `ntl_apply_migration` writes. A
 * missing ledger is reported as "none applied" rather than as an error,
 * because a fresh database legitimately has none.
 */
export async function listMigrations(
  db: SqlExecutor,
  schema: string,
  args: { limit: number; response_format: "markdown" | "json" },
): Promise<ToolOutput> {
  const exists = await db.query(
    `SELECT 1 FROM information_schema.tables
      WHERE table_schema = $1 AND table_name = 'migrations'`,
    [schema],
  );

  if (exists.rows.length === 0) {
    return respond(
      args.response_format,
      `No migration ledger in schema \`${schema}\`. None have been applied ` +
        `through this server.\n\nApplying one with \`ntl_apply_migration\` ` +
        `creates the ledger.`,
      { migrations: [], count: 0, ledger_exists: false },
    );
  }

  const rows = await db.query(
    `SELECT version, name, applied_at, statement_count
       FROM ${quoteIdent(schema)}.migrations
      ORDER BY applied_at DESC, version DESC
      LIMIT $1`,
    [args.limit],
  );

  return respond(
    args.response_format,
    [`## Applied migrations (${rows.rows.length})`, "", rowsToMarkdown(rows.rows)].join("\n"),
    { migrations: jsonSafe(rows.rows), count: rows.rows.length, ledger_exists: true },
  );
}

export const generateTypesInput = {
  schemas: z
    .array(z.string())
    .default(["ntl"])
    .describe("Schemas to generate types for"),
};

/**
 * Generate TypeScript interfaces for the current schema.
 *
 * The mapping is deliberately conservative. `BIGINT` becomes `string`, not
 * `number`: Postgres BIGINT exceeds `Number.MAX_SAFE_INTEGER`, and NTL stores
 * nanosecond timestamps in it, so `number` would silently lose precision on
 * exactly the values that matter.
 */
export async function generateTypescriptTypes(
  db: SqlExecutor,
  args: { schemas: string[] },
): Promise<ToolOutput> {
  const rows = await db.query(
    `SELECT c.table_schema AS schema,
            c.table_name   AS "table",
            c.column_name  AS name,
            c.data_type    AS type,
            c.udt_name     AS udt,
            c.is_nullable = 'YES' AS nullable
       FROM information_schema.columns c
       JOIN information_schema.tables t
         ON t.table_schema = c.table_schema AND t.table_name = c.table_name
      WHERE c.table_schema = ANY($1::text[])
        AND t.table_type IN ('BASE TABLE', 'VIEW')
      ORDER BY c.table_schema, c.table_name, c.ordinal_position`,
    [args.schemas],
  );

  if (rows.rows.length === 0) {
    return respond(
      "markdown",
      `No tables found in ${args.schemas.join(", ")}. Nothing to generate.`,
      { typescript: "", table_count: 0 },
    );
  }

  const tsType = (pgType: string, udt: string): string => {
    switch (pgType) {
      case "smallint":
      case "integer":
      case "real":
      case "double precision":
        return "number";
      case "bigint":
      case "numeric":
        // Exceeds Number.MAX_SAFE_INTEGER; a string keeps it exact.
        return "string";
      case "boolean":
        return "boolean";
      case "json":
      case "jsonb":
        return "unknown";
      case "bytea":
        return "Uint8Array";
      case "timestamp with time zone":
      case "timestamp without time zone":
      case "date":
        return "string";
      case "ARRAY":
        return `${tsType(udt.replace(/^_/, ""), udt)}[]`;
      default:
        return "string";
    }
  };

  const byTable = new Map<string, Row[]>();
  for (const row of rows.rows) {
    const key = `${String(row["schema"])}.${String(row["table"])}`;
    const list = byTable.get(key);
    if (list) list.push(row);
    else byTable.set(key, [row]);
  }

  const pascal = (name: string): string =>
    name
      .split(/[_\s]+/)
      .map((p) => p.charAt(0).toUpperCase() + p.slice(1))
      .join("");

  const out: string[] = [
    "// Generated by ntl-postgres-mcp-server. Do not edit by hand.",
    "//",
    "// bigint and numeric map to `string`, not `number`: Postgres BIGINT",
    "// exceeds Number.MAX_SAFE_INTEGER, and NTL stores nanosecond timestamps",
    "// in it. `number` would silently lose precision on exactly those values.",
    "",
  ];
  for (const [table, cols] of byTable) {
    const parts = table.split(".");
    const name = pascal(parts[1] ?? table);
    out.push(`export interface ${name} {`);
    for (const col of cols) {
      const optional = col["nullable"] === true ? " | null" : "";
      out.push(
        `  ${String(col["name"])}: ${tsType(String(col["type"]), String(col["udt"]))}${optional};`,
      );
    }
    out.push("}", "");
  }

  const typescript = out.join("\n");
  return respond(
    "markdown",
    ["```typescript", typescript, "```"].join("\n"),
    { typescript, table_count: byTable.size },
  );
}
