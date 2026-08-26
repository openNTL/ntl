/**
 * Operational tools: advisors, activity, and documentation search.
 *
 * `ntl_get_advisors` mirrors the Supabase MCP's advisor concept — a lint pass
 * over the live database rather than over code. It is the tool most worth
 * copying into a template, because it is where an MCP server stops being a SQL
 * pipe and starts being useful.
 */

import { z } from "zod";

import { jsonSafe, respond, rowsToMarkdown } from "../format.js";
import type { ToolOutput } from "../format.js";
import { SYSTEM_SCHEMAS } from "../constants.js";
import type { Row, SqlExecutor } from "../types.js";

const formatArg = z.enum(["markdown", "json"]).default("markdown");

export const getAdvisorsInput = {
  category: z
    .enum(["all", "security", "performance"])
    .default("all")
    .describe("Which advisor checks to run"),
  response_format: formatArg,
};

/** One advisory finding. */
interface Advisory {
  level: "error" | "warning" | "info";
  category: "security" | "performance";
  check: string;
  detail: string;
  remediation: string;
}

/**
 * Lint the live database for security and performance problems.
 *
 * Each check is chosen because it is both common and consequential, and each
 * finding carries a remediation — a finding an operator cannot act on is just
 * noise. Checks that would require a sequential scan of user data are excluded:
 * an advisory pass must not itself be the incident.
 */
