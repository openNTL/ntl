/**
 * MCP server construction.
 *
 * Kept separate from the Worker entry point so the whole tool surface can be
 * instantiated against any {@link SqlExecutor} — which is what lets the tests
 * drive it against a real Postgres in-process rather than mocking the MCP layer.
 */

import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { z } from "zod";

import { SERVER_NAME, SERVER_VERSION } from "./constants.js";
import { failFromError } from "./format.js";
import type { ToolOutput } from "./format.js";
import NTL_SCHEMA_SQL from "./schema/ntl.sql";
import { WritesDisabledError } from "./safety.js";
import * as ntlTools from "./tools/ntl.js";
import * as opsTools from "./tools/ops.js";
import * as schemaTools from "./tools/schema.js";
import * as sqlTools from "./tools/sql.js";
import type { ServerConfig, SqlExecutor } from "./types.js";

/**
 * Wrap a handler so a thrown error becomes an actionable tool result.
 *
 * MCP tools should return errors as content rather than throwing, so the agent
 * can read and act on them. Doing it once here means no individual tool can
 * forget.
 */
function guard<A>(
  name: string,
  handler: (args: A) => Promise<ToolOutput> | ToolOutput,
): (args: A) => Promise<ToolOutput> {
  return async (args: A) => {
    try {
      return await handler(args);
    } catch (error) {
      if (error instanceof WritesDisabledError) {
        return { content: [{ type: "text", text: error.message }], isError: true };
      }
      return failFromError(error, `${name} failed`);
    }
  };
}

/**
 * Build the MCP server over a SQL executor.
 *
 * Write tools are registered only when `config.allowWrites` is true. They are
 * omitted rather than registered-and-refusing, so an agent's tool list reflects
 * what it can actually do — offering a tool that always fails wastes a turn and
 * teaches the agent nothing.
 */
