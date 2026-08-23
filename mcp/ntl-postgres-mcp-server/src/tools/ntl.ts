/**
 * openNTL domain tools.
 *
 * These are what make this openNTL's MCP server rather than a generic Postgres
 * one. They answer the questions an operator actually has about a running
 * node — is it learning, which peers are good, what is failing — without
 * requiring the agent to know the schema.
 *
 * Every one is read-only.
 */

import { z } from "zod";

import { DEFAULT_LIMIT, MAX_LIMIT } from "../constants.js";
import { jsonSafe, respond, rowsToMarkdown } from "../format.js";
import type { ToolOutput } from "../format.js";
import { quoteIdent } from "../safety.js";
import type { SqlExecutor } from "../types.js";

const formatArg = z.enum(["markdown", "json"]).default("markdown");

/** Nanoseconds in an hour, for decay maths done in SQL. */
const NS_PER_HOUR = 3_600_000_000_000;

export const listSynapsesInput = {
  state: z
    .enum(["forming", "active", "weakening", "dormant", "pruned", "eligible", "any"])
    .default("eligible")
    .describe(
      "Filter by lifecycle state. 'eligible' means active or weakening — the " +
        "states that may carry a signal. 'any' means no filter.",
    ),
  signal_type: z
    .string()
    .optional()
    .describe(
      "Rank by affinity for this signal type instead of raw weight, e.g. 'Query'",
    ),
  min_weight: z
    .number()
    .min(0)
    .max(1)
    .optional()
    .describe("Only synapses at or above this weight"),
  decay_half_life_hours: z
    .number()
    .positive()
    .default(168)
    .describe(
      "Half-life used to show the decayed weight alongside the stored one. " +
        "Defaults to the edge-class 168h.",
    ),
  limit: z.number().int().min(1).max(MAX_LIMIT).default(DEFAULT_LIMIT),
  response_format: formatArg,
};

/**
 * List synapses and their learned weights.
 *
 * Shows the **decayed** weight next to the stored one. A node applies decay
 * lazily, so the stored value can be stale — and an operator comparing a
 * stored weight against a routing decision made minutes later would otherwise
 * be comparing the wrong numbers.
 */
export async function listSynapses(
  db: SqlExecutor,
  schema: string,
  args: {
    state: string;
    signal_type?: string;
    min_weight?: number;
    decay_half_life_hours: number;
    limit: number;
    response_format: "markdown" | "json";
  },
): Promise<ToolOutput> {
  const s = quoteIdent(schema);

  const states =
    args.state === "eligible"
      ? ["active", "weakening"]
      : args.state === "any"
        ? null
        : [args.state];

  // Decay is computed in SQL so the comparison is against one clock, not the
  // agent's clock and the database's.
  const rows = await db.query(
    `WITH now_ns AS (
       SELECT (EXTRACT(EPOCH FROM now()) * 1e9)::bigint AS t
     )
     SELECT s.id,
            encode(substring(s.peer from 1 for 6), 'hex') AS peer,
            s.weight,
            GREATEST(
              s.weight * power(
                2,
                -- LEAST(..., 0) clamps the exponent, matching the Rust
                -- guard that returns the weight unchanged when now_ns is at or
                -- before last_active_ns. A
                -- last_active_ns in the future — clock skew between the node
                -- writing rows and the database reading them, or a restored
                -- backup — otherwise makes the exponent positive and reports a
                -- decayed weight *above* the stored one, which is not a thing
                -- decay can do.
                LEAST(
                  -((SELECT t FROM now_ns) - s.last_active_ns)::double precision
                    / ($4::double precision * ${NS_PER_HOUR}),
                  0
                )
              ),
              0.001
            )::real AS decayed_weight,
            s.state,
            s.type_affinity,
            s.signals_transmitted,
            s.signals_received,
            (s.avg_latency_ns / 1e6)::numeric(12,3) AS avg_latency_ms,
            s.error_rate,
            ROUND((((SELECT t FROM now_ns) - s.last_active_ns) / 3.6e12)::numeric, 2)
              AS idle_hours
       FROM ${s}.synapses s
      WHERE ($1::text[] IS NULL OR s.state = ANY($1::text[]))
        AND ($2::real IS NULL OR s.weight >= $2::real)
      ORDER BY CASE
                 WHEN $3::text IS NOT NULL
                 THEN COALESCE((s.type_affinity ->> $3::text)::numeric, 0)
                 ELSE 0
               END DESC,
               s.weight DESC,
               s.id ASC
      LIMIT $5`,
    [
      states,
      args.min_weight ?? null,
      args.signal_type ?? null,
      args.decay_half_life_hours,
      args.limit,
    ],
  );

  const totalRow = await db.query(
    `SELECT COALESCE(SUM(weight), 0)::real AS total_weight, COUNT(*)::int AS n
       FROM ${s}.synapses WHERE state <> 'pruned'`,
  );
  const total = totalRow.rows[0] ?? {};

  const structured = {
    synapses: jsonSafe(rows.rows),
    count: rows.rows.length,
    total_outbound_weight: total["total_weight"] ?? 0,
    unpruned_count: total["n"] ?? 0,
  };

  if (rows.rows.length === 0) {
    return respond(
      args.response_format,
      `No synapses matching state \`${args.state}\`. A node that has not yet ` +
        `connected to a peer has none.`,
      structured,
    );
  }

  const markdown = [
    `## Synapses (${rows.rows.length})`,
    args.signal_type ? `\nRanked by affinity for \`${args.signal_type}\`.` : "",
    "",
    rowsToMarkdown(rows.rows),
    "",
    `Total outbound weight across ${String(total["n"])} unpruned synapses: ` +
      `**${Number(total["total_weight"] ?? 0).toFixed(4)}**`,
    "",
    "_`weight` is stored; `decayed_weight` is what routing would use now. " +
      "A large gap means the node has not touched this synapse recently._",
  ].join("\n");

  return respond(args.response_format, markdown, structured);
}

