/**
 * The Worker's 500 responses must not carry error text, stack traces or
 * connection details back to the caller (CodeQL js/stack-trace-exposure).
 * The detail goes to the Worker log, tied to the response by a requestId.
 */

import { WebStandardStreamableHTTPServerTransport } from "@modelcontextprotocol/sdk/server/webStandardStreamableHttp.js";
import { afterEach, describe, expect, it, vi } from "vitest";

import worker from "../src/index.js";
import type { Env } from "../src/types.js";

const TOKEN = "test-token-0123456789";

function request(): Request {
  return new Request("https://mcp.test/mcp", {
    method: "POST",
    headers: { Authorization: `Bearer ${TOKEN}`, "Content-Type": "application/json" },
    body: JSON.stringify({ jsonrpc: "2.0", id: 1, method: "tools/list" }),
  });
}

describe("500 responses do not expose error detail", () => {
  afterEach(() => vi.restoreAllMocks());

  it("a missing database binding is reported without the configuration text", async () => {
    const log = vi.spyOn(console, "error").mockImplementation(() => {});
    const res = await worker.fetch(request(), { MCP_AUTH_TOKEN: TOKEN } as unknown as Env);
    expect(res.status).toBe(500);
    const body = (await res.json()) as { error: string; message: string; requestId: string };
    expect(body.error).toBe("server_misconfigured");
    expect(body.message).not.toMatch(/HYPERDRIVE|DATABASE_URL/);
    expect(body.requestId).toMatch(/^[0-9a-f-]{36}$/);
    // The reason is still available to the operator, in the log.
    expect(String(log.mock.calls[0]?.[0])).toContain("DATABASE_URL");
    expect(String(log.mock.calls[0]?.[0])).toContain(body.requestId);
  });

  it("an error thrown while serving is reported without its message or stack", async () => {
    const log = vi.spyOn(console, "error").mockImplementation(() => {});
    vi.spyOn(WebStandardStreamableHTTPServerTransport.prototype, "handleRequest").mockRejectedValue(
      new Error("connection refused: postgres://user:hunter2@db.internal:5432/ntl"),
    );
    const env = {
      MCP_AUTH_TOKEN: TOKEN,
      DATABASE_URL: "postgres://user:hunter2@db.internal:5432/ntl",
    } as unknown as Env;
    const res = await worker.fetch(request(), env);
    expect(res.status).toBe(500);
    const text = await res.text();
    expect(text).toContain("internal_error");
    expect(text).not.toContain("hunter2");
    expect(text).not.toContain("db.internal");
    expect(text).not.toContain("at "); // no stack frames
    expect(String(log.mock.calls[0]?.[0])).toContain("db.internal");
  });
});