export async function getAdvisors(
  db: SqlExecutor,
  args: { category: "all" | "security" | "performance"; response_format: "markdown" | "json" },
): Promise<ToolOutput> {
  const findings: Advisory[] = [];
  const wantSecurity = args.category === "all" || args.category === "security";
  const wantPerformance = args.category === "all" || args.category === "performance";

  if (wantSecurity) {
    // Tables readable by PUBLIC. On a database reachable from an application
    // role this is how data leaks without anyone granting anything.
    const publicGrants = await db.query(
      `SELECT table_schema || '.' || table_name AS relation, privilege_type
         FROM information_schema.role_table_grants
        WHERE grantee = 'PUBLIC'
          AND NOT (table_schema = ANY($1::text[]))
        ORDER BY relation
        LIMIT 50`,
      [SYSTEM_SCHEMAS],
    );
    for (const row of publicGrants.rows) {
      findings.push({
        level: "warning",
        category: "security",
        check: "public_table_grant",
        detail: `${String(row["relation"])} grants ${String(row["privilege_type"])} to PUBLIC`,
        remediation: `REVOKE ${String(row["privilege_type"])} ON ${String(row["relation"])} FROM PUBLIC;`,
      });
    }

    // SECURITY DEFINER functions run as their owner. Without a pinned
    // search_path, a caller who can create objects can shadow a name the
    // function resolves and have it executed with the owner's rights.
    const securityDefiner = await db.query(
      `SELECT n.nspname || '.' || p.proname AS fn
         FROM pg_proc p
         JOIN pg_namespace n ON n.oid = p.pronamespace
        WHERE p.prosecdef
          AND NOT (n.nspname = ANY($1::text[]))
          AND (p.proconfig IS NULL
               OR NOT EXISTS (
                 SELECT 1 FROM unnest(p.proconfig) c WHERE c LIKE 'search_path=%'
               ))
        ORDER BY fn
        LIMIT 50`,
      [SYSTEM_SCHEMAS],
    );
    for (const row of securityDefiner.rows) {
      findings.push({
        level: "error",
        category: "security",
        check: "security_definer_without_search_path",
        detail: `${String(row["fn"])} is SECURITY DEFINER with no pinned search_path`,
        remediation:
          `ALTER FUNCTION ${String(row["fn"])} SET search_path = pg_catalog, public;` +
          ` Without this, a caller who can create objects can shadow a name the` +
          ` function resolves and have it run with the owner's privileges.`,
      });
    }

    // Superuser roles that can log in are a blast-radius problem.
    const superusers = await db.query(
      `SELECT rolname FROM pg_roles WHERE rolsuper AND rolcanlogin ORDER BY rolname`,
    );
    if (superusers.rows.length > 1) {
      findings.push({
        level: "warning",
        category: "security",
        check: "multiple_login_superusers",
        detail: `${superusers.rows.length} superuser roles can log in: ${superusers.rows
          .map((r) => String(r["rolname"]))
          .join(", ")}`,
        remediation:
          "Applications should connect as a least-privilege role. Reserve " +
          "superuser for administration.",
      });
    }
  }

  if (wantPerformance) {
    // Foreign keys without a supporting index make every parent delete or
    // update a sequential scan of the child.
    const unindexedFks = await db.query(
      `SELECT c.conrelid::regclass::text AS child,
              c.conname AS constraint_name
         FROM pg_constraint c
        WHERE c.contype = 'f'
          AND NOT EXISTS (
            SELECT 1 FROM pg_index i
             WHERE i.indrelid = c.conrelid
               AND (c.conkey::smallint[]) <@ (i.indkey::smallint[])
          )
          AND NOT (c.connamespace::regnamespace::text = ANY($1::text[]))
        ORDER BY child
        LIMIT 50`,
      [SYSTEM_SCHEMAS],
    );
    for (const row of unindexedFks.rows) {
      findings.push({
        level: "warning",
        category: "performance",
        check: "unindexed_foreign_key",
        detail: `${String(row["child"])} has foreign key ${String(row["constraint_name"])} with no covering index`,
        remediation:
          `Add an index on the referencing columns. Without one, every delete ` +
          `or key update on the parent scans ${String(row["child"])} sequentially.`,
      });
    }

    // Tables with no primary key cannot be replicated logically, and cannot be
    // updated safely by row identity.
    const noPk = await db.query(
      `SELECT n.nspname || '.' || c.relname AS relation
         FROM pg_class c
         JOIN pg_namespace n ON n.oid = c.relnamespace
        WHERE c.relkind = 'r'
          AND NOT (n.nspname = ANY($1::text[]))
          AND NOT EXISTS (
            SELECT 1 FROM pg_constraint k
             WHERE k.conrelid = c.oid AND k.contype = 'p'
          )
        ORDER BY relation
        LIMIT 50`,
      [SYSTEM_SCHEMAS],
    );
    for (const row of noPk.rows) {
      findings.push({
        level: "warning",
        category: "performance",
        check: "missing_primary_key",
        detail: `${String(row["relation"])} has no primary key`,
        remediation:
          "Add one. Without a primary key the table cannot participate in " +
          "logical replication and rows cannot be addressed by identity.",
      });
    }

    // Indexes that have never been read cost write throughput for nothing.
    // reltuples guards against flagging indexes on tables too small to matter.
    const unusedIndexes = await db.query(
      `SELECT s.schemaname || '.' || s.indexrelname AS index_name,
              s.relname AS "table",
              pg_size_pretty(pg_relation_size(s.indexrelid)) AS size
         FROM pg_stat_user_indexes s
         JOIN pg_index i ON i.indexrelid = s.indexrelid
         JOIN pg_class c ON c.oid = s.relid
        WHERE s.idx_scan = 0
          AND NOT i.indisunique
          AND NOT i.indisprimary
          AND c.reltuples > 10000
        ORDER BY pg_relation_size(s.indexrelid) DESC
        LIMIT 25`,
    );
    for (const row of unusedIndexes.rows) {
      findings.push({
        level: "info",
        category: "performance",
        check: "unused_index",
        detail: `${String(row["index_name"])} on ${String(row["table"])} (${String(row["size"])}) has never been scanned`,
        remediation:
          "Consider dropping it. Note that statistics reset on restart, so " +
          "confirm the counter has had time to accumulate before acting.",
      });
    }

    // Bloat proxy: dead tuples relative to live ones.
    const bloat = await db.query(
      `SELECT schemaname || '.' || relname AS relation,
              n_dead_tup AS dead_tuples,
              n_live_tup AS live_tuples,
              last_autovacuum
         FROM pg_stat_user_tables
        WHERE n_dead_tup > 10000
          AND n_live_tup > 0
          AND n_dead_tup::float / n_live_tup > 0.2
        ORDER BY n_dead_tup DESC
        LIMIT 25`,
    );
    for (const row of bloat.rows) {
      findings.push({
        level: "warning",
        category: "performance",
        check: "table_bloat",
        detail:
          `${String(row["relation"])} has ${String(row["dead_tuples"])} dead vs ` +
          `${String(row["live_tuples"])} live tuples`,
        remediation:
          "Run VACUUM (ANALYZE), and check whether autovacuum is keeping up " +
          "with the write rate on this table.",
      });
    }
  }

  const byLevel = {
    error: findings.filter((f) => f.level === "error").length,
    warning: findings.filter((f) => f.level === "warning").length,
    info: findings.filter((f) => f.level === "info").length,
  };

  const structured = { findings, count: findings.length, by_level: byLevel };

  if (findings.length === 0) {
    return respond(
      args.response_format,
      `No ${args.category === "all" ? "" : `${args.category} `}advisories. ` +
        `Note this is a lint pass over schema and statistics, not a security ` +
        `audit — it cannot see application logic or network exposure.`,
      structured,
    );
  }

  const rows: Row[] = findings.map((f) => ({
    level: f.level,
    category: f.category,
    check: f.check,
    detail: f.detail,
  }));

  const markdown = [
    `## Advisories (${findings.length})`,
    "",
    `${byLevel.error} error, ${byLevel.warning} warning, ${byLevel.info} info.`,
    "",
    rowsToMarkdown(rows),
    "",
    "## Remediation",
    "",
    ...findings.map((f) => `**${f.check}** — ${f.detail}\n\n> ${f.remediation}\n`),
  ].join("\n");

  return respond(args.response_format, markdown, structured);
}

