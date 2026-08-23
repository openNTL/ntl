/**
 * Shared types.
 *
 * The `SqlExecutor` abstraction is the most important design choice in this
 * template. Every tool is written against a four-method interface, which means
 * the whole server can be tested against a real Postgres running in-process
 * while production talks to Hyperdrive. A test backend that diverges from
 * production is worse than none, so the interface is kept deliberately small.
 */

/** One row, keyed by column name. */
export type Row = Record<string, unknown>;

/** The result of a statement. */
export interface QueryResult {
  rows: Row[];
  /** Rows affected, for statements that report it. */
  rowCount: number;
  /** Column names in selection order, when the driver exposes them. */
  fields?: string[];
}

/** Minimal SQL surface every backend must provide. */
export interface SqlExecutor {
  /** Run a single parameterised statement. */
  query(sql: string, params?: unknown[]): Promise<QueryResult>;

  /**
   * Run a script that may contain several statements.
   *
   * Separate from {@link query} because a parameterised statement goes through
   * the extended protocol, which accepts exactly one command — Postgres
   * rejects `INSERT …; INSERT …` there with SQLSTATE 42601. Scripts therefore
   * use the simple protocol, which is also why they cannot take parameters:
   * anything variable must be a separate `query` call.
   */
  exec(script: string): Promise<void>;

  /**
   * Run `fn` inside a genuinely read-only transaction.
   *
   * Implementations MUST use the database's own read-only enforcement
   * (`BEGIN TRANSACTION READ ONLY`), not SQL inspection. See `safety.ts`.
   */
  readOnly<T>(fn: (tx: SqlExecutor) => Promise<T>): Promise<T>;

  /** Run `fn` inside a read-write transaction, rolling back on error. */
  transaction<T>(fn: (tx: SqlExecutor) => Promise<T>): Promise<T>;

  /** Release resources. */
  close(): Promise<void>;
}

/** How a tool should render its result. */
export type ResponseFormat = "markdown" | "json";

/** Server configuration, resolved from the environment. */
export interface ServerConfig {
  /** Postgres connection string. */
  databaseUrl: string;
  /**
   * Whether write tools are permitted.
   *
   * Defaults to `false`. An MCP server holding database credentials should not
   * be able to mutate anything unless an operator said so explicitly.
   */
  allowWrites: boolean;
  /** Schema NTL tables live in. */
  schema: string;
  /** Statement timeout in milliseconds, applied per query. */
  statementTimeoutMs: number;
  /** Cap on rows any single tool will return. */
  maxRows: number;
}

/** Cloudflare Worker bindings. */
export interface Env {
  /** Hyperdrive binding, which supplies a pooled connection string. */
  HYPERDRIVE?: { connectionString: string };
  /** Direct connection string, for local development without Hyperdrive. */
  DATABASE_URL?: string;
  /** Shared secret required in the Authorization header. */
  MCP_AUTH_TOKEN?: string;
  /** Set to "true" to enable write tools. */
  ALLOW_WRITES?: string;
  /** Schema for NTL tables. Defaults to "ntl". */
  NTL_SCHEMA?: string;
}
