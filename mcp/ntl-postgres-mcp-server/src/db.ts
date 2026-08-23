/**
 * SQL executors.
 *
 * Two implementations of {@link SqlExecutor}:
 *
 * - {@link PostgresExecutor} — production, over `postgres.js`. On Cloudflare
 *   Workers the connection string comes from a Hyperdrive binding, which pools
 *   connections outside the isolate. Without pooling, every Worker invocation
 *   would open its own Postgres connection and a burst of traffic would
 *   exhaust `max_connections` long before it exhausted anything else.
 *
 * - {@link PgliteExecutor} — tests, over `@electric-sql/pglite`. This is real
 *   Postgres compiled to WebAssembly, not a mock, so the SQL under test is the
 *   SQL that will run in production.
 *
 * Both go through the same interface, and the conformance tests run against
 * both plus a real Postgres server.
 */

import postgres from "postgres";

import type { QueryResult, SqlExecutor } from "./types.js";

/** Options shared by executors. */
export interface ExecutorOptions {
  /** Per-statement timeout, in milliseconds. */
  statementTimeoutMs: number;
}

/**
 * Production executor over `postgres.js`.
 */
export class PostgresExecutor implements SqlExecutor {
  private readonly sql: postgres.Sql;
  private readonly options: ExecutorOptions;
  /** True when this instance wraps a transaction and must not be closed. */
  private readonly borrowed: boolean;

  constructor(
    connectionString: string,
    options: ExecutorOptions,
    existing?: postgres.Sql,
  ) {
    this.options = options;
    this.borrowed = existing !== undefined;
    this.sql =
      existing ??
      postgres(connectionString, {
        // One connection per isolate: Hyperdrive does the pooling, and a
        // Worker isolate is short-lived enough that a local pool would mostly
        // hold idle connections open.
        max: 1,
        // Prepared statements are cached per connection, which does not survive
        // a pooler that may hand out a different backend each time.
        prepare: false,
        idle_timeout: 20,
        connect_timeout: 10,
        // Errors carry SQLSTATE, which safety.ts matches on.
        onnotice: () => {},
      });
  }

  async query(sql: string, params: unknown[] = []): Promise<QueryResult> {
    const rows = await this.sql.unsafe(sql, params as never[]);
    return {
      rows: rows as unknown as QueryResult["rows"],
      rowCount: rows.count ?? rows.length,
      ...(rows.columns
        ? { fields: rows.columns.map((c: { name: string }) => c.name) }
        : {}),
    };
  }

  async exec(script: string): Promise<void> {
    // No params, so postgres.js uses the simple query protocol, which accepts
    // multiple statements.
    await this.sql.unsafe(script);
  }

  async readOnly<T>(fn: (tx: SqlExecutor) => Promise<T>): Promise<T> {
    return this.sql.begin(async (tx) => {
      // The database enforces read-only, not this process. See safety.ts.
      await tx.unsafe("SET TRANSACTION READ ONLY");
      await tx.unsafe(
        `SET LOCAL statement_timeout = ${this.options.statementTimeoutMs}`,
      );
      return fn(new PostgresExecutor("", this.options, tx as unknown as postgres.Sql));
    }) as Promise<T>;
  }

  async transaction<T>(fn: (tx: SqlExecutor) => Promise<T>): Promise<T> {
    return this.sql.begin(async (tx) => {
      await tx.unsafe(
        `SET LOCAL statement_timeout = ${this.options.statementTimeoutMs}`,
      );
      return fn(new PostgresExecutor("", this.options, tx as unknown as postgres.Sql));
    }) as Promise<T>;
  }

  async close(): Promise<void> {
    // A borrowed handle belongs to the enclosing transaction; closing it here
    // would tear down the caller's connection mid-flight.
    if (this.borrowed) return;
    await this.sql.end({ timeout: 5 });
  }
}

/**
 * Postgres type OID for `int8` (BIGINT).
 *
 * Both backends must agree on how this is represented, or a tool's structured
 * output changes type depending on which driver served it — a real bug we hit
 * in testing, where `signals_fired` came back as `137` from PGlite and `"137"`
 * from postgres.js.
 *
 * A string is the correct target: BIGINT exceeds `Number.MAX_SAFE_INTEGER`, and
 * NTL stores nanosecond timestamps in it, so a JS number would silently lose
 * precision on exactly the values that matter. postgres.js already does this;
 * PGlite is configured to match. It is also what
 * `ntl_generate_typescript_types` promises.
 */
export const INT8_OID = 20;

/** The subset of PGlite this module needs, so tests need no type gymnastics. */
export interface PgliteLike {
  query(
    sql: string,
    params?: unknown[],
  ): Promise<{ rows: unknown[]; affectedRows?: number; fields?: { name: string }[] }>;
  exec(sql: string): Promise<unknown>;
  close(): Promise<void>;
}

/**
 * Test executor over PGlite — real Postgres, in-process.
 */
export class PgliteExecutor implements SqlExecutor {
  private readonly db: PgliteLike;
  private readonly options: ExecutorOptions;
  private readonly borrowed: boolean;

  constructor(db: PgliteLike, options: ExecutorOptions, borrowed = false) {
    this.db = db;
    this.options = options;
    this.borrowed = borrowed;
  }

  async query(sql: string, params: unknown[] = []): Promise<QueryResult> {
    const result = await this.db.query(sql, params);
    return {
      rows: result.rows as QueryResult["rows"],
      rowCount: result.affectedRows ?? result.rows.length,
      ...(result.fields ? { fields: result.fields.map((f) => f.name) } : {}),
    };
  }

  async exec(script: string): Promise<void> {
    await this.db.exec(script);
  }

  async readOnly<T>(fn: (tx: SqlExecutor) => Promise<T>): Promise<T> {
    await this.db.exec("BEGIN TRANSACTION READ ONLY");
    try {
      const out = await fn(new PgliteExecutor(this.db, this.options, true));
      await this.db.exec("COMMIT");
      return out;
    } catch (error) {
      // Best-effort rollback: if the transaction is already aborted the
      // rollback is what clears it, and if the connection is gone there is
      // nothing to clear. Either way the original error is what matters.
      await this.db.exec("ROLLBACK").catch(() => {});
      throw error;
    }
  }

  async transaction<T>(fn: (tx: SqlExecutor) => Promise<T>): Promise<T> {
    await this.db.exec("BEGIN");
    try {
      const out = await fn(new PgliteExecutor(this.db, this.options, true));
      await this.db.exec("COMMIT");
      return out;
    } catch (error) {
      await this.db.exec("ROLLBACK").catch(() => {});
      throw error;
    }
  }

  async close(): Promise<void> {
    if (this.borrowed) return;
    await this.db.close();
  }
}
