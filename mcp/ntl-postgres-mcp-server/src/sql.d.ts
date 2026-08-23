/**
 * `.sql` files are imported as text.
 *
 * Wrangler is configured with a Text rule (see wrangler.toml) and vitest with
 * an equivalent transform (see vitest.config.ts), so the SQL has exactly one
 * source of truth rather than a generated TypeScript copy that could drift.
 */
declare module "*.sql" {
  const content: string;
  export default content;
}
