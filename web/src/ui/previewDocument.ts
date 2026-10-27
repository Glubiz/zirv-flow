const PREVIEW_CSP = "default-src 'none'; img-src https: data:; style-src 'unsafe-inline'; base-uri 'none'; form-action 'none'";

/** Establish the policy before the browser parses any of the server-rendered HTML. */
export function previewDocument(html: string): string {
  return `<!doctype html><html><head><meta http-equiv="Content-Security-Policy" content="${PREVIEW_CSP}"></head><body>${html}</body></html>`;
}
