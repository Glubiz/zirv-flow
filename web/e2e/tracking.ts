/** What stands where a tracked address goes in a message that was not sent to a person. */
export const TRACKED = "TRACKED";

/** The inert addresses a preview uses, which are not on the public URL. */
const INERT_CLICK_BASE = "https://preview.sample.invalid/t";
const INERT_UNSUBSCRIBE = "https://unsubscribe.sample.invalid/u";

export interface TrackingForms {
  /** The public URL the server builds tracking links on, such as `https://flow.example.test`. */
  publicUrl: string;
  /**
   * Addresses a link goes to in a test send. A test send rewrites no links (ruling 1d3048b4) while a
   * preview points each one at an inert tracking address, so an `href` holding exactly one of these
   * is the same link as the preview's.
   */
  direct: readonly string[];
}

/** `value` as a pattern that also reads a `/` the renderer wrote as `&#x2f;`. */
function pattern(value: string): string {
  return value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&").replace(/\//g, "(?:/|&#x2f;)");
}

/**
 * Maps the tracking forms of a message to one canonical `TRACKED` and nothing else:
 * - the click and open token paths under `<publicUrl>/t/` and the unsubscribe token under `<publicUrl>/u/`;
 * - the inert preview base `https://preview.sample.invalid/t` and the inert unsubscribe address;
 * - the `direct` addresses, inside `href` only;
 * - the open pixel (an `img` whose `src` is a tracking address), which is dropped.
 */
export function normaliseTracking(message: string, forms: TrackingForms): string {
  const rest = `[^\\s"'<>]*`;
  const tracked = [
    `${pattern(forms.publicUrl)}${pattern("/t/")}${rest}`,
    `${pattern(forms.publicUrl)}${pattern("/u/")}${rest}`,
    `${pattern(INERT_CLICK_BASE)}(?:${pattern("/")}${rest})?`,
    pattern(INERT_UNSUBSCRIBE),
  ].join("|");
  const pixel = new RegExp(`<img\\b[^>]*\\bsrc=["'](?:${tracked})["'][^>]*>`, "g");
  const direct = forms.direct.map((address) => pattern(address)).join("|");
  let out = message.replace(pixel, "").replace(new RegExp(tracked, "g"), TRACKED);
  if (direct !== "") out = out.replace(new RegExp(`(href=["'])(?:${direct})(?=["'])`, "g"), `$1${TRACKED}`);
  return out;
}

/** A message part with its line ends as the web reads them, and no trailing line ends. Spaces and tabs stay. */
export function asLines(part: string): string {
  return part.replace(/\r\n/g, "\n").replace(/\n+$/, "");
}

/** One tag per line, so a diff of minified markup points at the tag that differs. */
function diffLines(text: string): string[] {
  return text.replace(/></g, ">\n<").split("\n");
}

const CONTEXT = 3;

/** A unified diff of `expected` against `actual`, or the empty string when they are equal. */
export function unifiedDiff(expected: string, actual: string): string {
  if (expected === actual) return "";
  const a = diffLines(expected);
  const b = diffLines(actual);
  const width = b.length + 1;
  const table = new Array<number>((a.length + 1) * width).fill(0);
  const at = (x: number, y: number) => table[x * width + y] ?? 0;
  for (let i = a.length - 1; i >= 0; i -= 1) {
    for (let j = b.length - 1; j >= 0; j -= 1) {
      table[i * width + j] = a[i] === b[j] ? at(i + 1, j + 1) + 1 : Math.max(at(i + 1, j), at(i, j + 1));
    }
  }
  const ops: string[] = [];
  let i = 0;
  let j = 0;
  while (i < a.length || j < b.length) {
    if (i < a.length && j < b.length && a[i] === b[j]) {
      ops.push(` ${a[i]}`);
      i += 1;
      j += 1;
    } else if (j < b.length && (i === a.length || at(i, j + 1) >= at(i + 1, j))) {
      ops.push(`+${b[j]}`);
      j += 1;
    } else {
      ops.push(`-${a[i]}`);
      i += 1;
    }
  }
  const shown = ops.map((op, at) => op[0] !== " " || ops.slice(Math.max(0, at - CONTEXT), at + CONTEXT + 1).some((near) => near[0] !== " "));
  const out = ["--- expected", "+++ actual"];
  ops.forEach((op, at) => {
    if (shown[at]) out.push(op);
    else if (shown[at - 1]) out.push("...");
  });
  return out.join("\n");
}
