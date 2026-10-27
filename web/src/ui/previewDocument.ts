const PREVIEW_CSP = "default-src 'none'; img-src https: data:; style-src 'unsafe-inline'; base-uri 'none'; form-action 'none'";
export const PREVIEW_CSP_META = `<meta http-equiv="Content-Security-Policy" content="${PREVIEW_CSP}">`;

/** Put the preview policy first in the head without rewriting the server-rendered HTML. */
export function previewDocument(html: string): string {
  const tags = [...html.matchAll(/<!--[\s\S]*?-->|<(head|html)(?=[\s/>])(?:[^>"']|"[^"]*"|'[^']*')*>/gi)];
  const head = tags.find((tag) => tag[1]?.toLowerCase() === "head");
  const root = head ?? tags.find((tag) => tag[1]?.toLowerCase() === "html");
  if (root) {
    const at = root.index + root[0].length;
    const policy = head ? PREVIEW_CSP_META : `<head>${PREVIEW_CSP_META}</head>`;
    return `${html.slice(0, at)}${policy}${html.slice(at)}`;
  }
  return `<!doctype html><html><head>${PREVIEW_CSP_META}</head><body>${html}</body></html>`;
}