export const learningHealthInput = {
  sample: z
    .number()
    .int()
    .min(1)
    .max(100_000)
    .default(1_000)
    .describe("How many recent decisions to sample"),
  response_format: formatArg,
};

/**
 * Report whether the routing model is actually learning.
 *
 * Two ratios matter more than the rest, and this tool interprets them rather
 * than leaving an operator to:
 *
 * - **exploration near zero** — the node has stopped trying alternatives, so it
 *   cannot discover a better path and routing has ossified.
 * - **pending near 100%** — no receipts are arriving, so no weight has moved
 *   and the model reflects nothing.
 */
export async function learningHealth(
  db: SqlExecutor,
  schema: string,
  args: { sample: number; response_format: "markdown" | "json" },
): Promise<ToolOutput> {
  const s = quoteIdent(schema);

  const result = await db.query(
    `WITH recent AS (
       SELECT * FROM ${s}.journal ORDER BY decided_at_ns DESC LIMIT $1
     )
     SELECT COUNT(*)::int AS sampled,
            COUNT(*) FILTER (WHERE explored)::int AS explored,
            COUNT(*) FILTER (WHERE outcome = 'pending')::int AS pending,
            COUNT(*) FILTER (WHERE outcome = 'delivered')::int AS delivered,
            COUNT(*) FILTER (WHERE outcome = 'rejected')::int AS rejected,
            COUNT(*) FILTER (WHERE outcome = 'timed_out')::int AS timed_out,
            COUNT(*) FILTER (WHERE outcome = 'transport_failure')::int AS transport_failure,
            COUNT(*) FILTER (WHERE outcome = 'signature_failure')::int AS signature_failure,
            COUNT(DISTINCT peer)::int AS distinct_peers
       FROM recent`,
    [args.sample],
  );

  const r = result.rows[0] ?? {};
  const sampled = Number(r["sampled"] ?? 0);

  if (sampled === 0) {
    return respond(
      args.response_format,
      "No routing decisions recorded. Either the node has not routed anything " +
        "yet, or journalling is disabled — a node that does not journal cannot " +
        "learn.",
      { sampled: 0, healthy: false, warnings: ["no decisions recorded"] },
    );
  }

  const pct = (n: unknown) => (Number(n ?? 0) / sampled) * 100;
  const explorationRatio = pct(r["explored"]);
  const pendingRatio = pct(r["pending"]);
  const deliveryRatio = pct(r["delivered"]);

  const warnings: string[] = [];
  // Only meaningful with more than one peer to choose between: with a single
  // synapse there is nothing to explore, and warning would train the operator
  // to ignore warnings.
  if (explorationRatio === 0 && Number(r["distinct_peers"] ?? 0) > 1) {
    warnings.push(
      "Exploration is at zero across multiple peers. This node has stopped " +
        "trying alternatives, so it can no longer discover a better path. " +
        "Check that exploration_temperature is above min_temperature.",
    );
  }
  if (pendingRatio > 80) {
    warnings.push(
      "Over 80% of decisions are unresolved. Receipts are not coming back, so " +
        "weights reflect nothing. Check that peers emit receipts and that the " +
        "timeout sweep is running.",
    );
  }
  if (Number(r["signature_failure"] ?? 0) > 0) {
    warnings.push(
      `${String(r["signature_failure"])} signature failure(s) in the sample. ` +
        "This is either an attack or a serious implementation defect; both " +
        "warrant investigation.",
    );
  }

  const structured = {
    sampled,
    exploration_ratio: explorationRatio / 100,
    pending_ratio: pendingRatio / 100,
    delivery_ratio: deliveryRatio / 100,
    outcomes: {
      delivered: Number(r["delivered"] ?? 0),
      rejected: Number(r["rejected"] ?? 0),
      timed_out: Number(r["timed_out"] ?? 0),
      transport_failure: Number(r["transport_failure"] ?? 0),
      signature_failure: Number(r["signature_failure"] ?? 0),
      pending: Number(r["pending"] ?? 0),
    },
    distinct_peers: Number(r["distinct_peers"] ?? 0),
    healthy: warnings.length === 0,
    warnings,
  };

  const markdown = [
    "## Routing model health",
    "",
    `- decisions sampled: **${sampled}**`,
    `- delivered: **${deliveryRatio.toFixed(1)}%**`,
    `- exploratory: **${explorationRatio.toFixed(1)}%**`,
    `- pending: **${pendingRatio.toFixed(1)}%**`,
    `- distinct peers: **${String(r["distinct_peers"])}**`,
    "",
    "### Outcomes",
    "",
    rowsToMarkdown([
      {
        delivered: r["delivered"],
        rejected: r["rejected"],
        timed_out: r["timed_out"],
        transport_failure: r["transport_failure"],
        signature_failure: r["signature_failure"],
        pending: r["pending"],
      },
    ]),
    "",
    warnings.length === 0
      ? "No warnings. The model is receiving outcomes and still exploring."
      : ["### Warnings", "", ...warnings.map((w) => `- ${w}`)].join("\n"),
  ].join("\n");

  return respond(args.response_format, markdown, structured);
}

