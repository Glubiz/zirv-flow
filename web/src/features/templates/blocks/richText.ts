/**
 * The client-side clean pass for rich text. It mirrors the server allowlist in
 * `crates/zirv-flow-templates/src/content/blocks/sanitize.rs`: paragraphs, bold, italic, underline,
 * links, lists and line breaks; the only attribute is `href`, and it is `https`, `http`, `mailto`
 * or starts with a merge tag. The server stays the authority; this keeps the toolbar from
 * emitting anything it would drop. Source is parsed in an inert document, never in the page.
 */

const DROP_WITH_CONTENT = new Set(["script", "style", "template", "noscript"]);
const BLOCKS = new Set(["p", "ul", "ol", "div", "h1", "h2", "h3", "h4", "h5", "h6", "blockquote", "pre", "section", "article", "table", "tr"]);
const MERGE = /(\{\{[\s\S]*?\}\}|\{%[\s\S]*?%\})/;

/** Whether a link target may stay: an allowed scheme, or a merge tag first. */
export function linkAllowed(url: string): boolean {
  const value = url.trim();
  if (value.startsWith("{{") || value.startsWith("{%")) return true;
  return /^(https|http|mailto):/i.test(value);
}

function escapeText(text: string): string {
  return text.replaceAll("&", "&amp;").replaceAll("<", "&lt;").replaceAll(">", "&gt;").replaceAll(" ", "&nbsp;");
}

function escapeAttribute(text: string): string {
  return text.replaceAll("&", "&amp;").replaceAll('"', "&quot;").replaceAll("<", "&lt;").replaceAll(">", "&gt;");
}

/** Text with its merge tags left exactly as written: only the text around them is escaped. */
function serializeText(text: string): string {
  return text
    .split(MERGE)
    .map((part, index) => (index % 2 === 1 ? part : escapeText(part)))
    .join("");
}

function inline(node: Node): string {
  if (node.nodeType === 3) return serializeText(node.textContent ?? "");
  if (node.nodeType !== 1) return "";
  const element = node as Element;
  const name = element.tagName.toLowerCase();
  if (DROP_WITH_CONTENT.has(name)) return "";
  const inner = Array.from(element.childNodes).map(inline).join("");
  if (name === "br") return "<br>";
  if (name === "a") {
    const href = element.getAttribute("href");
    return href !== null && linkAllowed(href) ? `<a href="${escapeAttribute(href.trim())}">${inner}</a>` : inner;
  }
  if (name === "b" || name === "strong" || name === "i" || name === "em" || name === "u") return inner === "" ? "" : `<${name}>${inner}</${name}>`;
  return inner;
}

function list(element: Element): string {
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
      items.push(`<li>${Array.from(child.childNodes).map(inline).join("")}</li>`);
    } else {
      loose += inline(child);
    }
  }
  flush();
  return items.length === 0 ? "" : `<${name}>${items.join("")}</${name}>`;
}

function blocks(nodes: Node[]): string {
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
      out.push(list(element));
    } else if (element && BLOCKS.has(name)) {
      flush();
      const content = Array.from(element.childNodes);
      if (content.some((child) => child.nodeType === 1 && BLOCKS.has((child as Element).tagName.toLowerCase()))) {
        out.push(blocks(content));
      } else {
        const text = content.map(inline).join("");
        if (text.replaceAll("<br>", "").trim() !== "") out.push(`<p>${text}</p>`);
      }
    } else {
      run += inline(node);
    }
  }
  flush();
  return out.join("");
}

/** The rich text in `html` reduced to the allowlist. Empty input gives an empty string. */
export function cleanRichText(html: string): string {
  if (html.trim() === "") return "";
  const parsed = new DOMParser().parseFromString(`<body>${html}</body>`, "text/html");
  return blocks(Array.from(parsed.body.childNodes));
}
