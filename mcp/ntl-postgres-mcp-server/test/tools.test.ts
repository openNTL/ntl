/**
 * Tool behaviour against a real schema with real data.
 *
 * Every test creates the openNTL schema through the actual `ntl_init_schema`
 * tool and seeds it, so the schema file itself is under test rather than a
 * hand-written approximation of it.
 */

import { afterAll, beforeAll, describe, expect, it } from "vitest";

import NTL_SCHEMA_SQL from "../src/schema/ntl.sql";
import * as ntlTools from "../src/tools/ntl.js";
import * as opsTools from "../src/tools/ops.js";
import * as schemaTools from "../src/tools/schema.js";
import * as sqlTools from "../src/tools/sql.js";
import type { SqlExecutor } from "../src/types.js";
import { backends, resetSchema, seedNtlData, TEST_SCHEMA, textOf } from "./harness.js";

for (const backend of backends()) {
  describe(`tools (${backend.name})`, () => {
    let db: SqlExecutor;

    beforeAll(async () => {
      db = await backend.create();
      await resetSchema(db, TEST_SCHEMA);

      // Create the schema through the tool. If the schema file is wrong, this
      // fails here rather than producing confusing failures later.
      const result = await sqlTools.initSchema(db, TEST_SCHEMA, NTL_SCHEMA_SQL, {
        confirm: true,
      });
      expect(result.isError, textOf(result)).toBeFalsy();

      await seedNtlData(db, TEST_SCHEMA);
    });

    afterAll(async () => {
      await db?.close();
    });

    // ------------------------------------------------------------- schema

    describe("ntl_init_schema", () => {
      it("creates every expected table", async () => {
        const tables = await db.query(
          `SELECT table_name FROM information_schema.tables
            WHERE table_schema = $1 ORDER BY table_name`,
          [TEST_SCHEMA],
        );
        const names = tables.rows.map((r) => String(r["table_name"]));
        for (const expected of [
          "activation",
          "influence",
          "journal",
          "meta",
          "peers",
          "schema_version",
          "seen_signals",
          "signal_history",
          "synapses",
        ]) {
          expect(names, `missing table ${expected}`).toContain(expected);
        }
      });

      it("is idempotent", async () => {
        const again = await sqlTools.initSchema(db, TEST_SCHEMA, NTL_SCHEMA_SQL, {
          confirm: true,
        });
        expect(again.isError, textOf(again)).toBeFalsy();
      });

      it("refuses without confirmation", async () => {
        const result = await sqlTools.initSchema(db, TEST_SCHEMA, NTL_SCHEMA_SQL, {
          confirm: false,
        });
        expect(result.isError).toBe(true);
        expect(textOf(result)).toContain("confirm");
      });

      it("enforces the schema's own constraints", async () => {
        // The CHECK constraints are load-bearing: a weight outside [0,1] would
        // corrupt routing rather than merely look odd.
        await expect(
          db.query(
            `INSERT INTO ${TEST_SCHEMA}.synapses
               (id, peer, weight, attenuation_factor, state, established_at_ns, last_active_ns)
             VALUES ('bad', decode('ff','hex'), 5.0, 0.9, 'active', 0, 0)`,
          ),
        ).rejects.toThrow();

        await expect(
          db.query(
            `INSERT INTO ${TEST_SCHEMA}.synapses
               (id, peer, weight, attenuation_factor, state, established_at_ns, last_active_ns)
             VALUES ('bad2', decode('fe','hex'), 0.5, 0.9, 'not_a_state', 0, 0)`,
          ),
        ).rejects.toThrow();
      });

      it("enforces one synapse per peer", async () => {
        await expect(
          db.query(
            `INSERT INTO ${TEST_SCHEMA}.synapses
               (id, peer, weight, attenuation_factor, state, established_at_ns, last_active_ns)
             VALUES ('dup', decode($1,'hex'), 0.5, 0.9, 'active', 0, 0)`,
            ["01".repeat(32)],
          ),
        ).rejects.toThrow();
      });
    });

    describe("ntl_list_tables", () => {
      it("lists the openNTL tables with columns", async () => {
        const result = await db.readOnly((tx) =>
          schemaTools.listTables(tx, {
            schemas: [TEST_SCHEMA],
            include_columns: true,
            response_format: "markdown",
          }),
        );
        const text = textOf(result);
        expect(text).toContain("synapses");
        expect(text).toContain("journal");
        expect(text).toContain("## Columns");
        expect(result.structuredContent?.["table_count"]).toBeGreaterThan(5);
      });

      it("omits columns when asked", async () => {
        const result = await db.readOnly((tx) =>
          schemaTools.listTables(tx, {
            schemas: [TEST_SCHEMA],
            include_columns: false,
            response_format: "markdown",
          }),
        );
        expect(textOf(result)).not.toContain("## Columns");
      });

      it("returns json when asked", async () => {
        const result = await db.readOnly((tx) =>
          schemaTools.listTables(tx, {
            schemas: [TEST_SCHEMA],
            include_columns: false,
            response_format: "json",
          }),
        );
        const parsed = JSON.parse(textOf(result)) as { tables: unknown[] };
        expect(Array.isArray(parsed.tables)).toBe(true);
      });

      it("guides the user when a schema is empty", async () => {
        const result = await db.readOnly((tx) =>
          schemaTools.listTables(tx, {
            schemas: ["definitely_not_a_schema"],
            include_columns: false,
            response_format: "markdown",
          }),
        );
        // An empty result should say what to do, not just be empty.
        expect(textOf(result)).toContain("ntl_init_schema");
      });
    });

    describe("ntl_generate_typescript_types", () => {
      it("maps bigint to string, not number", async () => {
        const result = await db.readOnly((tx) =>
          schemaTools.generateTypescriptTypes(tx, { schemas: [TEST_SCHEMA] }),
        );
        const ts = String(result.structuredContent?.["typescript"] ?? "");
        expect(ts).toContain("export interface Synapses");
        // The whole point: nanosecond timestamps exceed
        // Number.MAX_SAFE_INTEGER, so `number` would lose precision.
        expect(ts).toMatch(/last_active_ns: string/);
        expect(ts).toMatch(/weight: number/);
        expect(ts).toMatch(/peer: Uint8Array/);
      });
    });

    describe("ntl_list_migrations", () => {
      it("reports an absent ledger as empty rather than an error", async () => {
        const result = await db.readOnly((tx) =>
          schemaTools.listMigrations(tx, TEST_SCHEMA, {
            limit: 10,
            response_format: "json",
          }),
        );
        expect(result.isError).toBeFalsy();
        const parsed = JSON.parse(textOf(result)) as { ledger_exists: boolean };
        expect(typeof parsed.ledger_exists).toBe("boolean");
      });
    });

    describe("ntl_apply_migration", () => {
      it("applies, records, and is then listed", async () => {
        const applied = await sqlTools.applyMigration(db, TEST_SCHEMA, {
          name: "add_test_column",
          sql: `ALTER TABLE ${TEST_SCHEMA}.synapses ADD COLUMN IF NOT EXISTS test_note text`,
        });
        expect(applied.isError, textOf(applied)).toBeFalsy();

        const cols = await db.query(
          `SELECT 1 FROM information_schema.columns
            WHERE table_schema = $1 AND table_name = 'synapses' AND column_name = 'test_note'`,
          [TEST_SCHEMA],
        );
        expect(cols.rows.length).toBe(1);

        const listed = await db.readOnly((tx) =>
          schemaTools.listMigrations(tx, TEST_SCHEMA, {
            limit: 10,
            response_format: "json",
          }),
        );
        expect(textOf(listed)).toContain("add_test_column");
      });

      it("rolls back completely when any statement fails", async () => {
        const before = await db.query(
          `SELECT count(*)::int AS n FROM information_schema.columns
            WHERE table_schema = $1 AND table_name = 'synapses'`,
          [TEST_SCHEMA],
        );

        const result = await sqlTools.applyMigration(db, TEST_SCHEMA, {
          name: "half_broken",
          sql:
            `ALTER TABLE ${TEST_SCHEMA}.synapses ADD COLUMN should_not_exist text; ` +
            `ALTER TABLE ${TEST_SCHEMA}.nonexistent_table ADD COLUMN x text`,
        });
        expect(result.isError).toBe(true);
        expect(textOf(result)).toContain("rolled back");

        const after = await db.query(
          `SELECT count(*)::int AS n FROM information_schema.columns
            WHERE table_schema = $1 AND table_name = 'synapses'`,
          [TEST_SCHEMA],
        );
        expect(
          after.rows[0]?.["n"],
          "a partially applied migration leaves a schema no version describes",
        ).toBe(before.rows[0]?.["n"]);

        const ledger = await db.query(
          `SELECT count(*)::int AS n FROM ${TEST_SCHEMA}.migrations WHERE name = 'half_broken'`,
        );
        expect(ledger.rows[0]?.["n"], "a failed migration must not be recorded").toBe(0);
      });
    });

    // ---------------------------------------------------------------- ntl

    describe("ntl_list_synapses", () => {
      it("returns eligible synapses by default, excluding dormant and pruned", async () => {
        const result = await db.readOnly((tx) =>
          ntlTools.listSynapses(tx, TEST_SCHEMA, {
            state: "eligible",
            decay_half_life_hours: 168,
            limit: 50,
            response_format: "json",
          }),
        );
        const parsed = JSON.parse(textOf(result)) as {
          synapses: { state: string }[];
        };
        expect(parsed.synapses.length).toBeGreaterThan(0);
        for (const s of parsed.synapses) {
          // Weakening is eligible — it means "below threshold, still
          // connected". Dormant and pruned are not.
          expect(["active", "weakening"]).toContain(s.state);
        }
      });

      it("filters by explicit state", async () => {
        const result = await db.readOnly((tx) =>
          ntlTools.listSynapses(tx, TEST_SCHEMA, {
            state: "dormant",
            decay_half_life_hours: 168,
            limit: 50,
            response_format: "json",
          }),
        );
        const parsed = JSON.parse(textOf(result)) as {
          synapses: { state: string }[];
        };
        for (const s of parsed.synapses) expect(s.state).toBe("dormant");
      });

      it("shows a decayed weight below the stored one for a stale synapse", async () => {
        const result = await db.readOnly((tx) =>
          ntlTools.listSynapses(tx, TEST_SCHEMA, {
            state: "any",
            decay_half_life_hours: 168,
            limit: 50,
            response_format: "json",
          }),
        );
        const parsed = JSON.parse(textOf(result)) as {
          synapses: { weight: number; decayed_weight: number; idle_hours: string }[];
        };
        const stale = parsed.synapses.find((s) => Number(s.idle_hours) > 400);
        expect(stale, "the seeded stale synapse should be present").toBeDefined();
        if (stale) {
          // Decay is why this matters: routing uses the decayed value, and an
          // operator comparing against the stored one would be misled.
          expect(stale.decayed_weight).toBeLessThan(stale.weight);
        }
      });

      it("never reports a decayed weight above the stored one", async () => {
        // A last_active_ns in the future — clock skew between the node writing
        // rows and the database reading them, or a restored backup — made the
        // SQL exponent positive, so "decay" reported a weight *above* the
        // stored value. The Rust implementation guards this; the SQL did not.
        await db.query(
          `INSERT INTO ${TEST_SCHEMA}.synapses
             (id, peer, weight, attenuation_factor, state, type_affinity,
              established_at_ns, last_active_ns, signals_transmitted,
              signals_received, avg_latency_ns, error_rate)
           VALUES ('syn-future', decode($1,'hex'), 0.5, 0.9, 'active', '{}'::jsonb,
                   0, (EXTRACT(EPOCH FROM now()) * 1e9)::bigint + 86400000000000,
                   0, 0, 0, 0)
           ON CONFLICT (id) DO UPDATE SET last_active_ns = EXCLUDED.last_active_ns`,
          ["ff".repeat(32)],
        );

        const result = await db.readOnly((tx) =>
          ntlTools.listSynapses(tx, TEST_SCHEMA, {
            state: "any",
            decay_half_life_hours: 168,
            limit: 50,
            response_format: "json",
          }),
        );
        const parsed = JSON.parse(textOf(result)) as {
          synapses: { id: string; weight: number; decayed_weight: number }[];
        };

        for (const syn of parsed.synapses) {
          expect(
            syn.decayed_weight,
            `${syn.id}: decay cannot increase a weight`,
          ).toBeLessThanOrEqual(syn.weight);
        }

        const future = parsed.synapses.find((syn) => syn.id === "syn-future");
        expect(future, "the future-dated synapse should be listed").toBeDefined();
        expect(future?.decayed_weight).toBeCloseTo(0.5, 5);

        await db.query(`DELETE FROM ${TEST_SCHEMA}.synapses WHERE id = 'syn-future'`);
      });

      it("honours min_weight", async () => {
        const result = await db.readOnly((tx) =>
          ntlTools.listSynapses(tx, TEST_SCHEMA, {
            state: "any",
            min_weight: 0.5,
            decay_half_life_hours: 168,
            limit: 50,
            response_format: "json",
          }),
        );
        const parsed = JSON.parse(textOf(result)) as { synapses: { weight: number }[] };
        for (const s of parsed.synapses) expect(s.weight).toBeGreaterThanOrEqual(0.5);
      });

      it("ranks by affinity when a signal type is given", async () => {
        const result = await db.readOnly((tx) =>
          ntlTools.listSynapses(tx, TEST_SCHEMA, {
            state: "any",
            signal_type: "Query",
            decay_half_life_hours: 168,
            limit: 50,
            response_format: "markdown",
          }),
        );
        expect(textOf(result)).toContain("Query");
      });

      it("reports total outbound weight", async () => {
        const result = await db.readOnly((tx) =>
          ntlTools.listSynapses(tx, TEST_SCHEMA, {
            state: "any",
            decay_half_life_hours: 168,
            limit: 50,
            response_format: "json",
          }),
        );
        const parsed = JSON.parse(textOf(result)) as {
          total_outbound_weight: number;
        };
        expect(parsed.total_outbound_weight).toBeGreaterThan(0);
      });
    });

    describe("ntl_get_learning_health", () => {
      it("computes the ratios that say whether the model is learning", async () => {
        const result = await db.readOnly((tx) =>
          ntlTools.learningHealth(tx, TEST_SCHEMA, {
            sample: 1000,
            response_format: "json",
          }),
        );
        const h = JSON.parse(textOf(result)) as {
          sampled: number;
          exploration_ratio: number;
          pending_ratio: number;
          delivery_ratio: number;
          outcomes: Record<string, number>;
          warnings: string[];
        };
        expect(h.sampled).toBe(7);
        expect(h.outcomes["delivered"]).toBe(3);
        expect(h.outcomes["pending"]).toBe(1);
        expect(h.outcomes["rejected"]).toBe(1);
        expect(h.outcomes["timed_out"]).toBe(1);
        // 1 of 7 seeded decisions was exploratory.
        expect(h.exploration_ratio).toBeCloseTo(1 / 7, 3);
        expect(h.pending_ratio).toBeCloseTo(1 / 7, 3);
        expect(h.delivery_ratio).toBeCloseTo(3 / 7, 3);
      });

      it("says so plainly when there is nothing to report", async () => {
        // A separate schema on the same connection, so this cannot disturb the
        // schema the enclosing suite built.
        const emptySchema = "ntl_empty";
        await resetSchema(db, emptySchema);
        await sqlTools.initSchema(db, emptySchema, NTL_SCHEMA_SQL, { confirm: true });

        const asJson = await db.readOnly((tx) =>
          ntlTools.learningHealth(tx, emptySchema, {
            sample: 100,
            response_format: "json",
          }),
        );
        const h = JSON.parse(textOf(asJson)) as { sampled: number };
        expect(h.sampled).toBe(0);

        // The explanation lives in the markdown rendering; the json form is
        // the structured data.
        const asMarkdown = await db.readOnly((tx) =>
          ntlTools.learningHealth(tx, emptySchema, {
            sample: 100,
            response_format: "markdown",
          }),
        );
        expect(textOf(asMarkdown)).toMatch(/cannot learn|not routed/i);
      });
    });

    describe("ntl_list_journal", () => {
      it("lists decisions and filters by outcome", async () => {
        const all = await db.readOnly((tx) =>
          ntlTools.listJournal(tx, TEST_SCHEMA, {
            outcome: "any",
            explored_only: false,
            limit: 50,
            response_format: "json",
          }),
        );
        expect((JSON.parse(textOf(all)) as { count: number }).count).toBe(7);

        const delivered = await db.readOnly((tx) =>
          ntlTools.listJournal(tx, TEST_SCHEMA, {
            outcome: "delivered",
            explored_only: false,
            limit: 50,
            response_format: "json",
          }),
        );
        const parsed = JSON.parse(textOf(delivered)) as {
          decisions: { outcome: string }[];
        };
        expect(parsed.decisions.length).toBe(3);
        for (const d of parsed.decisions) expect(d.outcome).toBe("delivered");
      });

      it("filters to exploratory decisions", async () => {
        const result = await db.readOnly((tx) =>
          ntlTools.listJournal(tx, TEST_SCHEMA, {
            outcome: "any",
            explored_only: true,
            limit: 50,
            response_format: "json",
          }),
        );
        const parsed = JSON.parse(textOf(result)) as {
          decisions: { explored: boolean }[];
        };
        expect(parsed.decisions.length).toBe(1);
        expect(parsed.decisions[0]?.explored).toBe(true);
      });

      it("filters by signal type", async () => {
        const result = await db.readOnly((tx) =>
          ntlTools.listJournal(tx, TEST_SCHEMA, {
            outcome: "any",
            signal_type: "Query",
            explored_only: false,
            limit: 50,
            response_format: "json",
          }),
        );
        const parsed = JSON.parse(textOf(result)) as {
          decisions: { signal_type: string }[];
        };
        expect(parsed.decisions.length).toBeGreaterThan(0);
        for (const d of parsed.decisions) expect(d.signal_type).toBe("Query");
      });
    });

    describe("ntl_get_node_status", () => {
      it("summarises identity, topology and activation", async () => {
        // Also pins the BIGINT-as-string contract, which differed between
        // drivers until it was normalised at the driver boundary.
        const result = await db.readOnly((tx) =>
          ntlTools.nodeStatus(tx, TEST_SCHEMA, { response_format: "json" }),
        );
        const s = JSON.parse(textOf(result)) as {
          node_id: string;
          schema_version: number;
          synapses: { total: number; eligible: number };
          peers_by_source: { source: string; n: number }[];
          // BIGINT is a string on both backends by design — it exceeds
          // Number.MAX_SAFE_INTEGER and NTL stores nanosecond timestamps in it.
          activation: { signals_fired: string } | null;
          live_dedup_entries: number;
        };
        expect(s.node_id).toBe("a1b2c3d4e5f6");
        expect(s.schema_version).toBe(1);
        expect(s.synapses.total).toBe(5);
        // active + active + weakening; dormant and pruned are not eligible.
        expect(s.synapses.eligible).toBe(3);
        expect(s.peers_by_source.length).toBe(4);
        expect(s.activation?.signals_fired).toBe("137");
        // One of the two seeded dedup entries has already expired.
        expect(s.live_dedup_entries).toBe(1);
      });
    });

    // ---------------------------------------------------------------- ops

    describe("ntl_get_advisors", () => {
      it("runs without error and returns structured findings", async () => {
        const result = await db.readOnly((tx) =>
          opsTools.getAdvisors(tx, { category: "all", response_format: "json" }),
        );
        expect(result.isError).toBeFalsy();
        const parsed = JSON.parse(textOf(result)) as {
          findings: { level: string; remediation: string }[];
          by_level: Record<string, number>;
        };
        expect(Array.isArray(parsed.findings)).toBe(true);
        // Every finding must be actionable — a finding without a remediation
        // is noise.
        for (const f of parsed.findings) {
          expect(f.remediation.length).toBeGreaterThan(10);
        }
      });

      it("flags a table with no primary key", async () => {
        await db.query(`CREATE TABLE IF NOT EXISTS ${TEST_SCHEMA}.no_pk (v text)`);
        const result = await db.readOnly((tx) =>
          opsTools.getAdvisors(tx, {
            category: "performance",
            response_format: "json",
          }),
        );
        const parsed = JSON.parse(textOf(result)) as {
          findings: { check: string; detail: string }[];
        };
        const found = parsed.findings.find(
          (f) => f.check === "missing_primary_key" && f.detail.includes("no_pk"),
        );
        expect(found, "should have flagged the primary-key-less table").toBeDefined();
      });

      it("flags a table granted to PUBLIC", async () => {
        await db.query(`CREATE TABLE IF NOT EXISTS ${TEST_SCHEMA}.leaky (id int PRIMARY KEY)`);
        await db.query(`GRANT SELECT ON ${TEST_SCHEMA}.leaky TO PUBLIC`);
        const result = await db.readOnly((tx) =>
          opsTools.getAdvisors(tx, { category: "security", response_format: "json" }),
        );
        const parsed = JSON.parse(textOf(result)) as {
          findings: { check: string; detail: string }[];
        };
        const found = parsed.findings.find(
          (f) => f.check === "public_table_grant" && f.detail.includes("leaky"),
        );
        expect(found, "should have flagged the PUBLIC grant").toBeDefined();
      });

      it("separates security from performance categories", async () => {
        const security = await db.readOnly((tx) =>
          opsTools.getAdvisors(tx, { category: "security", response_format: "json" }),
        );
        const parsed = JSON.parse(textOf(security)) as {
          findings: { category: string }[];
        };
        for (const f of parsed.findings) expect(f.category).toBe("security");
      });
    });

    describe("ntl_get_activity", () => {
      it("reports connections and notes whether pg_stat_statements exists", async () => {
        const result = await db.readOnly((tx) =>
          opsTools.getActivity(tx, { include_idle: true, response_format: "json" }),
        );
        expect(result.isError).toBeFalsy();
        const parsed = JSON.parse(textOf(result)) as {
          pg_stat_statements_installed: boolean;
        };
        expect(typeof parsed.pg_stat_statements_installed).toBe("boolean");
      });
    });

    describe("ntl_list_extensions", () => {
      it("lists installed extensions", async () => {
        const result = await db.readOnly((tx) =>
          opsTools.getActivity(tx, { include_idle: false, response_format: "json" }),
        );
        expect(result.isError).toBeFalsy();

        const ext = await db.readOnly((tx) =>
          schemaTools.listExtensions(tx, { response_format: "json" }),
        );
        const parsed = JSON.parse(textOf(ext)) as {
          installed: { name: string }[];
        };
        // plpgsql is installed in every stock Postgres.
        expect(parsed.installed.some((e) => e.name === "plpgsql")).toBe(true);
      });
    });
  });
}

describe("ntl_search_docs", () => {
  it("finds relevant documentation", () => {
    const result = opsTools.searchDocs({ query: "influence cap", limit: 5 });
    const parsed = result.structuredContent as { results: { title: string }[] };
    expect(parsed.results.length).toBeGreaterThan(0);
    expect(parsed.results.map((r) => r.title)).toContain("Learning Model");
  });

  it("ranks the threat model first for a security query", () => {
    const result = opsTools.searchDocs({ query: "sybil attack poisoning", limit: 3 });
    const parsed = result.structuredContent as { results: { title: string }[] };
    expect(parsed.results[0]?.title).toBe("Threat Model");
  });

  it("lists available topics when nothing matches", () => {
    const result = opsTools.searchDocs({ query: "zzzz nonexistent", limit: 5 });
    expect(textOf(result)).toContain("Available topics");
  });

  it("respects the limit", () => {
    const result = opsTools.searchDocs({ query: "openntl signal storage", limit: 2 });
    const parsed = result.structuredContent as { results: unknown[] };
    expect(parsed.results.length).toBeLessThanOrEqual(2);
  });
});
