import type { Tone } from "../../ui";

export interface StatusLabel {
  label: string;
  tone: Tone;
}

const ERASURE: Record<string, StatusLabel> = {
  pending: { label: "Waiting to run", tone: "attention" },
  completed: { label: "Erased", tone: "neutral" },
  failed: { label: "Failed", tone: "error" },
};

const EXPORT: Record<string, StatusLabel> = {
  pending: { label: "Preparing", tone: "attention" },
  ready: { label: "Ready", tone: "neutral" },
  failed: { label: "Failed", tone: "error" },
  expired: { label: "Expired", tone: "neutral" },
};

export function erasureStatus(status: string): StatusLabel {
  return ERASURE[status] ?? { label: status, tone: "neutral" };
}

export function exportStatus(status: string): StatusLabel {
  return EXPORT[status] ?? { label: status, tone: "neutral" };
}

export const IDENTIFIER_KINDS: { value: string; label: string }[] = [
  { value: "email", label: "Email address" },
  { value: "phone", label: "Phone number" },
  { value: "anonymous_id", label: "Anonymous id" },
  { value: "external", label: "External id" },
];

export const ORPHAN_POLICIES: { value: string; label: string; help: string }[] = [
  {
    value: "keep",
    label: "Keep the person",
    help: "A person whose last record is retracted stays, with their traits recomputed.",
  },
  {
    value: "delete",
    label: "Delete the person",
    help: "A person left with no source record, event or explicit consent is deleted with everything that refers to them. This adds no suppression.",
  },
];

/** The phrase a person types to confirm an erasure. */
export const CONFIRM_PHRASE = "ERASE";

/** Seconds until `expiresAt`, never below zero. */
export function secondsLeft(expiresAt: string, now: number): number {
  const end = new Date(expiresAt).getTime();
  return Number.isNaN(end) ? 0 : Math.max(0, Math.floor((end - now) / 1000));
}
