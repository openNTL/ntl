/**
 * Response formatting.
 *
 * Every tool supports both `markdown` (default, for a human or an agent
 * reading prose) and `json` (for an agent that will process the result). The
 * character cap is enforced here rather than per-tool, so no tool can forget
 * it: a tool that returns a million-row table does not help an agent, it
 * exhausts the context the agent needs to reason with.
 */

import { CHARACTER_LIMIT } from "./constants.js";
import type { Row } from "./types.js";

/**
 * A tool result, ready to hand back to the MCP SDK.
 *
 * The index signature is required by the SDK's `CallToolResult`, which allows
 * arbitrary extra fields for forward compatibility.
 */
export interface ToolOutput {
  [key: string]: unknown;
  content: { type: "text"; text: string }[];
  structuredContent?: Record<string, unknown>;
  isError?: boolean;
}

/**
 * Truncate text at the character cap, saying so explicitly.
 *
 * Silent truncation is the failure mode to avoid: an agent that believes it
 * saw a whole table will draw conclusions from a fragment.
 */
export function capText(text: string): string {
  if (text.length <= CHARACTER_LIMIT) return text;
  const kept = text.slice(0, CHARACTER_LIMIT);
  return (
    `${kept}\n\n---\n` +
    `**Output truncated** at ${CHARACTER_LIMIT.toLocaleString()} characters ` +
    `(${text.length.toLocaleString()} total). This is not the whole result. ` +
    `Narrow the query — add a WHERE clause, select fewer columns, or lower ` +
    `\`limit\` — rather than assuming what was cut.`
  );
}

/** Render rows as a GitHub-flavoured Markdown table. */
export function rowsToMarkdown(rows: Row[], columns?: string[]): string {
  if (rows.length === 0) return "_No rows._";

  const cols = columns ?? [...new Set(rows.flatMap((r) => Object.keys(r)))];
  if (cols.length === 0) return "_No columns._";

  const cell = (value: unknown): string => {
    if (value === null || value === undefined) return "_null_";
    if (value instanceof Uint8Array) {
      // Byte arrays are node and signal identifiers. Full hex would be
      // unreadable at 32 bytes, so show a recognisable prefix.
      const hex = Array.from(value.slice(0, 8))
        .map((b) => b.toString(16).padStart(2, "0"))
        .join("");
      return `\`${hex}…\` (${value.length}B)`;
    }
    if (typeof value === "object") return `\`${JSON.stringify(value)}\``;
    // Pipes would break the table; escape rather than mangle.
    return String(value).replace(/\|/g, "\\|").replace(/\n/g, " ");
  };

  const header = `| ${cols.join(" | ")} |`;
  const divider = `| ${cols.map(() => "---").join(" | ")} |`;
  const body = rows
    .map((row) => `| ${cols.map((c) => cell(row[c])).join(" | ")} |`)
    .join("\n");

  return [header, divider, body].join("\n");
}

/** Build a successful tool result. */
export function ok(
  markdown: string,
  structured?: Record<string, unknown>,
): ToolOutput {
  return {
    content: [{ type: "text", text: capText(markdown) }],
    ...(structured ? { structuredContent: structured } : {}),
  };
}

/**
 * Build a tool result in the caller's requested format.
 */
export function respond(
  format: "markdown" | "json",
  markdown: string,
  structured: Record<string, unknown>,
): ToolOutput {
  const text = format === "json" ? JSON.stringify(structured, null, 2) : markdown;
  return {
    content: [{ type: "text", text: capText(text) }],
    structuredContent: structured,
  };
}

/**
 * Build an error result.
 *
 * MCP errors are returned as content with `isError`, not thrown, so the agent
 * sees the message and can act on it. Every message here should say what to do
 * next, not merely what went wrong.
 */
export function fail(message: string, hint?: string): ToolOutput {
  const text = hint ? `${message}\n\n${hint}` : message;
  return {
    content: [{ type: "text", text: capText(text) }],
    isError: true,
  };
}

/**
 * Convert a thrown value into an actionable error result.
 *
 * Postgres errors carry more than a message — `detail` and `hint` are often
 * where the actionable part lives, and dropping them makes the agent guess.
 */
export function failFromError(error: unknown, context: string): ToolOutput {
  if (typeof error === "object" && error !== null) {
    const e = error as {
      message?: string;
      code?: string;
      detail?: string;
      hint?: string;
      position?: string;
    };
    const parts = [`${context}: ${e.message ?? String(error)}`];
    if (e.code) parts.push(`SQLSTATE: ${e.code}`);
    if (e.detail) parts.push(`Detail: ${e.detail}`);
    if (e.hint) parts.push(`Hint: ${e.hint}`);
    if (e.position) parts.push(`Position: character ${e.position}`);
    return fail(parts.join("\n"));
  }
  return fail(`${context}: ${String(error)}`);
}

/** Serialise a row so `JSON.stringify` can handle Postgres types. */
export function jsonSafe(rows: Row[]): Row[] {
  return rows.map((row) => {
    const out: Row = {};
    for (const [key, value] of Object.entries(row)) {
      if (value instanceof Uint8Array) {
        // Hex, because these are identifiers an agent may want to match on and
        // base64 is harder to compare against psql output.
        out[key] = Array.from(value)
          .map((b) => b.toString(16).padStart(2, "0"))
          .join("");
      } else if (typeof value === "bigint") {
        // BIGINT exceeds Number.MAX_SAFE_INTEGER; a string keeps it exact.
        out[key] = value.toString();
      } else {
        out[key] = value;
      }
    }
    return out;
  });
}