export const getActivityInput = {
  include_idle: z
    .boolean()
    .default(false)
    .describe("Include idle connections as well as active ones"),
  response_format: formatArg,
};

/**
 * Show current database activity.
 *
 * Named `activity` rather than `logs`: Postgres log files are not reachable
 * over a SQL connection, and a tool called `get_logs` that returned session
 * state instead would be lying about what it does. What is reachable —
 * `pg_stat_activity`, and `pg_stat_statements` when installed — is usually what
 * an operator wanted anyway.
 */
export async function getActivity(
  db: SqlExecutor,
  args: { include_idle: boolean; response_format: "markdown" | "json" },
): Promise<ToolOutput> {
  const activity = await db.query(
    `SELECT pid,
            usename AS "user",
            application_name,
            state,
            wait_event_type,
            ROUND(EXTRACT(EPOCH FROM (now() - query_start))::numeric, 2) AS query_seconds,
            LEFT(query, 200) AS query
       FROM pg_stat_activity
      WHERE datname = current_database()
        AND pid <> pg_backend_pid()
        AND ($1::boolean OR state <> 'idle')
      ORDER BY query_start ASC NULLS LAST
      LIMIT 100`,
    [args.include_idle],
  );

  // pg_stat_statements is the single most useful extension for this, and is
  // frequently absent. Say so rather than returning a bare empty result.
  const hasStatements = await db.query(
    `SELECT 1 FROM pg_extension WHERE extname = 'pg_stat_statements'`,
  );

  let slowest: Row[] = [];
  if (hasStatements.rows.length > 0) {
    try {
      const result = await db.query(
        `SELECT LEFT(query, 200) AS query,
                calls,
                ROUND(total_exec_time::numeric, 1) AS total_ms,
                ROUND(mean_exec_time::numeric, 2) AS mean_ms,
                rows
           FROM pg_stat_statements
          ORDER BY total_exec_time DESC
          LIMIT 20`,
      );
      slowest = result.rows;
    } catch {
      // Column names differ across pg_stat_statements versions. A failure here
      // must not take out the activity listing, which is the primary answer.
      slowest = [];
    }
  }

  const structured = {
    activity: jsonSafe(activity.rows),
    connection_count: activity.rows.length,
    pg_stat_statements_installed: hasStatements.rows.length > 0,
    slowest_statements: jsonSafe(slowest),
  };

  const markdown = [
    `## Activity (${activity.rows.length} connection(s))`,
    "",
    rowsToMarkdown(activity.rows),
    "",
    hasStatements.rows.length > 0
      ? [
          "## Slowest statements by total time",
          "",
          slowest.length > 0
            ? rowsToMarkdown(slowest)
            : "_pg_stat_statements is installed but returned nothing usable; " +
              "its column names vary by version._",
        ].join("\n")
      : "_`pg_stat_statements` is not installed, so per-statement timings are " +
        "unavailable. `CREATE EXTENSION pg_stat_statements;` to enable it._",
    "",
    "_Postgres log files are not reachable over a SQL connection. This tool " +
      "reports live session state and, where available, statement statistics._",
  ].join("\n");

  return respond(args.response_format, markdown, structured);
}

