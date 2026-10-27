import type { components } from "../../api";
import { BLOCKS_SCHEMA, emptyContent, readContent, type BlocksContent } from "./blocks/document";

export type Template = components["schemas"]["TemplateBody"];
export type TemplateDetail = components["schemas"]["TemplateDetailBody"];
export type TemplateIssue = components["schemas"]["TemplateIssueProblem"];
export type TemplatePreview = components["schemas"]["TemplatePreviewBody"];
export type TemplateUse = components["schemas"]["TemplateUseBody"];
export type TemplateVersionSummary = components["schemas"]["TemplateVersionSummaryBody"];
export type TemplateVersion = components["schemas"]["TemplateVersionBody"];
export type TestSend = components["schemas"]["TestSendBody"];

/** How a template is written. Blocks is built by the block editor. */
export type EditMode = "html" | "mjml" | "blocks";

export const MODES: readonly { id: EditMode; label: string; hint: string; available: boolean }[] = [
  { id: "html", label: "HTML", hint: "Write the HTML and the text part yourself.", available: true },
  { id: "mjml", label: "MJML", hint: "Write MJML; the server turns it into responsive HTML.", available: true },
  { id: "blocks", label: "Blocks", hint: "Build the message from blocks.", available: true },
];

export interface EmailDraft {
  subject: string;
  sender: string;
  lang: string;
  source: string;
  text: string;
}

export interface WebhookDraft {
  headers: { name: string; value: string }[];
  body: string;
}

/** The part of a template document this editor changes, with the rest kept as it was read. */
export type Editable =
  | { kind: "email"; mode: "html" | "mjml"; email: EmailDraft; rest: Record<string, unknown> }
  | { kind: "webhook"; webhook: WebhookDraft; rest: Record<string, unknown> }
  | { kind: "blocks"; content: BlocksContent; /** The document is of a newer schema than this editor knows: it is shown, not changed. */ newer: boolean; rest: Record<string, unknown> }
  | { kind: "unsupported"; mode: string; rest: Record<string, unknown> };

function record(value: unknown): Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value) ? (value as Record<string, unknown>) : {};
}

function str(value: unknown): string {
  return typeof value === "string" ? value : "";
}

/** Reads a stored document into the fields the editor shows. */
export function readDocument(document: unknown): Editable {
  const doc = record(document);
  const email = record(doc["email"]);
  const webhook = record(doc["webhook"]);
  if (doc["mode"] === "code" && doc["channel"] === "email" && doc["email"] !== undefined) {
    return {
      kind: "email",
      mode: email["syntax"] === "mjml" ? "mjml" : "html",
      email: {
        subject: str(email["subject"]),
        sender: str(email["sender"]),
        lang: str(email["lang"]),
        source: str(email["html"]),
        text: str(email["text"]),
      },
      rest: doc,
    };
  }
  if (doc["mode"] === "blocks" && doc["channel"] === "email" && typeof doc["blocks"] === "object" && doc["blocks"] !== null) {
    const schema = doc["schema"];
    const content = readContent(doc["blocks"]);
    if (schema === 1) {
      // A schema 1 document renders from its own settings, with no theme and no footer.
      content.theme ??= "none";
      content.footer ??= "none";
    }
    return { kind: "blocks", content, newer: typeof schema === "number" && schema > BLOCKS_SCHEMA, rest: doc };
  }
  if (doc["mode"] === "code" && doc["channel"] === "webhook" && doc["webhook"] !== undefined) {
    const headers = record(webhook["headers"]);
    return {
      kind: "webhook",
      webhook: {
        headers: Object.entries(headers).map(([name, value]) => ({ name, value: str(value) })),
        body: str(webhook["body"]),
      },
      rest: doc,
    };
  }
  return { kind: "unsupported", mode: str(doc["mode"]) || "unknown", rest: doc };
}

/** The document to save for the fields as edited. */
export function writeDocument(editable: Editable): Record<string, unknown> {
  if (editable.kind === "email") {
    const before = record(editable.rest["email"]);
    const kept = Object.fromEntries(Object.entries(before).filter(([key]) => key !== "sender" && key !== "lang" && key !== "syntax"));
    const email: Record<string, unknown> = {
      ...kept,
      subject: editable.email.subject,
      html: editable.email.source,
      text: editable.email.text,
    };
    if (editable.email.sender !== "") email["sender"] = editable.email.sender;
    if (editable.email.lang !== "") email["lang"] = editable.email.lang;
    if (editable.mode === "mjml") email["syntax"] = "mjml";
    return { ...editable.rest, email };
  }
  if (editable.kind === "blocks") {
    return editable.newer ? editable.rest : { ...editable.rest, schema: BLOCKS_SCHEMA, blocks: editable.content };
  }
  if (editable.kind === "webhook") {
    const headers: Record<string, string> = {};
    for (const header of editable.webhook.headers) {
      if (header.name.trim() !== "") headers[header.name.trim()] = header.value;
    }
    return { ...editable.rest, webhook: { ...record(editable.rest["webhook"]), headers, body: editable.webhook.body } };
  }
  return editable.rest;
}

/** The generated client types a document as an object with no keys; this is the one place that says otherwise. */
export function asBody(document: Record<string, unknown>): Record<string, never> {
  return document as Record<string, never>;
}

/** The document a new template starts with, or null for the server's own empty one. */
export function startingDocument(
  channel: "email" | "webhook",
  mode: EditMode,
  category: "marketing" | "transactional",
): Record<string, unknown> | null {
  if (channel === "webhook") return null;
  if (mode === "blocks") {
    return { schema: BLOCKS_SCHEMA, channel: "email", mode: "blocks", blocks: emptyContent(category) };
  }
  return {
    schema: 1,
    channel: "email",
    mode: "code",
    email: { subject: "", html: "", text: "", category, ...(mode === "mjml" ? { syntax: "mjml" } : {}) },
  };
}

