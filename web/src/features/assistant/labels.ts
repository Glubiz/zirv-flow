import type { components } from "../../api";
import { ApiError } from "../../api";

export type Draft = components["schemas"]["DraftBody"];
export type DraftChange = components["schemas"]["Change"];
export type DryRun = components["schemas"]["DryRunBody"];
export type DraftIssue = components["schemas"]["DraftIssue"];

/** What a validator code means, in a sentence. The wording of the mapping and manifest editors, kept in step by hand. */
const ISSUE_TEXT: Record<string, string> = {
  arguments_invalid: "The answer was not in the shape the draft needs.",
  required: "A required setting is missing here.",
  invalid_type: "This has the wrong kind of value, for example text where a number or a list is expected.",
  invalid_value: "This is not one of the values allowed here.",
  out_of_range: "This number or list size is outside what is allowed.",
  invalid_length: "This text is empty or too long.",
  invalid_url: "This is not an allowed address. It must be a full https address without a user name or fragment.",
  unknown_field: "This setting does not exist in a manifest.",
  expression_not_supported: "Expressions and scripts are not supported.",
  unsupported_version: 'Only manifest version "v1" is supported.',
  manifest_too_large: "The manifest is larger than 64 KiB.",
  too_many_streams: "A manifest can describe at most 20 streams.",
  too_many_fields: "A stream can declare at most 200 fields.",
  too_deep: "The manifest is nested too deeply.",
  too_many_headers: "At most 10 fixed headers are allowed.",
  forbidden_header: "This header is set by zirv-flow itself and cannot be fixed in a manifest.",
  invalid_header: "This header name or value is not allowed.",
  too_many_query: "A stream can have at most 20 fixed query parameters.",
  path_invalid: "A stream path must start with / and have no //, .., ? or # in it.",
  invalid_pointer: 'This must be a JSON pointer such as /data/items, or "" for a top-level list.',
  duplicate_stream: "Two streams have the same name.",
  duplicate_field: "Two fields of this stream have the same name.",
  invalid_field_name: "A field name cannot be blank.",
  primary_key_empty: "A stream needs at least one primary key field.",
  primary_key_unknown_field: "The primary key names a field the stream does not declare.",
  cursor_field_unknown: "The cursor field is not one of the stream's declared fields.",
  invalid_mapping: "The mapping is not in the shape the mapping editor saves.",
  list_target_not_allowed: "A manifest cannot add people to a list.",
  no_identifier: "The mapping needs at least one column that becomes an identifier.",
  too_many_rules: "The mapping has too many rules.",
  unknown_column: "The mapping uses a field that the stream does not have.",
  unknown_list: "The mapping uses a list that does not exist in this workspace.",
  invalid_identifier_kind: "The mapping gives an identifier a kind that does not exist.",
  invalid_trait_key: "The mapping uses a field key that is not a valid key.",
  unknown_trait: "The mapping uses a field of your account that does not exist.",
  archived_trait: "The mapping uses a field of your account that has been archived.",
  duplicate_trait: "Two columns of the mapping fill the same field.",
  invalid_transform: "A step in the mapping is not valid.",
  invalid_event_name: "The event name in the mapping is not valid.",
  invalid_event_property: "An event property in the mapping is not valid.",
  duplicate_event_property: "Two event properties in the mapping have the same name.",
  too_many_event_properties: "The mapping's event has too many properties.",
  event_requires_primary_key: "An event needs the stream to have a primary key.",
  key_column_not_supported: "A webhook stream has no key column.",
  delete_signal_unsupported: "A webhook stream has no delete signal.",
  blank_header: "A field name of the sample is blank.",
  duplicate_header: "Two fields of the sample have the same name.",
};

/** A problem of a draft as a sentence, with the place it concerns when there is one. */
export function issueSentence(issue: DraftIssue): string {
  const text = ISSUE_TEXT[issue.code] ?? "The answer broke a rule that the draft must follow.";
  return issue.at ? `${text} (${issue.at})` : text;
}

const DRY_RUN_REASONS: Record<string, string> = {
  source_not_saved:
    "The source is not saved yet, so the draft was not tried against the API. Save the source, then draft again to see a sample.",
  connection_changed:
    "The draft changes the base address or the sign-in, so it was not tried with the saved credentials. Save those changes first.",
};

export function dryRunReason(reason: string | null | undefined): string {
  return DRY_RUN_REASONS[reason ?? ""] ?? "The draft could not be tried.";
}

export interface Failure {
  title: string;
  detail: string;
  traceId: string | null;
}

/** What went wrong with the whole request, as a title and a detail. */
export function failureText(error: unknown): Failure {
  if (error instanceof ApiError) {
    const traceId = error.traceId;
    switch (error.code) {
      case "ai_budget_exceeded":
        return { traceId, title: "The monthly AI budget is spent", detail: "Nothing was sent. An admin can raise the budget in the AI settings." };
      case "ai_rate_limited":
        return { traceId, title: "Too many AI requests", detail: "Wait a minute, then try again." };
      case "ai_disabled":
        return { traceId, title: "The AI assistant is off", detail: "An admin can turn it on in the AI settings." };
      case "ai_provider_error":
        return { traceId, title: "The model could not be reached", detail: "The provider failed or answered with something unusable. Try again in a moment." };
      case "ai_user_required":
        return { traceId, title: "Sign in as a person to use the assistant", detail: "An API key cannot draft." };
      case "ai_draft_outdated":
        return { traceId, title: "The saved version changed", detail: "Someone saved a change after this draft was made. Draft again." };
      case "ai_draft_invalid":
        return { traceId, title: "This draft cannot be accepted", detail: "It did not pass validation." };
      default:
    }
    if (error.status === 404) {
      return { traceId, title: "Not found", detail: "The source, the stream or the draft is gone, or you cannot see it." };
    }
    return { traceId, title: error.title, detail: error.detail };
  }
  return {
    title: "Could not reach the server",
    detail: error instanceof Error ? error.message : "Check your connection and try again.",
    traceId: null,
  };
}

/** A JSON pointer as a readable place: the keys, joined. */
export function readablePath(pointer: string): string {
  if (pointer === "") return "The whole draft";
  return pointer
    .split("/")
    .slice(1)
    .map((part) => part.replaceAll("~1", "/").replaceAll("~0", "~"))
    .join(" › ");
}

export function changeLabel(change: DraftChange["change"]): string {
  return change === "added" ? "Added" : change === "removed" ? "Removed" : "Changed";
}

/** A value for a table cell: text as it is, anything else as JSON, shortened. */
export function valueText(value: unknown): string {
  if (value === null || value === undefined) return "";
  const text = typeof value === "string" ? value : JSON.stringify(value);
  return text.length > 120 ? `${text.slice(0, 117)}…` : text;
}

const PREVIEW_REASONS: Record<string, string> = {
  missing_identifier: "no identifier had a value",
  invalid_identifier: "the identifier is not valid",
  wrong_type: "the value does not fit its type",
  bad_timestamp: "the date or time could not be read",
  unknown_column: "the stream has no such field",
  mapping_unavailable: "the stream had no usable mapping",
  invalid_key: "the key is too long",
};

export function previewReason(reason: string): string {
  return PREVIEW_REASONS[reason] ?? reason;
}
