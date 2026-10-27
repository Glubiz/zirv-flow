import type { Change, ChangeTag } from "./diff";

/** Block and section ids are `[A-Za-z0-9_-]`; anything else never reaches a selector. */
const SAFE_ID = /^[A-Za-z0-9_-]+$/;

const COLOR: Record<ChangeTag, string> = { Added: "#2e9d5b", Changed: "#d99a1e", Removed: "#d02c2a" };

/** The `<style>` that outlines the marked sections and blocks of an annotated render: green added, amber changed, red removed. */
export function outlineStyle(changes: readonly Change[]): string {
  const rules = changes.flatMap((change) =>
    change.markId !== undefined && SAFE_ID.test(change.markId)
      ? [`.zf-block-${change.markId}{outline:2px solid ${COLOR[change.tag]};outline-offset:4px;border-radius:3px}`]
      : [],
  );
  return rules.length === 0 ? "" : `<style>${rules.join("")}</style>`;
}

/** The rendered HTML with the outline style added to its head. The frame that shows it has no sandbox permission, so this adds no script. */
export function withOutlines(html: string, changes: readonly Change[]): string {
  const style = outlineStyle(changes);
  if (style === "") return html;
  return /<\/head>/i.test(html) ? html.replace(/<\/head>/i, () => `${style}</head>`) : `${style}${html}`;
}
