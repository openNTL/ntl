/** Shared constants. */

/** Server identity reported in the MCP handshake. */
export const SERVER_NAME = "ntl-postgres-mcp-server";
export const SERVER_VERSION = "0.2.0-beta.1";

/**
 * Cap on characters returned by any single tool.
 *
 * An MCP tool that returns a million-row table does not help an agent; it
 * exhausts the context it needs to reason. Truncation is reported explicitly
 * so the agent knows to narrow its query rather than silently believing it saw
 * everything.
 */
export const CHARACTER_LIMIT = 25_000;

/** Default and maximum page sizes for listing tools. */
export const DEFAULT_LIMIT = 50;
export const MAX_LIMIT = 500;

/** Default per-statement timeout. */
export const DEFAULT_STATEMENT_TIMEOUT_MS = 15_000;

/** Default cap on rows returned by a single query. */
export const DEFAULT_MAX_ROWS = 1_000;

/** Schemas excluded from introspection by default. */
export const SYSTEM_SCHEMAS = [
  "pg_catalog",
  "information_schema",
  "pg_toast",
] as const;
