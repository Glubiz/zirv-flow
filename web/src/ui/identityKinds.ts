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