export const listJournalInput = {
  outcome: z
    .enum([
      "any",
      "pending",
      "delivered",
      "rejected",
      "timed_out",
      "transport_failure",
      "signature_failure",
    ])
    .default("any")
    .describe("Filter by outcome"),
  signal_type: z.string().optional().describe("Filter by signal type, e.g. 'Query'"),
  explored_only: z
    .boolean()
    .default(false)
    .describe("Only exploratory decisions — where the model probed an alternative"),
  limit: z.number().int().min(1).max(MAX_LIMIT).default(DEFAULT_LIMIT),
  response_format: formatArg,
};

/** List routing decisions and their outcomes — the model's training data. */
export async function listJournal(
  db: SqlExecutor,
  schema: string,
  args: {
    outcome: string;
    signal_type?: string;
    explored_only: boolean;
    limit: number;
    response_format: "markdown" | "json";
  },
): Promise<ToolOutput> {
  const s = quoteIdent(schema);

  const rows = await db.query(
    `SELECT j.id,
            encode(substring(j.signal from 1 for 8), 'hex') AS signal,
            j.signal_type,
            encode(substring(j.peer from 1 for 6), 'hex') AS peer,
            j.score,
            j.signal_weight,
            j.explored,
            j.outcome,
            to_timestamp(j.decided_at_ns / 1e9) AS decided_at,
            CASE WHEN j.resolved_at_ns IS NOT NULL
                 THEN ROUND(((j.resolved_at_ns - j.decided_at_ns) / 1e6)::numeric, 1)
            END AS resolve_ms
       FROM ${s}.journal j
      WHERE ($1::text IS NULL OR j.outcome = $1::text)
        AND ($2::text IS NULL OR j.signal_type = $2::text)
        AND (NOT $3::boolean OR j.explored)
      ORDER BY j.decided_at_ns DESC, j.id DESC
      LIMIT $4`,
    [
      args.outcome === "any" ? null : args.outcome,
      args.signal_type ?? null,
      args.explored_only,
      args.limit,
    ],
  );

  const structured = { decisions: jsonSafe(rows.rows), count: rows.rows.length };

  if (rows.rows.length === 0) {
    return respond(
      args.response_format,
      "No matching decisions.",
      structured,
    );
  }

  return respond(
    args.response_format,
    [
      `## Routing decisions (${rows.rows.length})`,
      "",
      rowsToMarkdown(rows.rows),
      "",
      "_`resolve_ms` is how long the outcome took to come back. A null means " +
        "still pending._",
    ].join("\n"),
    structured,
  );
}