export function modeLabel(editable: Editable): string {
  if (editable.kind === "webhook") return "Webhook template";
  if (editable.kind === "blocks") return "Blocks template";
  if (editable.kind === "email") return editable.mode === "mjml" ? "MJML template" : "HTML template";
  return "Blocks template";
}

export function isBlocking(issue: TemplateIssue): boolean {
  return issue.severity === "blocking";
}

export function blockingCount(issues: readonly TemplateIssue[]): number {
  return issues.filter(isBlocking).length;
}

/** The issues whose pointer is `at` or below it. */
export function issuesAt(issues: readonly TemplateIssue[], at: string): TemplateIssue[] {
  return issues.filter((issue) => issue.at === at || issue.at.startsWith(`${at}/`));
}

const ISSUE_TEXT: Record<string, string> = {
  unknown_merge_field: "is not a field in this workspace.",
  missing_unsubscribe: "A marketing email needs an unsubscribe link. Add {{ system.unsubscribe_url }} to a link.",
  missing_sender: "Choose the sender identity this message goes out from.",
  missing_content: "The template has no content yet.",
  syntax_error: "The merge tags have a syntax error.",
  mjml_parse_error: "The MJML could not be parsed.",
  mjml_include: "MJML includes are not allowed.",
  mjml_too_deep: "The MJML is nested too deeply.",
  invalid_json_body: "The body is not valid JSON.",
  dynamic_merge_field: "Merge tags must name a field directly.",
  content_too_large: "The content is too large.",
  body_too_large: "The body is too large.",
  invalid_header_name: "A header name is not valid.",
  too_many_headers: "There are too many headers.",
  headers_too_large: "The headers are too large.",
  missing_lang: "Set the language of the message.",
  insecure_link: "A link uses http instead of https.",
  html_near_clip: "The HTML is close to the size some mail clients cut off.",
  subject_too_long: "The subject is longer than most inboxes show.",
  preheader_too_long: "The preheader is longer than most inboxes show.",
  sample_render_failed: "A sample render failed, so some checks did not run.",
  url_from_merge_field: "A link address comes from a merge tag.",
  invalid_setting: "A style setting is not valid.",
  invalid_columns: "A section holds one to three columns.",
  duplicate_block_id: "Two blocks share an id.",
  invalid_block_id: "A block id is not valid.",
  invalid_url: "A web address is not valid. Use https, http or mailto.",
  image_without_alt: "An image has no alt text. Screen readers skip it.",
  markup_in_plain_field: "A plain text field holds markup.",
  blocks_limit: "The email has more sections or blocks than the editor allows.",
  mjml_in_raw_html: "Raw HTML cannot hold MJML tags.",
  sanitizer_rejected: "Some of the text is not allowed in rich text.",
};

/** What an issue says, in words. */
export function issueText(issue: TemplateIssue): string {
  const base = ISSUE_TEXT[issue.code];
  if (issue.code === "unknown_merge_field") {
    return `${issue.field ?? "A merge tag"} ${base ?? ""}`.trim();
  }
  const detail = issue.message ? ` ${issue.message}` : "";
  return `${base ?? `${issue.code.replaceAll("_", " ")}.`}${detail}`;
}

/** Which editor tab an issue is in, in words. */
export function issueWhere(issue: TemplateIssue): string {
  const at = issue.at;
  if (at.startsWith("/email/subject")) return "Subject";
  if (at.startsWith("/email/html")) return "Source";
  if (at.startsWith("/email/text")) return "Text";
  if (at.startsWith("/email/sender") || at.startsWith("/email/lang")) return "Settings";
  if (at.startsWith("/blocks/block/")) return "Block";
  if (at.startsWith("/blocks/settings")) return "Document style";
  if (/^\/blocks\/(subject|preheader|sender|lang)/.test(at)) return "Message";
  if (at.startsWith("/webhook/headers")) return "Headers";
  if (at.startsWith("/webhook/body")) return "Body";
  return "Template";
}

/** The document with another subject line; a webhook or an unknown document has none. */
export function withSubject(editable: Editable, subject: string): Editable {
  if (editable.kind === "email") return { ...editable, email: { ...editable.email, subject } };
  if (editable.kind === "blocks") return { ...editable, content: { ...editable.content, subject } };
  return editable;
}

/** The status badge of a template: always words. */
export function statusOf(template: Template): { label: string; tone: "neutral" | "attention" | "error" } {
  if (template.archived_at) return { label: "Archived", tone: "error" };
  if (template.current_version === null || template.current_version === undefined) {
    return { label: "Draft, never published", tone: "attention" };
  }
  return { label: `Published v${template.current_version}`, tone: "neutral" };
}

/** The values of the theme in effect that a template may read. */
export const MERGE_THEME_VALUES: readonly { label: string; expression: string }[] = [
  { label: "Primary colour", expression: "theme.primary" },
  { label: "Text colour", expression: "theme.text" },
  { label: "Background colour", expression: "theme.background" },
  { label: "Link colour", expression: "theme.link" },
  { label: "Button colour", expression: "theme.button" },
  { label: "Button text colour", expression: "theme.button_text" },
  { label: "Logo address", expression: "theme.logo_url" },
  { label: "Theme name", expression: "theme.name" },
];

export const MERGE_SYSTEM_VALUES: readonly { label: string; expression: string }[] = [
  { label: "Unsubscribe link", expression: "system.unsubscribe_url" },
  { label: "Email preferences link", expression: "system.preferences_url" },
];
