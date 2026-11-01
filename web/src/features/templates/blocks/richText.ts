/**
 * The client-side clean pass for rich text. It mirrors the server allowlist in
 * `crates/zirv-flow-templates/src/content/blocks/sanitize.rs`: paragraphs, bold, italic, underline,
 * links, lists and line breaks; the attributes are `href`, which is `https`, `http`, `mailto`
 * or starts with a merge tag (`{{`), and `data-zf-utm="off"`, the opt-out from UTM tagging. The
 * server stays the authority; this keeps the toolbar from emitting anything it would drop. Source is parsed in an inert document, never in the page.
 *
 * Merge tags are not HTML. In the stored value they pass through exactly as written, as on the
 * server. In anything that goes into the editor's DOM they are escaped, so markup inside a tag
 * (`{{<img onerror=...>}}`) is only ever text there.
 */

const DROP_WITH_CONTENT = new Set(["script", "style", "template", "noscript"]);
const BLOCKS = new Set(["p", "ul", "ol", "div", "h1", "h2", "h3", "h4", "h5", "h6", "blockquote", "pre", "section", "article", "table", "tr"]);
const MERGE = /\{\{[\s\S]*?\}\}|\{%[\s\S]*?%\}/g;
const HOLDER = /(\d+)/g;

/** The one data attribute a link keeps, and the one value it keeps it with. */
export const UTM_ATTRIBUTE = "data-zf-utm";

/** Whether a link target may stay: an allowed scheme, or a merge tag (`{{`) first, as the server reads it. */
export function linkAllowed(url: string): boolean {
  const value = url.trim();
  return value.startsWith("{{") || /^(https|http|mailto):/i.test(value);
}

function escapeText(text: string): string {
  return text.replaceAll("&", "&amp;").replaceAll("<", "&lt;").replaceAll(">", "&gt;").replaceAll(" ", "&nbsp;");
}

function escapeAttribute(text: string): string {
  return text.replaceAll("&", "&amp;").replaceAll('"', "&quot;").replaceAll("<", "&lt;").replaceAll(">", "&gt;");
}

interface Context {
  tags: string[];
  /** True for HTML that goes into the editor's DOM: merge tags are escaped there. */
  dom: boolean;
}

/** Puts the lifted merge tags back as text. */
function unlift(text: string, context: Context): string {
  return text.replace(HOLDER, (_whole, index: string) => context.tags[Number(index)] ?? "");
}

/** Text with its merge tags: as written in the stored value, escaped like the rest in the DOM. */
function escaping(text: string, context: Context, escape: (text: string) => string): string {
  if (context.dom) return escape(text);
  let out = "";
  let at = 0;
  for (const match of text.matchAll(MERGE)) {
    out += escape(text.slice(at, match.index)) + match[0];
    at = match.index + match[0].length;
  }
  return out + escape(text.slice(at));
}

function plain(text: string, context: Context): string {
  return escaping(unlift(text, context), context, escapeText);
}

function inline(node: Node, context: Context): string {
  if (node.nodeType === 3) return plain(node.textContent ?? "", context);
  if (node.nodeType !== 1) return "";
  const element = node as Element;
  const name = element.tagName.toLowerCase();
  if (DROP_WITH_CONTENT.has(name)) return "";
  const inner = Array.from(element.childNodes)
    .map((child) => inline(child, context))
    .join("");
  if (name === "br") return "<br>";
  if (name === "a") {
    const href = element.getAttribute("href");
    if (href === null) return inner;
    const raw = unlift(href, context).trim();
    if (!linkAllowed(raw)) return inner;
    const optOut = element.getAttribute(UTM_ATTRIBUTE)?.trim().toLowerCase() === "off" ? ` ${UTM_ATTRIBUTE}="off"` : "";
    return `<a href="${escaping(raw, context, escapeAttribute)}"${optOut}>${inner}</a>`;
  }
  if (name === "b" || name === "strong" || name === "i" || name === "em" || name === "u") return inner === "" ? "" : `<${name}>${inner}</${name}>`;
  return inner;
}

function list(element: Element, context: Context): string {
  const name = element.tagName.toLowerCase();
  const items: string[] = [];
  let loose = "";
  const flush = () => {
    if (loose.trim() !== "") items.push(`<li>${loose}</li>`);
    loose = "";
  };
  for (const child of Array.from(element.childNodes)) {
    if (child.nodeType === 1 && (child as Element).tagName.toLowerCase() === "li") {
      flush();
      items.push(`<li>${Array.from(child.childNodes).map((inner) => inline(inner, context)).join("")}</li>`);
    } else {
      loose += inline(child, context);
    }
  }
  flush();
  return items.length === 0 ? "" : `<${name}>${items.join("")}</${name}>`;
}

function blocks(nodes: Node[], context: Context): string {
  const out: string[] = [];
  let run = "";
  const flush = () => {
    if (run.replaceAll("<br>", "").trim() !== "") out.push(`<p>${run}</p>`);
    run = "";
  };
  for (const node of nodes) {
    const element = node.nodeType === 1 ? (node as Element) : null;
    const name = element?.tagName.toLowerCase() ?? "";
    if (element && DROP_WITH_CONTENT.has(name)) continue;
    if (element && (name === "ul" || name === "ol")) {
      flush();
      out.push(list(element, context));
    } else if (element && BLOCKS.has(name)) {
      flush();
      const content = Array.from(element.childNodes);
      if (content.some((child) => child.nodeType === 1 && BLOCKS.has((child as Element).tagName.toLowerCase()))) {
        out.push(blocks(content, context));
      } else {
        const text = content.map((child) => inline(child, context)).join("");
        if (text.replaceAll("<br>", "").trim() !== "") out.push(`<p>${text}</p>`);
      }
    } else {
      run += inline(node, context);
    }
  }
  flush();
  return out.join("");
}

function clean(html: string, dom: boolean): string {
  if (html.trim() === "") return "";
  // Merge tags are lifted out before the parse, so markup inside one is never parsed as markup.
  const tags: string[] = [];
  const lifted = html.replace(MERGE, (tag) => `${tags.push(tag) - 1}`);
  const parsed = new DOMParser().parseFromString(`<body>${lifted}</body>`, "text/html");
  return blocks(Array.from(parsed.body.childNodes), { tags, dom });
}

/** The rich text in `html` reduced to the allowlist, as the value that is stored. Empty input gives an empty string. */
export function cleanRichText(html: string): string {
  return clean(html, false);
}

/** The same text as HTML that is safe to give to the editor's DOM: merge tags are escaped and shown as text. */
export function richTextToEditor(html: string): string {
  return clean(html, true);
}

/** The stored value of what the editor's DOM holds. Its text nodes are text, so a merge tag in one is read as written. */
export function richTextFromEditor(root: Element): string {
  return blocks(Array.from(root.childNodes), { tags: [], dom: false });
}

/** Clean HTML to insert into the editor's DOM, as nodes of the page, from pasted or dropped HTML. */
export function editorNodes(html: string): Node[] {
  const parsed = new DOMParser().parseFromString(`<body>${richTextToEditor(html)}</body>`, "text/html");
  return Array.from(parsed.body.childNodes).map((node) => document.importNode(node, true));
}