export function createServer(db: SqlExecutor, config: ServerConfig): McpServer {
  const server = new McpServer({
    name: SERVER_NAME,
    version: SERVER_VERSION,
  });

  const readOnlyAnnotations = {
    readOnlyHint: true,
    destructiveHint: false,
    idempotentHint: true,
    openWorldHint: false,
  } as const;

  // ---------------------------------------------------------------- schema

  server.registerTool(
    "ntl_list_tables",
    {
      title: "List tables",
      description:
        "List tables, views and materialized views with row estimates, size " +
        "and optionally columns. Row counts are planner estimates, not exact " +
        "counts, so this is safe to run on a large database.",
      inputSchema: schemaTools.listTablesInput,
      annotations: readOnlyAnnotations,
    },
    guard("ntl_list_tables", (args) =>
      db.readOnly((tx) => schemaTools.listTables(tx, args)),
    ),
  );

  server.registerTool(
    "ntl_list_extensions",
    {
      title: "List extensions",
      description:
        "List installed Postgres extensions and their versions, plus which " +
        "extensions are available but not yet installed.",
      inputSchema: schemaTools.listExtensionsInput,
      annotations: readOnlyAnnotations,
    },
    guard("ntl_list_extensions", (args) =>
      db.readOnly((tx) => schemaTools.listExtensions(tx, args)),
    ),
  );

  server.registerTool(
    "ntl_list_migrations",
    {
      title: "List migrations",
      description:
        "List migrations applied through this server, newest first. A missing " +
        "ledger means none have been applied, which is not an error.",
      inputSchema: schemaTools.listMigrationsInput,
      annotations: readOnlyAnnotations,
    },
    guard("ntl_list_migrations", (args) =>
      db.readOnly((tx) => schemaTools.listMigrations(tx, config.schema, args)),
    ),
  );

  server.registerTool(
    "ntl_generate_typescript_types",
    {
      title: "Generate TypeScript types",
      description:
        "Generate TypeScript interfaces from the live schema. bigint and " +
        "numeric map to string, not number, because Postgres BIGINT exceeds " +
        "Number.MAX_SAFE_INTEGER and NTL stores nanosecond timestamps in it.",
      inputSchema: schemaTools.generateTypesInput,
      annotations: readOnlyAnnotations,
    },
    guard("ntl_generate_typescript_types", (args) =>
      db.readOnly((tx) => schemaTools.generateTypescriptTypes(tx, args)),
    ),
  );

  // ------------------------------------------------------------------- sql

  server.registerTool(
    "ntl_execute_sql",
    {
      title: "Execute SQL",
      description:
        (config.allowWrites
          ? "Run SQL against the database. Writes are ENABLED on this server. "
          : "Run SQL against the database. This server is READ-ONLY: the " +
            "statement runs inside a read-only transaction and Postgres will " +
            "reject any write. ") +
        "Pass values in `params` bound to $1, $2 … rather than interpolating " +
        "them into the query string.",
      inputSchema: sqlTools.executeSqlInput,
      annotations: {
        readOnlyHint: !config.allowWrites,
        destructiveHint: config.allowWrites,
        idempotentHint: false,
        openWorldHint: false,
      },
    },
    guard("ntl_execute_sql", (args) =>
      sqlTools.executeSql(db, config.allowWrites, args),
    ),
  );

  // ---------------------------------------------------------------- domain

  server.registerTool(
    "ntl_list_synapses",
    {
      title: "List synapses",
      description:
        "List an openNTL node's synapses with their learned weights, per-type " +
        "affinity and lifecycle state. Shows the decayed weight alongside the " +
        "stored one, because decay is applied lazily and the stored value can " +
        "be stale.",
      inputSchema: ntlTools.listSynapsesInput,
      annotations: readOnlyAnnotations,
    },
    guard("ntl_list_synapses", (args) =>
      db.readOnly((tx) => ntlTools.listSynapses(tx, config.schema, args)),
    ),
  );

  server.registerTool(
    "ntl_get_learning_health",
    {
      title: "Get learning health",
      description:
        "Report whether the routing model is actually learning: the fraction " +
        "of recent decisions that were exploratory, the fraction still awaiting " +
        "a receipt, and the outcome breakdown. Exploration at zero means the " +
        "node has stopped learning; pending near 100% means its weights reflect " +
        "nothing.",
      inputSchema: ntlTools.learningHealthInput,
      annotations: readOnlyAnnotations,
    },
    guard("ntl_get_learning_health", (args) =>
      db.readOnly((tx) => ntlTools.learningHealth(tx, config.schema, args)),
    ),
  );

  server.registerTool(
    "ntl_list_journal",
    {
      title: "List routing decisions",
      description:
        "List entries from the learning journal — each a routing decision and " +
        "its observed outcome. This is the model's training data. Filter by " +
        "outcome, signal type, or exploratory decisions only.",
      inputSchema: ntlTools.listJournalInput,
      annotations: readOnlyAnnotations,
    },
    guard("ntl_list_journal", (args) =>
      db.readOnly((tx) => ntlTools.listJournal(tx, config.schema, args)),
    ),
  );

  server.registerTool(
    "ntl_get_node_status",
    {
      title: "Get node status",
      description:
        "Summarise an openNTL node's persisted state: identity, schema version, " +
        "synapse counts and total outbound weight, peers by provenance, the " +
        "activation snapshot, and live deduplication entries.",
      inputSchema: ntlTools.nodeStatusInput,
      annotations: readOnlyAnnotations,
    },
    guard("ntl_get_node_status", (args) =>
      db.readOnly((tx) => ntlTools.nodeStatus(tx, config.schema, args)),
    ),
  );

  // -------------------------------------------------------------------- ops

  server.registerTool(
    "ntl_get_advisors",
    {
      title: "Get advisors",
      description:
        "Lint the live database for security and performance problems — public " +
        "grants, SECURITY DEFINER functions without a pinned search_path, " +
        "unindexed foreign keys, missing primary keys, unused indexes and " +
        "bloat. Each finding carries a remediation.",
      inputSchema: opsTools.getAdvisorsInput,
      annotations: readOnlyAnnotations,
    },
    guard("ntl_get_advisors", (args) =>
      db.readOnly((tx) => opsTools.getAdvisors(tx, args)),
    ),
  );

  server.registerTool(
    "ntl_get_activity",
    {
      title: "Get database activity",
      description:
        "Show current connections and, when pg_stat_statements is installed, " +
        "the slowest statements by total time. Postgres log files are not " +
        "reachable over SQL, so this reports live session state rather than logs.",
      inputSchema: opsTools.getActivityInput,
      annotations: readOnlyAnnotations,
    },
    guard("ntl_get_activity", (args) =>
      db.readOnly((tx) => opsTools.getActivity(tx, args)),
    ),
  );

  server.registerTool(
    "ntl_search_docs",
    {
      title: "Search openNTL docs",
      description:
        "Search openNTL specification and guide documentation by keyword. The " +
        "index is embedded, so this works without network access.",
      inputSchema: opsTools.searchDocsInput,
      annotations: { ...readOnlyAnnotations, openWorldHint: false },
    },
    guard("ntl_search_docs", (args) => opsTools.searchDocs(args)),
  );

  // ----------------------------------------------------------------- write

  if (config.allowWrites) {
    server.registerTool(
      "ntl_apply_migration",
      {
        title: "Apply migration",
        description:
          "Apply a schema migration in a single transaction and record it in " +
          "the migration ledger. All statements roll back together if any " +
          "fails, so the schema is never left in a state no version describes. " +
          "Prefer this over ntl_execute_sql for DDL, so the change is recorded.",
        inputSchema: sqlTools.applyMigrationInput,
        annotations: {
          readOnlyHint: false,
          destructiveHint: true,
          idempotentHint: false,
          openWorldHint: false,
        },
      },
      guard("ntl_apply_migration", (args) =>
        sqlTools.applyMigration(db, config.schema, args),
      ),
    );

    server.registerTool(
      "ntl_init_schema",
      {
        title: "Initialize openNTL schema",
        description:
          "Create the openNTL schema and its tables — synapses, peers, " +
          "seen_signals, activation, journal, influence, meta and " +
          "signal_history. Idempotent, so it is safe to re-run against a " +
          "partially created schema.",
        inputSchema: sqlTools.initSchemaInput,
        annotations: {
          readOnlyHint: false,
          destructiveHint: false,
          idempotentHint: true,
          openWorldHint: false,
        },
      },
      guard("ntl_init_schema", (args) =>
        sqlTools.initSchema(db, config.schema, NTL_SCHEMA_SQL, args),
      ),
    );
  }

  // -------------------------------------------------------------- resources

  // Exposed as a resource as well as through a tool, because an agent about to
  // write a migration benefits from reading the reference schema directly.
  server.registerResource(
    "ntl-schema",
    "ntl://schema/postgres",
    {
      title: "openNTL PostgreSQL schema",
      description:
        "The reference DDL for a Postgres-backed openNTL node, with the " +
        "reasoning behind each Postgres-specific choice.",
      mimeType: "application/sql",
    },
    () => ({
      contents: [
        {
          uri: "ntl://schema/postgres",
          mimeType: "application/sql",
          text: NTL_SCHEMA_SQL.replaceAll("{{SCHEMA}}", `"${config.schema}"`),
        },
      ],
    }),
  );

  return server;
}

/** The Zod schema for validating resolved configuration. */
export const serverConfigSchema = z.object({
  databaseUrl: z.string().min(1),
  allowWrites: z.boolean(),
  schema: z.string().min(1).max(63),
  statementTimeoutMs: z.number().int().positive(),
  maxRows: z.number().int().positive(),
});
