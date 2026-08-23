/**
 * End-to-end MCP protocol tests.
 *
 * The tool tests call handler functions directly. These go through the real MCP
 * client, transport and server, so they also exercise schema validation, tool
 * registration, annotations, and the structured-content plumbing — the layer
 * where a tool can be perfectly correct and still unusable because its schema
 * rejects valid input.
 */

import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { InMemoryTransport } from "@modelcontextprotocol/sdk/inMemory.js";
import { afterAll, beforeAll, describe, expect, it } from "vitest";

import NTL_SCHEMA_SQL from "../src/schema/ntl.sql";
import { createServer } from "../src/server.js";
import { initSchema } from "../src/tools/sql.js";
import type { ServerConfig, SqlExecutor } from "../src/types.js";
import { backends, resetSchema, seedNtlData, TEST_SCHEMA, testConfig } from "./harness.js";

/** Connect a client to a server built over `db`. */
async function connect(
  db: SqlExecutor,
  config: ServerConfig,
): Promise<{ client: Client; close: () => Promise<void> }> {
  const server = createServer(db, config);
  const [clientTransport, serverTransport] = InMemoryTransport.createLinkedPair();
  const client = new Client({ name: "test-client", version: "1.0.0" });

  await Promise.all([
    server.connect(serverTransport),
    client.connect(clientTransport),
  ]);

  return {
    client,
    close: async () => {
      await client.close();
      await server.close();
    },
  };
}

function textOf(result: unknown): string {
  const r = result as { content?: { type: string; text?: string }[] };
  return (r.content ?? [])
    .filter((c) => c.type === "text")
    .map((c) => c.text ?? "")
    .join("\n");
}

