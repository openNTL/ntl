# ntl-postgres-mcp-server

An MCP server for a PostgreSQL-backed [openNTL](https://openntl.org) node,
running on Cloudflare Workers.

It is also **a template**. If you want a Postgres MCP server for your own
schema, copy this directory and replace the domain tools — the auth, read-only
enforcement, formatting, error handling and test harness are the parts worth
keeping, and they are the parts that take longest to get right.

Modelled on the shape of the Supabase MCP server, so an agent that knows one
knows this one.

```
npm install
npm test                  # 167 tests, real Postgres, no mocks
npx wrangler deploy
```

## Why you might copy this

Most database MCP servers get four things wrong. This one is built around
avoiding them, and each has tests that would fail if it regressed.

**1. Read-only enforced by the database, not by parsing SQL.**

The obvious approach is to inspect the query and reject anything that looks
like a write. Every implementation that does this is bypassable:

```sql
WITH x AS (DELETE FROM t RETURNING *) SELECT * FROM x;   -- no leading DELETE
SELECT my_function_that_writes();                        -- writes in a function
/* SELECT */ INSERT INTO t VALUES (1);                   -- comment-prefixed
SELECT * INTO new_table FROM t;                          -- SELECT that creates
```

A blocklist has to anticipate all of it. Postgres already knows which
statements write, so read-only tools run inside `BEGIN TRANSACTION READ ONLY`
and the *database* rejects the write with SQLSTATE 25006. There is nothing for
a cleverly-phrased statement to slip past.

There is exactly one way out of a transaction, and it is not clever phrasing —
it is ending the transaction:

```sql
COMMIT; DROP TABLE ntl.synapses;
```

That works only over the *simple* query protocol, which accepts several
commands in one string and honours transaction control. So the boundary is the
protocol, not a check on the string: read-only queries are pinned to the
extended protocol, which accepts exactly one command, and Postgres rejects the
smuggled statement with SQLSTATE 42601 before it runs. The simple-protocol path
throws if it is ever reached inside a read-only transaction, and is only
routed to when the operator has enabled writes.

This is worth dwelling on if you copy the file, because an earlier version of
it had the hole: `COMMIT; DROP TABLE canary` returned *success* on both
drivers, and the trailing `COMMIT` that should have complained produced only a
notice, which was being swallowed. The transaction was doing its job; the
protocol underneath it was not.

[`test/safety.test.ts`](test/safety.test.ts) fires 16 blocklist bypasses and 8
transaction escapes at it, and after each one asserts the data is untouched
rather than merely that an error came back.

**2. Writes off by default.**

A server holding database credentials should not be able to mutate anything
unless an operator said so. `ALLOW_WRITES` defaults to `false`, and write tools
are **omitted from the tool list** rather than registered-and-refusing —
offering a tool that can only fail wastes an agent's turn.

**3. Bounded output.**

A tool that returns a million rows does not help an agent, it exhausts the
context the agent needs to reason with. Output is capped and **says so** when
truncated. Silent truncation is worse than an error: an agent that believes it
saw a whole table will draw conclusions from a fragment.

**4. Errors that say what to do next.**

Postgres puts the actionable part in `detail` and `hint`, and most wrappers
drop both. Every error here carries SQLSTATE, detail, hint and position, and
the read-only refusal explains how to enable writes rather than just saying no.

## Tools

Read-only, always available:

| Tool | What it does |
|---|---|
| `ntl_list_tables` | Tables, views, row estimates, sizes, optionally columns |
| `ntl_list_extensions` | Installed and available extensions |
| `ntl_list_migrations` | Migrations applied through this server |
| `ntl_generate_typescript_types` | TypeScript interfaces from the live schema |
| `ntl_execute_sql` | Arbitrary SQL, read-only unless writes are enabled |
| `ntl_get_advisors` | Security and performance lint over the live database |
| `ntl_get_activity` | Current connections and slowest statements |
| `ntl_search_docs` | openNTL documentation, indexed offline |

openNTL domain tools — these are what make it openNTL's server rather than a
generic Postgres one:

| Tool | What it answers |
|---|---|
| `ntl_list_synapses` | What has the node learned? Weights, per-type affinity, decayed vs stored weight |
| `ntl_get_learning_health` | Is the model actually learning? Exploration and pending ratios |
| `ntl_list_journal` | Routing decisions and their outcomes — the training data |
| `ntl_get_node_status` | Identity, topology, activation snapshot, dedup entries |

Write tools, only when `ALLOW_WRITES=true`:

| Tool | What it does |
|---|---|
| `ntl_apply_migration` | Apply DDL in one transaction and record it in a ledger |
| `ntl_init_schema` | Create the openNTL schema. Idempotent. |

Plus a resource, `ntl://schema/postgres`, serving the reference DDL — useful
for an agent about to write a migration.

### Two tools worth stealing

`ntl_get_advisors` is where a database MCP server stops being a SQL pipe. It
lints the live database — tables granted to `PUBLIC`, `SECURITY DEFINER`
functions without a pinned `search_path`, unindexed foreign keys, missing
primary keys, unused indexes, bloat — and every finding carries a remediation.
A finding an operator cannot act on is noise.

Note what it deliberately does *not* do: no check requires a sequential scan of
user data. An advisory pass must not itself be the incident.

`ntl_get_learning_health` is worth copying for its *shape* rather than its
content: it does not just return numbers, it interprets them. Exploration at
zero across multiple peers means the node has stopped learning. Pending near
100% means no receipts are arriving and the weights reflect nothing. An agent
handed raw counters would have to know the domain to see either.

## Deploying

```bash
# 1. Hyperdrive, so connections are pooled outside the isolate
wrangler hyperdrive create ntl-postgres \
  --connection-string="postgres://user:pass@host/db"
# paste the id into wrangler.toml

# 2. Auth. Not optional — see below.
openssl rand -hex 32 | wrangler secret put MCP_AUTH_TOKEN

# 3. Ship
wrangler deploy
```

Then point a client at it:

```json
{
  "mcpServers": {
    "ntl-postgres": {
      "url": "https://ntl-postgres-mcp-server.<your-subdomain>.workers.dev/mcp",
      "headers": { "Authorization": "Bearer <your-token>" }
    }
  }
}
```

### Hyperdrive is not optional either

Without connection pooling, every Worker invocation opens its own Postgres
connection. A traffic burst exhausts `max_connections` long before it exhausts
anything else, and the failure looks like a database outage rather than a
capacity problem. `DATABASE_URL` exists for local development; use Hyperdrive
in production.

### Auth is required, and the server refuses to run without it

If `MCP_AUTH_TOKEN` is unset the server returns 500 to every request rather
than serving unauthenticated. An MCP server with database credentials and no
auth is an open SQL console on the public internet. Comparison is
timing-safe.

There is one unauthenticated route, `/health`, which reports nothing about the
database.

### Connect as a role that cannot write

Read-only transactions bound what the *SQL* can do. They say nothing about what
the *role* can do, so a bug in this server is still bounded by the grants on the
credentials you hand it. Give the read-only deployment a role with no write
grants:

```sql
CREATE ROLE ntl_mcp_ro LOGIN PASSWORD '…';
GRANT USAGE ON SCHEMA ntl TO ntl_mcp_ro;
GRANT SELECT ON ALL TABLES IN SCHEMA ntl TO ntl_mcp_ro;
ALTER DEFAULT PRIVILEGES IN SCHEMA ntl GRANT SELECT ON TABLES TO ntl_mcp_ro;
```

Then the transaction and the grants have to both fail before anything is
written. This costs nothing and is the difference between one layer and two.

### Enabling writes

Writes are enabled per environment, not per request, so it is a deliberate act
against a named target:

```bash
wrangler deploy --env admin   # ALLOW_WRITES=true
```

Keep the read-only deployment as the default one agents talk to.

## Copying this as a template

```bash
cp -r mcp/ntl-postgres-mcp-server my-mcp-server
```

Three files to change:

| File | What to do |
|---|---|
| `src/db.ts` | Implement `SqlExecutor` for your driver. Four methods. |
| `src/tools/ntl.ts` | Replace with your domain tools. Delete what does not apply. |
| `wrangler.toml` | Swap the Hyperdrive binding for what your database needs. |

Largely portable as-is: `src/safety.ts`, `src/format.ts`, `src/index.ts`,
`src/tools/schema.ts`, `src/tools/sql.ts`, and the whole test harness.

### If you point this at a different engine

Two assumptions here are Postgres-specific and will bite:

- **Transactional DDL.** `ntl_apply_migration` runs every statement in one
  transaction and rolls back together. MySQL does not support this; a migration
  that half-applies leaves a schema no version number describes. If you port
  this, either apply one statement per migration or say plainly that rollback
  is not guaranteed.
- **`SET TRANSACTION READ ONLY`.** The whole safety model rests on the
  database enforcing it. If your engine has no equivalent, you do not have
  read-only mode — do not pretend otherwise by falling back to SQL parsing.
  Check your driver's multi-statement behaviour too: a driver that quietly
  batches commands over a protocol that honours `COMMIT` gives the transaction
  away.

## Testing

No mocks. A mocked database passes while the SQL is wrong, which is the only
failure mode these tests exist to catch.

```bash
npm test                    # PGlite — real Postgres in WASM, no service needed
TEST_DATABASE_URL="postgres://user@localhost/db" npm test   # both
```

The suite runs against every configured backend. That is not belt-and-braces:
it caught two real bugs during development.

**Multi-statement SQL.** Parameterised statements go through the extended
protocol, which accepts exactly one command — so `ntl_init_schema` failed with
SQLSTATE 42601 on a script that worked fine as separate statements. Hence
`exec()` alongside `query()` in `SqlExecutor`.

**BIGINT type instability.** postgres.js returns BIGINT as a string, PGlite as
a number. A tool's structured output changed type depending on which driver
served it. Now normalised to a string at the driver boundary — correct anyway,
since BIGINT exceeds `Number.MAX_SAFE_INTEGER` and openNTL stores nanosecond
timestamps in it.

Both were invisible to a single-backend suite.

### Layers

| File | Covers |
|---|---|
| `test/safety.test.ts` | Read-only bypasses, identifier injection, rollback |
| `test/tools.test.ts` | Every tool against a real seeded schema |
| `test/protocol.test.ts` | The real MCP client, transport, schema validation, annotations |

The protocol layer is worth testing separately: a tool can be perfectly correct
and still unusable because its input schema rejects valid arguments.

## Local development

```bash
# Against a local Postgres
echo 'DATABASE_URL="postgres://localhost/ntl"' >> .dev.vars
echo 'MCP_AUTH_TOKEN="dev-token"' >> .dev.vars
npm run dev

# Poke at it
npx @modelcontextprotocol/inspector
```

## Licence

Apache 2.0, as with the rest of openNTL. Copy it.
