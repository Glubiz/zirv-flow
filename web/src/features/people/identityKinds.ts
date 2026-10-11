export function kindLabel(kind: string): string {
  switch (kind) {
    case "email":
      return "Email address";
    case "phone":
      return "Phone number";
    case "anonymous_id":
      return "Anonymous ID";
    default:
      return kind.startsWith("external:") ? `External ID (${kind.slice("external:".length)})` : kind;
  }
}

export function reasonLabel(reason: string): string {
  switch (reason) {
    case "unique_kind":
      return "One per person";
    case "max_per_person":
      return "Too many for one person";
    case "merge_deferred":
      return "Not merged";
    default:
      return reason;
  }
}

export function reasonExplanation(reason: string, kind: string): string {
  switch (reason) {
    case "unique_kind":
      return `Merging these people would give one person two different values of a kind that a person holds only once. The ${kindLabel(kind)} is on both of them.`;
    case "max_per_person":
      return `Merging these people would give one person more identifiers of one kind than a person may hold. The ${kindLabel(kind)} is on both of them.`;
    default:
      return `These people share a ${kindLabel(kind)} but were not merged.`;
  }
}

export function resolutionLabel(resolution: string | null | undefined): string {
  switch (resolution) {
    case "merged":
      return "Merged";
    case "kept_separate":
      return "Kept separate";
    case "identifier_moved":
      return "Identifier moved";
    default:
      return "Open";
  }
}