for (const backend of backends()) {
  describe(`MCP protocol, read-only (${backend.name})`, () => {
    let db: SqlExecutor;
    let client: Client;
    let close: () => Promise<void>;

    beforeAll(async () => {
      db = await backend.create();
      await resetSchema(db, TEST_SCHEMA);
      await initSchema(db, TEST_SCHEMA, NTL_SCHEMA_SQL, { confirm: true });
      await seedNtlData(db, TEST_SCHEMA);

      ({ client, close } = await connect(db, testConfig({ allowWrites: false })));
    });

    afterAll(async () => {
      await close?.();
      await db?.close();
    });

    it("advertises the expected read-only tools", async () => {
      const { tools } = await client.listTools();
      const names = tools.map((t) => t.name).sort();

      expect(names).toEqual([
        "ntl_execute_sql",
        "ntl_generate_typescript_types",
        "ntl_get_activity",
        "ntl_get_advisors",
        "ntl_get_learning_health",
        "ntl_get_node_status",
        "ntl_list_extensions",
        "ntl_list_journal",
        "ntl_list_migrations",
        "ntl_list_synapses",
        "ntl_list_tables",
        "ntl_search_docs",
      ]);
    });

    it("omits write tools rather than registering ones that always fail", async () => {
      const { tools } = await client.listTools();
      const names = tools.map((t) => t.name);
      // Offering a tool that can only fail wastes an agent's turn and teaches
      // it nothing.
      expect(names).not.toContain("ntl_apply_migration");
      expect(names).not.toContain("ntl_init_schema");
    });

    it("gives every tool a description and annotations", async () => {
      const { tools } = await client.listTools();
      for (const tool of tools) {
        expect(tool.description, `${tool.name} needs a description`).toBeTruthy();
        expect(
          (tool.description ?? "").length,
          `${tool.name} description is too terse to be useful`,
        ).toBeGreaterThan(40);
        expect(tool.annotations, `${tool.name} needs annotations`).toBeDefined();
      }
    });

    it("marks read-only tools as read-only", async () => {
      const { tools } = await client.listTools();
      for (const tool of tools) {
        // With writes disabled, every tool including execute_sql is read-only.
        expect(
          tool.annotations?.readOnlyHint,
          `${tool.name} should be marked readOnlyHint`,
        ).toBe(true);
      }
    });

    it("says in the description that the server is read-only", async () => {
      const { tools } = await client.listTools();
      const exec = tools.find((t) => t.name === "ntl_execute_sql");
      expect(exec?.description).toMatch(/READ-ONLY/);
    });

    it("returns both text and structured content", async () => {
      const result = await client.callTool({
        name: "ntl_get_node_status",
        arguments: { response_format: "json" },
      });
      expect(textOf(result)).toContain("node_id");
      expect(result.structuredContent).toBeDefined();
    });

    it("applies schema defaults so minimal calls work", async () => {
      // An agent should not have to supply every optional argument.
      const result = await client.callTool({
        name: "ntl_list_synapses",
        arguments: {},
      });
      expect(result.isError).toBeFalsy();
      expect(textOf(result)).toContain("Synapses");
    });

    it("rejects input that violates the schema before reaching SQL", async () => {
      // The SDK surfaces validation failures as an error *result* rather than
      // throwing, which is the better contract: the agent reads the message
      // and can correct its call.
      const badState = await client.callTool({
        name: "ntl_list_synapses",
        arguments: { state: "not_a_real_state" },
      });
      expect(badState.isError).toBe(true);
      expect(textOf(badState)).toMatch(/validation|invalid/i);

      const badLimit = await client.callTool({
        name: "ntl_list_synapses",
        arguments: { limit: 99_999 },
      });
      expect(badLimit.isError).toBe(true);

      // The message should name the offending argument, or the agent has to
      // guess which one to fix.
      expect(textOf(badLimit)).toMatch(/limit/i);
    });

    it("refuses a write through the protocol", async () => {
      const result = await client.callTool({
        name: "ntl_execute_sql",
        arguments: { query: `DELETE FROM ${TEST_SCHEMA}.synapses` },
      });
      expect(result.isError).toBe(true);
      expect(textOf(result).toLowerCase()).toMatch(/read-only|read only|cannot execute/);

      // And nothing was deleted.
      const check = await db.query(`SELECT count(*)::int AS n FROM ${TEST_SCHEMA}.synapses`);
      expect(check.rows[0]?.["n"]).toBe(5);
    });

    it("exposes the reference schema as a resource", async () => {
      const { resources } = await client.listResources();
      expect(resources.map((r) => r.uri)).toContain("ntl://schema/postgres");

      const read = await client.readResource({ uri: "ntl://schema/postgres" });
      const text = String(read.contents[0]?.text ?? "");
      expect(text).toContain("CREATE TABLE IF NOT EXISTS synapses");
      // The placeholder must be substituted, or a copy-paste of this resource
      // would not run.
      expect(text).not.toContain("{{SCHEMA}}");
      expect(text).toContain(`"${TEST_SCHEMA}"`);
    });

    it("reports an unknown tool as an error rather than hanging", async () => {
      const result = await client.callTool({
        name: "ntl_does_not_exist",
        arguments: {},
      });
      expect(result.isError).toBe(true);
      expect(textOf(result)).toMatch(/not found|unknown|does_not_exist/i);
    });
  });

  describe(`MCP protocol, writes enabled (${backend.name})`, () => {
    let db: SqlExecutor;
    let client: Client;
    let close: () => Promise<void>;
    const writeSchema = "ntl_write_test";

    beforeAll(async () => {
      db = await backend.create();
      await resetSchema(db, writeSchema);
      ({ client, close } = await connect(
        db,
        testConfig({ allowWrites: true, schema: writeSchema }),
      ));
    });

    afterAll(async () => {
      await close?.();
      await db?.close();
    });

    it("advertises the write tools", async () => {
      const { tools } = await client.listTools();
      const names = tools.map((t) => t.name);
      expect(names).toContain("ntl_apply_migration");
      expect(names).toContain("ntl_init_schema");
    });

    it("marks destructive tools as destructive", async () => {
      const { tools } = await client.listTools();
      const migration = tools.find((t) => t.name === "ntl_apply_migration");
      expect(migration?.annotations?.readOnlyHint).toBe(false);
      expect(migration?.annotations?.destructiveHint).toBe(true);

      // init_schema is idempotent and additive, so it is a write but not
      // destructive. The distinction matters to a client deciding what to
      // confirm with a human.
      const init = tools.find((t) => t.name === "ntl_init_schema");
      expect(init?.annotations?.destructiveHint).toBe(false);
      expect(init?.annotations?.idempotentHint).toBe(true);
    });

    it("creates the schema end to end", async () => {
      const result = await client.callTool({
        name: "ntl_init_schema",
        arguments: { confirm: true },
      });
      expect(result.isError, textOf(result)).toBeFalsy();
      expect(textOf(result)).toContain("synapses");

      const tables = await db.query(
        `SELECT count(*)::int AS n FROM information_schema.tables WHERE table_schema = $1`,
        [writeSchema],
      );
      expect(Number(tables.rows[0]?.["n"])).toBeGreaterThan(8);
    });

    it("rejects a migration name that is not snake_case", async () => {
      const result = await client.callTool({
        name: "ntl_apply_migration",
        arguments: { name: "Bad Name!", sql: "SELECT 1" },
      });
      expect(result.isError).toBe(true);
      // The message must say what a valid name looks like, not just that this
      // one was wrong.
      expect(textOf(result)).toContain("add_synapse_region");
    });

    it("permits a write through the protocol", async () => {
      const result = await client.callTool({
        name: "ntl_execute_sql",
        arguments: {
          query: `INSERT INTO ${writeSchema}.meta (key, value) VALUES ($1, decode($2,'hex'))`,
          params: ["protocol-test", "abcdef"],
        },
      });
      expect(result.isError, textOf(result)).toBeFalsy();

      const check = await db.query(
        `SELECT count(*)::int AS n FROM ${writeSchema}.meta WHERE key = 'protocol-test'`,
      );
      expect(check.rows[0]?.["n"]).toBe(1);
    });

    it("describes execute_sql as write-enabled", async () => {
      const { tools } = await client.listTools();
      const exec = tools.find((t) => t.name === "ntl_execute_sql");
      // An agent should be able to tell from the description alone whether it
      // can write, without probing.
      expect(exec?.description).toMatch(/ENABLED/);
      expect(exec?.annotations?.readOnlyHint).toBe(false);
    });
  });
}