/** Documentation entries the search tool matches against. */
const DOCS: { title: string; url: string; keywords: string[]; summary: string }[] = [
  {
    title: "Storage Interface",
    url: "https://openntl.org/spec/storage-interface",
    keywords: ["storage", "nodestore", "persist", "backend", "durability", "dedup", "postgres", "sqlite"],
    summary:
      "Normative contract for storage backends: what must be persisted, durability per deployment class, and what may be memory-only.",
  },
  {
    title: "Learning Model",
    url: "https://openntl.org/spec/learning-model",
    keywords: ["learning", "weight", "reward", "receipt", "decay", "exploration", "softmax", "influence", "hyperparameter"],
    summary:
      "How routing weights are updated: reward signal, Hebbian update rule, half-life decay, outbound normalisation, per-identity influence caps, and exploration policy.",
  },
  {
    title: "Threat Model",
    url: "https://openntl.org/spec/threat-model",
    keywords: ["threat", "security", "attack", "sybil", "poisoning", "eclipse", "forgery"],
    summary:
      "Adversarial analysis of learned routing, and an explicit list of what NTL does not defend against.",
  },
  {
    title: "Delivery Semantics",
    url: "https://openntl.org/spec/delivery-semantics",
    keywords: ["delivery", "acknowledged", "best-effort", "retry", "idempotency", "receipt"],
    summary:
      "Best-effort and acknowledged delivery classes, retry policy with full jitter, and idempotency keyed on signal ID.",
  },
  {
    title: "Activation Model",
    url: "https://openntl.org/spec/activation-model",
    keywords: ["activation", "threshold", "refractory", "queue", "overflow", "backpressure", "batch"],
    summary:
      "Threshold-based admission control, batched firing, bounded queue with overflow policy, and node-class refractory periods.",
  },
  {
    title: "Propagation Rules",
    url: "https://openntl.org/spec/propagation-rules",
    keywords: ["propagation", "routing", "ttl", "loop", "scope", "flood", "targeted", "gradient", "fanout"],
    summary:
      "How signals route: TTL, loop prevention, weight floor, deduplication, signature verification, and the four propagation scopes.",
  },
  {
    title: "Signal Format",
    url: "https://openntl.org/spec/signal-format",
    keywords: ["signal", "format", "cbor", "wire", "encoding", "field", "validation"],
    summary:
      "Binary wire format, body fields, encoding options, signal types, and validation rules ordered cheapest-first.",
  },
  {
    title: "Synapse Lifecycle",
    url: "https://openntl.org/spec/synapse-lifecycle",
    keywords: ["synapse", "lifecycle", "state", "weakening", "dormant", "prune", "eligibility", "handshake"],
    summary:
      "Formation, state transitions, which states may carry signals, and pruning. Includes why Weakening must stay eligible.",
  },
  {
    title: "Storage Backends Guide",
    url: "https://openntl.org/guides/storage-backends",
    keywords: ["backend", "multi-database", "sqlite", "postgres", "graph", "kv", "siafudb", "redis", "neo4j"],
    summary:
      "The multi-database matrix: SQLite, PostgreSQL, graph databases, KV stores, SiafuDB, and how to write your own backend.",
  },
  {
    title: "Postgres MCP Server Guide",
    url: "https://openntl.org/guides/postgres-mcp",
    keywords: ["mcp", "postgres", "workers", "cloudflare", "template", "hyperdrive", "tool"],
    summary:
      "This server: its tools, how to deploy it on Cloudflare Workers, and how to copy it as a template for another database.",
  },
  {
    title: "Quickstart",
    url: "https://openntl.org/guides/quickstart",
    keywords: ["quickstart", "install", "cli", "init", "emit", "listen", "getting started"],
    summary:
      "Build the CLI, initialize a node, exchange an acknowledged signal between two local nodes, and watch a weight change.",
  },
  {
    title: "Prior Art",
    url: "https://openntl.org/research/07-prior-art",
    keywords: ["prior art", "q-routing", "antnet", "dtn", "libp2p", "mqtt", "nats", "ndn", "comparison"],
    summary:
      "How NTL relates to Q-routing, AntNet, Named Data Networking, delay-tolerant networking, libp2p, MQTT and NATS — including where those are the better choice.",
  },
];

export const searchDocsInput = {
  query: z
    .string()
    .min(2)
    .max(200)
    .describe("What to search for, e.g. 'influence cap' or 'delivery guarantee'"),
  limit: z.number().int().min(1).max(12).default(5),
};

/**
 * Search openNTL documentation.
 *
 * The index is embedded rather than fetched. A documentation tool that needs an
 * outbound HTTP request fails exactly when an operator most needs it — during
 * an incident, behind a restrictive egress policy — and the corpus here is
 * small enough that the tradeoff is not close.
 */
export function searchDocs(args: { query: string; limit: number }): ToolOutput {
  const terms = args.query
    .toLowerCase()
    .split(/[^a-z0-9-]+/)
    .filter((t) => t.length > 1);

  const scored = DOCS.map((doc) => {
    const haystack = `${doc.title} ${doc.keywords.join(" ")} ${doc.summary}`.toLowerCase();
    let score = 0;
    for (const term of terms) {
      if (doc.title.toLowerCase().includes(term)) score += 5;
      if (doc.keywords.some((k) => k.includes(term))) score += 3;
      if (haystack.includes(term)) score += 1;
    }
    return { doc, score };
  })
    .filter((s) => s.score > 0)
    .sort((a, b) => b.score - a.score)
    .slice(0, args.limit);

  if (scored.length === 0) {
    return respond(
      "markdown",
      `No documentation matched ${JSON.stringify(args.query)}.\n\n` +
        `Available topics: ${DOCS.map((d) => d.title).join(", ")}.`,
      { results: [], count: 0 },
    );
  }

  return respond(
    "markdown",
    [
      `## Documentation matches (${scored.length})`,
      "",
      ...scored.map(
        ({ doc }) => `### [${doc.title}](${doc.url})\n\n${doc.summary}\n`,
      ),
    ].join("\n"),
    {
      results: scored.map(({ doc, score }) => ({
        title: doc.title,
        url: doc.url,
        summary: doc.summary,
        relevance: score,
      })),
      count: scored.length,
    },
  );
}
