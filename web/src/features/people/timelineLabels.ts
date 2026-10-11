const ACTIONS: Record<string, string> = {
  "person.created": "Person created",
  "person.merge": "Merged with another person",
  "person.merge.rewrite": "Merge finished",
  "trait.change": "Fields changed",
  "consent.change": "Consent changed",
  "list.membership": "List membership changed",
  "identity.conflict": "Identity conflict found",
  "identity.conflict.resolved": "Identity conflict closed",
  "identity.split": "Kept apart from another person",
  "identity.limit": "Identifier left off",
  "event.accepted": "Event accepted",
  "event.rejected": "Event rejected",
};

export function actionLabel(action: string): string {
  return ACTIONS[action] ?? action;
}

const OUTCOMES: Record<string, string> = {
  changed: "Changed",
  created: "Created",
  merged: "Merged",
  moved: "Moved",
  added: "Added",
  removed: "Removed",
  recorded: "Recorded",
  completed: "Completed",
  accepted: "Accepted",
  rejected: "Rejected",
  skipped: "Skipped",
  new_person: "New person",
  kept_separate: "Kept separate",
  identifier_moved: "Identifier moved",
};

export function outcomeLabel(outcome: string): string {
  return OUTCOMES[outcome] ?? outcome;
}

export function reasonText(reason: string): string {
  return reason.replaceAll("_", " ");
}

export function validationLabel(state: string): string {
  switch (state) {
    case "valid":
      return "Valid";
    case "flagged":
      return "Flagged";
    case "unplanned":
      return "Not in a schema";
    default:
      return state;
  }
}
