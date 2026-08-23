/**
 * Test harness.
 *
 * Tests run against **real Postgres**, twice:
 *
 * - PGlite, always. Real Postgres compiled to WebAssembly, in-process, so CI
 *   needs no database service.
 * - A live Postgres server, when `TEST_DATABASE_URL` is set. This catches the
 *   things PGlite cannot: the actual wire protocol through postgres.js, and any
 *   divergence between PGlite's Postgres version and the deployment target.
 *
 * There are no mocks. A mocked database would pass while the SQL was wrong,
 * which is the only failure mode these tests exist to catch.
 */

import { PGlite } from "@electric-sql/pglite";

import { INT8_OID, PgliteExecutor, PostgresExecutor } from "../src/db.js";
import type { PgliteLike } from "../src/db.js";
import type { ServerConfig, SqlExecutor } from "../src/types.js";

export const TEST_SCHEMA = "ntl_test";

const EXECUTOR_OPTIONS = { statementTimeoutMs: 10_000 };

/** A backend under test, with a label for test output. */
export interface Backend {
  name: string;
  create(): Promise<SqlExecutor>;
}

/**
 * Every backend the suite should run against.
 *
 * PGlite is unconditional. The live server is added only when configured, so a
 * developer without one still gets the full suite.
 */
export function backends(): Backend[] {
  const list: Backend[] = [
    {
      name: "pglite",
      async create() {
        const db = await PGlite.create({
          // Match postgres.js: BIGINT as a string, not a number. Without this
          // a tool's structured output changes type depending on which driver
          // served it, and nanosecond timestamps lose precision.
          parsers: { [INT8_OID]: (value: string) => value },
        });
        return new PgliteExecutor(db as unknown as PgliteLike, EXECUTOR_OPTIONS);
      },
    },
  ];

  const url = process.env["TEST_DATABASE_URL"];
  if (url) {
    list.push({
      name: "postgres",
      async create() {
        // Deliberately NOT destructive. An earlier version dropped the test
        // schema here, which was invisible on PGlite (every create() is an
        // isolated database) and broke every test on a real server: a nested
        // create() inside a single test wiped the schema the enclosing suite
        // had built. Resetting is now an explicit, separate step.
        return new PostgresExecutor(url, EXECUTOR_OPTIONS);
      },
    });
  }

  return list;
}

/**
 * Drop and recreate a schema, so a suite starts from a known state.
 *
 * Explicit rather than folded into `create()`: a constructor with a
 * destructive side effect is a trap for any test that needs a second
 * connection.
 */
export async function resetSchema(db: SqlExecutor, schema: string): Promise<void> {
  await db.query(`DROP SCHEMA IF EXISTS ${schema} CASCADE`);
}

/** Config for a test server. */
export function testConfig(overrides: Partial<ServerConfig> = {}): ServerConfig {
  return {
    databaseUrl: "postgres://test",
    allowWrites: false,
    schema: TEST_SCHEMA,
    statementTimeoutMs: 10_000,
    maxRows: 1_000,
    ...overrides,
  };
}

/** Read the text of a tool result. */
export function textOf(result: {
  content: { type: string; text?: string }[];
}): string {
  return result.content
    .filter((c) => c.type === "text")
    .map((c) => c.text ?? "")
    .join("\n");
}

/**
 * Seed a schema with representative openNTL data.
 *
 * The values are chosen to exercise the interesting cases rather than to look
 * plausible: a synapse in every lifecycle state, decisions with every outcome,
 * an exploratory decision, and a peer of each provenance.
 */