export const nodeStatusInput = {
  response_format: formatArg,
};

/** Summarise a node's persisted state: identity, topology, activation. */
export async function nodeStatus(
  db: SqlExecutor,
  schema: string,
  args: { response_format: "markdown" | "json" },
): Promise<ToolOutput> {
  const s = quoteIdent(schema);

  const [identity, synapses, peers, activation, dedup, version] = await Promise.all([
    db.query(
      `SELECT encode(substring(value from 1 for 6), 'hex') AS node_id
         FROM ${s}.meta WHERE key = 'node-id'`,
    ),
    db.query(
      `SELECT COUNT(*)::int AS total,
              COUNT(*) FILTER (WHERE state IN ('active','weakening'))::int AS eligible,
              COALESCE(SUM(weight) FILTER (WHERE state <> 'pruned'), 0)::real AS total_weight
         FROM ${s}.synapses`,
    ),
    db.query(
      `SELECT source, COUNT(*)::int AS n FROM ${s}.peers GROUP BY source ORDER BY source`,
    ),
    db.query(
      `SELECT potential, threshold, signals_fired,
              to_timestamp(taken_at_ns / 1e9) AS taken_at
         FROM ${s}.activation WHERE singleton`,
    ),
    db.query(
      `SELECT COUNT(*)::int AS n FROM ${s}.seen_signals
        WHERE expires_ns > (EXTRACT(EPOCH FROM now()) * 1e9)::bigint`,
    ),
    db.query(`SELECT version, applied_at FROM ${s}.schema_version WHERE singleton`),
  ]);

  const syn = synapses.rows[0] ?? {};
  const act = activation.rows[0];
  const structured = {
    node_id: identity.rows[0]?.["node_id"] ?? null,
    schema,
    schema_version: version.rows[0]?.["version"] ?? null,
    synapses: {
      total: syn["total"] ?? 0,
      eligible: syn["eligible"] ?? 0,
      total_outbound_weight: syn["total_weight"] ?? 0,
    },
    peers_by_source: jsonSafe(peers.rows),
    activation: act ? jsonSafe([act])[0] : null,
    live_dedup_entries: dedup.rows[0]?.["n"] ?? 0,
  };

  const markdown = [
    "## Node status",
    "",
    `- node id: \`${String(structured.node_id ?? "not set")}\``,
    `- schema: \`${schema}\` (version ${String(structured.schema_version ?? "unknown")})`,
    "",
    "### Topology",
    "",
    `- synapses: **${String(syn["total"])}** (${String(syn["eligible"])} eligible to carry signals)`,
    `- total outbound weight: **${Number(syn["total_weight"] ?? 0).toFixed(4)}**`,
    "",
    peers.rows.length > 0
      ? ["Peers by provenance:", "", rowsToMarkdown(peers.rows)].join("\n")
      : "No known peers.",
    "",
    "### Activation",
    "",
    act
      ? [
          `- potential: ${String(act["potential"])}`,
          `- threshold: ${String(act["threshold"])}`,
          `- signals fired: ${String(act["signals_fired"])}`,
          `- snapshot taken: ${String(act["taken_at"])}`,
        ].join("\n")
      : "No activation snapshot persisted. On a full node this means a restart " +
        "would reset backpressure.",
    "",
    `### Deduplication`,
    "",
    `- live entries: **${String(structured.live_dedup_entries)}**`,
  ].join("\n");

  return respond(args.response_format, markdown, structured);
}
