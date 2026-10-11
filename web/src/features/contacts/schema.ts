import { client, unwrap, useResource, type components, type Resource } from "../../api";
import { useWorkspaceAccess } from "../members";

export type Contact = components["schemas"]["ContactBody"];
export type DefinitionBody = components["schemas"]["ContactDefinitionBody"];
export type DefinitionField = components["schemas"]["ContactFieldBody"];
export type Trait = components["schemas"]["TraitBody"];

/** A field of the contact definition with what the field registry says about its key. */
export interface SchemaField {
  key: string;
  required: boolean;
  /** `email`, `phone`, `anonymous_id`, `external:<name>`, or null for an ordinary field. */
  identifier: string | null;
  /** The registry type: `string`, `number`, `boolean`, `timestamp`, `date` or `string_list`. */
  type: string;
  pii: boolean;
}

export interface ContactSchema {
  /** False while the workspace has saved no definition: contacts carry only a key. */
  defined: boolean;
  fields: SchemaField[];
  /** Every live registry field, for the definition editor. */
  registry: Trait[];
  updatedAt: string | null;
}

export function toSchema(definition: DefinitionBody, traits: Trait[]): ContactSchema {
  return {
    defined: definition.defined,
    registry: traits.filter((trait) => !trait.archived),
    updatedAt: definition.updated_at ?? null,
    fields: definition.fields.map((field) => {
      const trait = traits.find((candidate) => candidate.key === field.key);
      return {
        key: field.key,
        required: field.required ?? false,
        identifier: field.identifier ?? null,
        type: trait?.type ?? "string",
        pii: trait?.pii ?? false,
      };
    }),
  };
}

export function useContactSchema(workspaceId: string): Resource<ContactSchema> {
  return useResource(async () => {
    const [definition, traits] = await Promise.all([
      unwrap(client.GET("/api/v1/workspaces/{ws}/contact-definition", { params: { path: { ws: workspaceId } } })),
      unwrap(client.GET("/api/v1/workspaces/{ws}/traits", { params: { path: { ws: workspaceId } } })),
    ]);
    return toSchema(definition, traits);
  }, [workspaceId]);
}

export interface ContactAccess {
  /** Create, edit and delete contacts: any member, admin or the owner. */
  canWrite: boolean;
  /** Edit the contact definition: workspace admins and the owner. */
  canDefine: boolean;
}

export function useContactAccess(workspaceId: string): ContactAccess {
  const access = useWorkspaceAccess(workspaceId);
  if (access.status !== "ready") {
    return { canWrite: false, canDefine: false };
  }
  return {
    canWrite: access.data.canManage || access.data.role !== null,
    canDefine: access.data.canManage,
  };
}

export const IDENTIFIER_KINDS: { value: string; label: string }[] = [
  { value: "email", label: "Email address" },
  { value: "phone", label: "Phone number" },
  { value: "anonymous_id", label: "Anonymous ID" },
  { value: "external", label: "ID from another system" },
];

export function identifierLabel(identifier: string): string {
  if (identifier.startsWith("external:")) return `${identifier.slice("external:".length)} ID`;
  return IDENTIFIER_KINDS.find((kind) => kind.value === identifier)?.label ?? identifier;
}

/** A stored value as text. */
export function showValue(value: unknown): string {
  if (value === null || value === undefined) return "";
  if (typeof value === "boolean") return value ? "Yes" : "No";
  if (Array.isArray(value)) return value.map(String).join(", ");
  return typeof value === "string" ? value : JSON.stringify(value);
}

const REASONS: Record<string, string> = {
  missing_required: "This field is required.",
  wrong_type: "This value is not the right type for the field.",
  unplanned_property: "This field is not part of the contact definition.",
  invalid_key: "This is not a valid field key.",
  invalid_email: "Enter a valid email address.",
  invalid_phone: "Enter a phone number with its country code, such as +4512345678.",
  invalid_identifier_value: "This value cannot be used as an identifier.",
  archived_key: "This field is archived.",
  too_long: "This value is too long.",
  unknown_key: "This field is not in the field registry.",
  identifier_needs_string: "An identifier field must be a text field.",
  required: "This is required.",
  empty: "Add at least one contact.",
  too_many: "There are too many contacts in one request.",
};

export function reasonText(reason: string): string {
  return REASONS[reason] ?? `Not accepted (${reason}).`;
}
