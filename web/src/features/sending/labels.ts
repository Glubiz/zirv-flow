import type { components } from "../../api";

export type SendingDomain = components["schemas"]["SendingDomainBody"];
export type DomainRecord = components["schemas"]["DomainRecordBody"];
export type SenderIdentity = components["schemas"]["SenderIdentityBody"];

export type RecordStatus = "pending" | "ok" | "mismatch" | "missing";

export interface StatusView {
  status: RecordStatus;
  /** The word that leads the badge, so colour is never the only signal. */
  word: string;
  /** The rest of the sentence. */
  reason: string;
  /** The class of the badge (`st st-...`). */
  className: string;
}

export function recordStatus(status: string): StatusView {
  switch (status) {
    case "ok":
      return { status: "ok", word: "OK", reason: "matches", className: "st st-ok" };
    case "mismatch":
      return { status: "mismatch", word: "Mismatch", reason: "a different value is published", className: "st st-fail" };
    case "missing":
      return { status: "missing", word: "Missing", reason: "no record at this host", className: "st st-skip" };
    default:
      return { status: "pending", word: "Pending", reason: "not checked yet", className: "st st-defer" };
  }
}

export interface KindText {
  title: string;
  purpose: string;
}

export const RECORD_KINDS: Record<string, KindText> = {
  dkim: { title: "DKIM", purpose: "Signs your mail with this domain's key" },
  spf: { title: "SPF", purpose: "Lets receivers check that your relay may send for this domain" },
  dmarc: { title: "DMARC", purpose: "Tells receivers what to do when a check fails" },
};

export function kindText(kind: string): KindText {
  return RECORD_KINDS[kind] ?? { title: kind.toUpperCase(), purpose: "" };
}

export function matching(records: readonly DomainRecord[]): number {
  return records.filter((record) => record.status === "ok").length;
}

/** The sentence under the domain's name: verified, or how far from it. */
export function verifiedText(domain: SendingDomain): string {
  if (domain.status === "verified") return "Verified";
  return `Not verified · ${matching(domain.records)} of ${domain.records.length} records match`;
}

const SPF_PREFIX = /^v=spf1(\s|$)/i;

/**
 * The SPF record to publish when the host already has one: a name can hold only one `v=spf1`
 * record, so the mechanisms we expect are merged into the observed record, before its final `all`.
 * Null when there is nothing to merge into, or nothing to add.
 */
export function mergedSpf(observed: readonly string[], expected: string): string | null {
  const current = observed.find((text) => SPF_PREFIX.test(text.trim()));
  if (!current) return null;
  const have = current.trim().split(/\s+/);
  const wanted = spfMechanisms(expected);
  const missing = wanted.filter((term) => !have.includes(term));
  if (missing.length === 0) return null;
  const last = have[have.length - 1] ?? "";
  const tail = /^[-~+?]?all$/i.test(last) ? [last] : [];
  const body = tail.length > 0 ? have.slice(0, -1) : have;
  return [...body, ...missing, ...tail].join(" ");
}

/** The mechanisms the expected SPF value adds, for the sentence beside the merge. */
export function spfMechanisms(expected: string): string[] {
  return expected
    .trim()
    .split(/\s+/)
    .slice(1)
    .filter((term) => !/^[-~+?]?all$/i.test(term));
}

/** Why an identity cannot send, or null when it can. */
export function blockedReason(domain: SendingDomain): string | null {
  return domain.status === "verified" ? null : "domain not verified";
}