export async function seedNtlData(db: SqlExecutor, schema: string): Promise<void> {
  const nowNs = Date.now() * 1_000_000;
  const hourNs = 3_600_000_000_000;

  await db.transaction(async (tx) => {
    // Node identity.
    await tx.query(
      `INSERT INTO ${schema}.meta (key, value) VALUES ('node-id', decode($1,'hex'))
       ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value`,
      ["a1b2c3d4e5f6" + "00".repeat(26)],
    );

    // One synapse per lifecycle state, so state filtering is genuinely tested.
    const states = ["active", "active", "weakening", "dormant", "pruned"];
    for (let i = 0; i < states.length; i++) {
      await tx.query(
        `INSERT INTO ${schema}.synapses
           (id, peer, weight, attenuation_factor, state, type_affinity,
            established_at_ns, last_active_ns, signals_transmitted,
            signals_received, avg_latency_ns, error_rate)
         VALUES ($1, decode($2,'hex'), $3, 0.9, $4, $5::jsonb, $6, $7, $8, $9, $10, $11)
         ON CONFLICT (id) DO NOTHING`,
        [
          `syn-${i}`,
          (i + 1).toString(16).padStart(2, "0").repeat(32),
          // Descending weights so ordering assertions are meaningful.
          0.9 - i * 0.2,
          states[i],
          JSON.stringify({ Data: 10 - i, Query: i * 3 }),
          nowNs - 100 * hourNs,
          // The last synapse is stale, so decay has something to show.
          i === 4 ? nowNs - 500 * hourNs : nowNs - i * hourNs,
          100 - i * 10,
          50 - i * 5,
          1_500_000 + i * 100_000,
          i * 0.05,
        ],
      );
    }

    // Peers, one per provenance, because provenance gates eviction.
    const sources = ["configured", "bootstrap", "discovered", "observed"];
    for (let i = 0; i < sources.length; i++) {
      await tx.query(
        `INSERT INTO ${schema}.peers
           (id, addresses, region, advertised_types, last_seen_ns, source)
         VALUES (decode($1,'hex'), $2::jsonb, $3, $4::jsonb, $5, $6)
         ON CONFLICT (id) DO NOTHING`,
        [
          (i + 1).toString(16).padStart(2, "0").repeat(32),
          JSON.stringify([`ntl://10.0.0.${i + 1}:4433`]),
          i < 2 ? "af-south-1" : null,
          JSON.stringify(["Data", "Query"]),
          nowNs - i * hourNs,
          sources[i],
        ],
      );
    }

    // Activation snapshot.
    await tx.query(
      `INSERT INTO ${schema}.activation
         (singleton, potential, threshold, refractory_until_ns, signals_fired, taken_at_ns)
       VALUES (TRUE, 0.42, 0.55, $1, 137, $2)
       ON CONFLICT (singleton) DO UPDATE
         SET potential = EXCLUDED.potential, threshold = EXCLUDED.threshold`,
      [nowNs + 1_000_000, nowNs],
    );

    // Journal: every outcome represented, and one exploratory decision.
    const outcomes = [
      "delivered",
      "delivered",
      "delivered",
      "rejected",
      "timed_out",
      "pending",
      "transport_failure",
    ];
    for (let i = 0; i < outcomes.length; i++) {
      await tx.query(
        `INSERT INTO ${schema}.journal
           (signal, signal_type, synapse, peer, score, signal_weight, explored,
            decided_at_ns, outcome, resolved_at_ns)
         VALUES (decode($1,'hex'), $2, $3, decode($4,'hex'), $5, 0.8, $6, $7, $8, $9)`,
        [
          (i + 16).toString(16).padStart(2, "0").repeat(16),
          i % 3 === 0 ? "Query" : "Data",
          `syn-${i % 2}`,
          ((i % 2) + 1).toString(16).padStart(2, "0").repeat(32),
          0.5 + i * 0.05,
          i === 1,
          nowNs - i * 60_000_000_000,
          outcomes[i],
          outcomes[i] === "pending" ? null : nowNs - i * 60_000_000_000 + 500_000_000,
        ],
      );
    }

    // Live and expired dedup entries, so expiry filtering is tested.
    await tx.query(
      `INSERT INTO ${schema}.seen_signals (id, expires_ns) VALUES
         (decode($1,'hex'), $2), (decode($3,'hex'), $4)
       ON CONFLICT (id) DO NOTHING`,
      ["aa".repeat(16), nowNs + hourNs, "bb".repeat(16), nowNs - hourNs],
    );

    // Influence records.
    await tx.query(
      `INSERT INTO ${schema}.influence (peer, magnitude, at_ns) VALUES
         (decode($1,'hex'), 0.05, $2), (decode($1,'hex'), 0.03, $3)`,
      ["01".repeat(32), nowNs - 600_000_000_000, nowNs - 300_000_000_000],
    );
  });
}
