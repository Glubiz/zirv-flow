import type { components } from "../../api";
import type { Tone } from "../../ui";

export type CsvImport = components["schemas"]["CsvImportBody"];
export type CsvImportDetail = components["schemas"]["CsvImportDetailBody"];

export function statusLabel(status: string): string {
  switch (status) {
    case "uploading":
      return "Uploading";
    case "uploaded":
      return "Uploaded";
    case "mapped":
      return "Mapped";
    case "dry_running":
      return "Dry run in progress";
    case "dry_run_done":
      return "Dry run done";
    case "running":
      return "Importing";
    case "succeeded":
      return "Imported";
    case "failed":
      return "Failed";
    case "cancelled":
      return "Cancelled";
    default:
      return status;
  }
}

export function statusTone(status: string): Tone {
  switch (status) {
    case "failed":
      return "error";
    case "dry_running":
    case "running":
      return "attention";
    default:
      return "neutral";
  }
}

export function isActive(status: string): boolean {
  return status === "dry_running" || status === "running";
}

/** Why a row was refused, in words. The codes are the API's; none holds a value from the file. */
export function reasonText(reason: string): string {
  switch (reason) {
    case "missing_identifier":
      return "no identifier in the row";
    case "invalid_identifier":
      return "the identifier is not valid for its kind";
    case "wrong_type":
      return "the value does not fit the field's type";
    case "bad_timestamp":
      return "the date or time could not be read with its format";
    case "unknown_column":
      return "the mapping names a column this row does not have";
    case "invalid_key":
      return "the key column's value is longer than 512 characters";
    case "apply_failed":
      return "the row could not be saved";
    default:
      return reason;
  }
}

/** Why an import failed, in words. */
export function errorText(code: string | null | undefined): string {
  switch (code) {
    case "reject_threshold":
      return "Too many rows were rejected, so the import stopped. Rows applied before it stopped stay applied.";
    case "list_not_found":
      return "A list in the mapping no longer exists. Rows applied before it stopped stay applied.";
    case "file_missing":
      return "The uploaded file is no longer stored. Files are kept for 7 days after an import ends.";
    case "file_unreadable":
      return "The stored file could not be read.";
    case "no_mapping":
    case "bad_header":
      return "The import has no usable mapping.";
    default:
      return "The import could not finish.";
  }
}

export function percent(done: number, total: number): number {
  if (total <= 0) return done > 0 ? 100 : 0;
  return Math.min(100, Math.floor((done / total) * 100));
}

export function plural(count: number, one: string, many: string): string {
  return `${num(count)} ${count === 1 ? one : many}`;
}

export function rejectsUrl(workspaceId: string, importId: string, phase?: "dry_run" | "run"): string {
  const base = `/api/v1/workspaces/${workspaceId}/imports/${importId}/rejects.csv`;
  return phase ? `${base}?phase=${phase}` : base;
}

/** A count with thousands separators, the same wherever the page is opened. */
export function num(value: number): string {
  return value.toLocaleString("en-US");
}
