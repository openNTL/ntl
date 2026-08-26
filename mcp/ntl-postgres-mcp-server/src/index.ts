/**
 * Cloudflare Worker entry point.
 *
 * Serves MCP over streamable HTTP, stateless. Stateless matters on Workers:
 * an isolate can be evicted between requests, so a server holding session
 * state would lose it unpredictably. Each request carries everything needed to
 * serve it.
 *
 * ## Copying this template
 *
 * Three things to change for a different database:
 *
 * 1. `db.ts` — implement `SqlExecutor` for your driver.
 * 2. `src/tools/` — replace the domain tools with yours. The schema, SQL and
 *    ops tools are largely portable.
 * 3. `wrangler.toml` — swap the Hyperdrive binding for whatever your database
 *    needs.
 *
 * The auth, read-only enforcement, formatting and error handling below are
 * database-agnostic and worth keeping as they are.
 */

// The web-standard transport takes a `Request` and returns a `Response`,
// which is exactly the Worker shape. The Node transport expects
// IncomingMessage/ServerResponse and does not exist here.
import { WebStandardStreamableHTTPServerTransport } from "@modelcontextprotocol/sdk/server/webStandardStreamableHttp.js";

import { DEFAULT_MAX_ROWS, DEFAULT_STATEMENT_TIMEOUT_MS } from "./constants.js";
import { PostgresExecutor } from "./db.js";
import { createServer } from "./server.js";
import type { Env, ServerConfig } from "./types.js";

/** Timing-safe string comparison, so a token cannot be recovered byte by byte. */
function timingSafeEqual(a: string, b: string): boolean {
  const encoder = new TextEncoder();
  const aBytes = encoder.encode(a);
  const bBytes = encoder.encode(b);
  // Comparing lengths first leaks length, which is acceptable and unavoidable;
  // what matters is that content comparison takes constant time.
  if (aBytes.length !== bBytes.length) return false;
  let diff = 0;
  for (let i = 0; i < aBytes.length; i++) {
    diff |= (aBytes[i] ?? 0) ^ (bBytes[i] ?? 0);
  }
  return diff === 0;
}

/** Resolve configuration from Worker bindings, failing loudly if unusable. */
function resolveConfig(env: Env): ServerConfig {
  // Hyperdrive first: it pools connections outside the isolate, and without
  // pooling a traffic burst exhausts Postgres' max_connections long before it
  // exhausts anything else.
  const databaseUrl = env.HYPERDRIVE?.connectionString ?? env.DATABASE_URL;
  if (!databaseUrl) {
    throw new Error(
      "No database configured. Bind a Hyperdrive instance as HYPERDRIVE, or " +
        "set DATABASE_URL for local development.",
    );
  }

  const schema = env.NTL_SCHEMA ?? "ntl";
  if (!/^[A-Za-z_][A-Za-z0-9_$]*$/.test(schema) || schema.length > 63) {
    throw new Error(
      `NTL_SCHEMA=${JSON.stringify(schema)} is not a valid Postgres identifier.`,
    );
  }

  return {
    databaseUrl,
    // Off unless explicitly enabled. An MCP server holding database
    // credentials should not be able to mutate anything by default.
    allowWrites: env.ALLOW_WRITES === "true",
    schema,
    statementTimeoutMs: DEFAULT_STATEMENT_TIMEOUT_MS,
    maxRows: DEFAULT_MAX_ROWS,
  };
}

const CORS_HEADERS = {
  "Access-Control-Allow-Origin": "*",
  "Access-Control-Allow-Methods": "POST, GET, OPTIONS",
  "Access-Control-Allow-Headers": "Content-Type, Authorization, Mcp-Session-Id",
  "Access-Control-Max-Age": "86400",
} as const;

function json(body: unknown, status: number): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "Content-Type": "application/json", ...CORS_HEADERS },
  });
}

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    if (request.method === "OPTIONS") {
      return new Response(null, { status: 204, headers: CORS_HEADERS });
    }

    const url = new URL(request.url);

    // An unauthenticated health check, so a load balancer does not need a token.
    // It deliberately reports nothing about the database.
    if (url.pathname === "/health") {
      return json({ status: "ok", server: "ntl-postgres-mcp-server" }, 200);
    }

    let config: ServerConfig;
    try {
      config = resolveConfig(env);
    } catch (error) {
      // A misconfiguration is the operator's problem, not the caller's, and
      // saying so plainly saves an hour of guessing.
      return json(
        {
          error: "server_misconfigured",
          message: error instanceof Error ? error.message : String(error),
        },
        500,
      );
    }

    // Auth. Required rather than optional: this server holds database
    // credentials, and an unauthenticated deployment is an open SQL console.
    if (!env.MCP_AUTH_TOKEN) {
      return json(
        {
          error: "server_misconfigured",
          message:
            "MCP_AUTH_TOKEN is not set. This server will not serve requests " +
            "without one — it holds database credentials, and running it open " +
            "would expose a SQL console to the internet. Set it with " +
            "`wrangler secret put MCP_AUTH_TOKEN`.",
        },
        500,
      );
    }

    const header = request.headers.get("Authorization") ?? "";
    const presented = header.startsWith("Bearer ") ? header.slice(7) : "";
    if (!timingSafeEqual(presented, env.MCP_AUTH_TOKEN)) {
      return json(
        {
          error: "unauthorized",
          message: "Provide a valid bearer token in the Authorization header.",
        },
        401,
      );
    }

    if (url.pathname !== "/mcp" && url.pathname !== "/") {
      return json(
        { error: "not_found", message: `No handler for ${url.pathname}. Use /mcp.` },
        404,
      );
    }

    const db = new PostgresExecutor(config.databaseUrl, {
      statementTimeoutMs: config.statementTimeoutMs,
    });

    try {
      const server = createServer(db, config);
      // Stateless: no session id generation, because an evicted isolate would
      // lose the session and the client would see it vanish mid-conversation.
      const transport = new WebStandardStreamableHTTPServerTransport({
        enableJsonResponse: true,
      });

      await server.connect(transport);
      const response = await transport.handleRequest(request);

      const headers = new Headers(response.headers);
      for (const [key, value] of Object.entries(CORS_HEADERS)) {
        headers.set(key, value);
      }
      return new Response(response.body, { status: response.status, headers });
    } catch (error) {
      return json(
        {
          error: "internal_error",
          message: error instanceof Error ? error.message : String(error),
        },
        500,
      );
    } finally {
      // Always release the connection. A Worker isolate can be evicted at any
      // point, and a leaked connection outlives the request that opened it.
      await db.close().catch(() => {});
    }
  },
};
