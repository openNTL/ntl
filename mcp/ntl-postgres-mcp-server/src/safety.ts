/**
 * Read-only enforcement.
 *
 * ## Why this file does not parse SQL
 *
 * The obvious way to make a database MCP server safe is to inspect the SQL and
 * reject anything that looks like a write. Every implementation that does this
 * is bypassable, because SQL is not a regular language and Postgres is
 * generous:
 *
 * ```sql
 * -- all of these write, none contain a leading INSERT/UPDATE/DELETE
 * SELECT nextval('s');                       -- side effect via sequence
 * WITH x AS (DELETE FROM t RETURNING *) SELECT * FROM x;
 * SELECT my_function_that_writes();          -- writes inside a function
 * SELECT dblink_exec('...', 'DROP TABLE t'); -- writes via extension
 * COPY t FROM PROGRAM 'curl ...';            -- writes, and worse
 * ```
 *
 * A blocklist has to anticipate all of it. Postgres already knows which
 * statements write, so this module delegates: a read-only tool runs inside
 * `BEGIN TRANSACTION READ ONLY`, and the *database* rejects the write with
 * error 25006. That is not a heuristic, and it cannot be talked around by
 * clever phrasing.
 *
 * The identifier quoting below is a separate concern: values are always bound
 * as parameters, but identifiers (schema and table names) cannot be, so those
 * are validated and quoted.
 */

/** Postgres error code for a write attempted in a read-only transaction. */
export const READ_ONLY_SQL_STATE = "25006";

/**
 * Whether a Postgres error is the read-only transaction rejection.
 *
 * Matched on SQLSTATE, not message text, so it survives locale and version
 * changes.
 */
export function isReadOnlyViolation(error: unknown): boolean {
  if (typeof error !== "object" || error === null) return false;
  const code = (error as { code?: unknown }).code;
  return code === READ_ONLY_SQL_STATE;
}

/**
 * Rewrite a read-only violation into a message an agent can act on.
 *
 * A bare "cannot execute INSERT in a read-only transaction" leaves the agent
 * guessing whether the statement was wrong or the server was configured to
 * refuse it. This says which.
 */
export function explainReadOnlyViolation(): string {
  return [
    "This statement writes, and this server is running read-only.",
    "",
    "Either use a read-only statement, or ask the operator to enable writes",
    "by setting ALLOW_WRITES=true. Schema changes should go through",
    "ntl_apply_migration rather than ntl_execute_sql, so they are recorded.",
  ].join("\n");
}

/** Thrown when a tool is called that this deployment does not permit. */
export class WritesDisabledError extends Error {
  constructor(toolName: string) {
    super(
      `${toolName} modifies the database, and this server is running ` +
        `read-only. Set ALLOW_WRITES=true to enable write tools. This is off ` +
        `by default because an MCP server holding database credentials should ` +
        `not be able to mutate anything unless an operator said so.`,
    );
    this.name = "WritesDisabledError";
  }
}

/**
 * Validate and quote a Postgres identifier.
 *
 * Identifiers cannot be bound as parameters, so any identifier reaching SQL
 * must be validated here. The allowed shape is deliberately narrow — letters,
 * digits, underscore, dollar, not starting with a digit — which covers every
 * unquoted identifier Postgres accepts and nothing else.
 *
 * @throws if the identifier is empty, too long, or contains anything else.
 */
export function quoteIdent(name: string): string {
  if (name.length === 0) {
    throw new Error("identifier must not be empty");
  }
  if (name.length > 63) {
    // Postgres truncates at 63 bytes; silently accepting a longer name would
    // mean operating on a different object than the caller asked for.
    throw new Error(
      `identifier ${JSON.stringify(name)} exceeds Postgres' 63-character limit`,
    );
  }
  if (!/^[A-Za-z_][A-Za-z0-9_$]*$/.test(name)) {
    throw new Error(
      `identifier ${JSON.stringify(name)} is not a valid Postgres identifier. ` +
        `Expected letters, digits, underscore or dollar, not starting with a digit.`,
    );
  }
  return `"${name}"`;
}

/**
 * Quote a possibly schema-qualified relation name.
 *
 * Accepts `table` or `schema.table`. Anything else is rejected rather than
 * guessed at.
 */
export function quoteRelation(relation: string): string {
  const parts = relation.split(".");
  if (parts.length === 1 && parts[0] !== undefined) {
    return quoteIdent(parts[0]);
  }
  if (parts.length === 2 && parts[0] !== undefined && parts[1] !== undefined) {
    return `${quoteIdent(parts[0])}.${quoteIdent(parts[1])}`;
  }
  throw new Error(
    `${JSON.stringify(relation)} is not a valid relation name. ` +
      `Expected "table" or "schema.table".`,
  );
}
