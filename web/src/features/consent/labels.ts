export const POLICIES: { value: string; label: string; help: string }[] = [
  { value: "opt_out", label: "Opt-out", help: "People receive it unless they have unsubscribed." },
  { value: "opt_in", label: "Opt-in", help: "People receive it only after they have subscribed." },
];

function labelOf(options: { value: string; label: string }[], value: string): string {
  return options.find((option) => option.value === value)?.label ?? value;
}

export function policyLabel(value: string): string {
  return labelOf(POLICIES, value);
}

export function policyHelp(value: string): string {
  return POLICIES.find((policy) => policy.value === value)?.help ?? "";
}

export const REASONS: { value: string; label: string }[] = [
  { value: "hard_bounce", label: "Hard bounce" },
  { value: "complaint", label: "Complaint" },
  { value: "manual", label: "Added manually" },
  { value: "erased", label: "Erased" },
];

export function reasonLabel(value: string): string {
  return labelOf(REASONS, value);
}

export const KINDS: { value: string; label: string }[] = [
  { value: "email", label: "Email address" },
  { value: "phone", label: "Phone number" },
];

export function kindLabel(value: string): string {
  return labelOf(KINDS, value);
}

const ROW_PROBLEMS: Record<string, string> = {
  malformed_row: "The row does not have a kind and a value",
  unsupported_kind: "The kind is not email or phone",
  invalid_email: "Not a valid email address",
  invalid_phone: "Not a valid international phone number",
  note_too_long: "The note is over 500 characters",
  note_contains_value: "The note contains the address or number",
};

export function rowProblemLabel(code: string): string {
  return ROW_PROBLEMS[code] ?? code;
}

export const STATUSES: { value: string; label: string }[] = [
  { value: "subscribed", label: "Subscribed" },
  { value: "unsubscribed", label: "Unsubscribed" },
  { value: "pending_confirmation", label: "Pending confirmation" },
];

export function statusLabel(value: string): string {
  return STATUSES.find((status) => status.value === value)?.label ?? value;
}
