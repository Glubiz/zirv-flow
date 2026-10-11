import { useState, type SyntheticEvent } from "react";

import { ApiError, client, describeError, unwrap, type ErrorSummary } from "../../api";
import { Button, Dialog, InlineError, SelectField, TextField } from "../../ui";
import { identifierLabel, reasonText, type Contact, type ContactSchema, type SchemaField } from "./schema";

interface ContactFormProps {
  workspaceId: string;
  schema: ContactSchema;
  /** The contact being edited; absent when creating. */
  contact?: Contact;
  onClose: () => void;
  onSaved: (contact: Contact) => void;
}

type Values = Record<string, string>;

const LIST_SEPARATOR = ",";

/** A stored value as the text of its input. */
function toInput(field: SchemaField, value: unknown): string {
  if (value === null || value === undefined) return "";
  if (field.type === "boolean") return value === true ? "true" : "false";
  if (Array.isArray(value)) return value.map(String).join(`${LIST_SEPARATOR} `);
  return typeof value === "string" ? value : JSON.stringify(value);
}

/** The JSON value of an input's text, or an error naming what is wrong with it. */
function fromInput(field: SchemaField, text: string): { value: unknown } | { error: string } {
  switch (field.type) {
    case "number": {
      const number = Number(text);
      return text.trim() !== "" && Number.isFinite(number) ? { value: number } : { error: "Enter a number." };
    }
    case "boolean":
      return { value: text === "true" };
    case "string_list":
      return {
        value: text
          .split(LIST_SEPARATOR)
          .map((item) => item.trim())
          .filter((item) => item !== ""),
      };
    default:
      return { value: text };
  }
}

const TYPE_HINTS: Record<string, string> = {
  timestamp: "A date and time such as 2026-10-01T09:00:00Z.",
  date: "A date such as 2026-10-01.",
  string_list: "Separate the items with commas.",
  number: "A number.",
};

function labelFor(field: SchemaField): string {
  return field.required ? `${field.key} (required)` : field.key;
}

function hintFor(field: SchemaField, hidden: boolean): string | undefined {
  const parts = [
    field.identifier ? `Identifies the contact by ${identifierLabel(field.identifier).toLowerCase()}.` : null,
    TYPE_HINTS[field.type] ?? null,
    hidden ? "Hidden from you. Leave blank to keep the current value." : null,
  ].filter((part): part is string => part !== null);
  return parts.length > 0 ? parts.join(" ") : undefined;
}

export function ContactForm({ workspaceId, schema, contact, onClose, onSaved }: ContactFormProps) {
  const hiddenKeys = new Set(contact?.masked_fields ?? []);
  const initial: Values = Object.fromEntries(
    schema.fields.map((field) => [
      field.key,
      hiddenKeys.has(field.key) ? "" : toInput(field, contact?.fields[field.key]),
    ]),
  );
  const [key, setKey] = useState("");
  const [values, setValues] = useState<Values>(initial);
  const [problems, setProblems] = useState<Record<string, string>>({});
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);

  function set(name: string, text: string) {
    setValues((current) => ({ ...current, [name]: text }));
  }

  /** The fields to send, or the problems that stop the request. */
  function collect(): { fields: Record<string, unknown> } | { problems: Record<string, string> } {
    const fields: Record<string, unknown> = {};
    const found: Record<string, string> = {};
    for (const field of schema.fields) {
      const text = values[field.key] ?? "";
      const before = initial[field.key] ?? "";
      if (contact !== undefined && text === before) continue;
      if (text === "") {
        // Blank on a create is an absent field; on an edit it removes the field, unless it was
        // hidden, when blank means "keep".
        if (contact !== undefined && !hiddenKeys.has(field.key)) fields[field.key] = null;
        continue;
      }
      const parsed = fromInput(field, text);
      if ("error" in parsed) found[field.key] = parsed.error;
      else fields[field.key] = parsed.value;
    }
    return Object.keys(found).length > 0 ? { problems: found } : { fields };
  }

  async function submit(event: SyntheticEvent) {
    event.preventDefault();
    setFailure(null);
    const collected = collect();
    if ("problems" in collected) {
      setProblems(collected.problems);
      return;
    }
    setProblems({});
    setBusy(true);
    try {
      const saved = contact
        ? await unwrap(
            client.PATCH("/api/v1/workspaces/{ws}/contacts/{key}", {
              params: { path: { ws: workspaceId, key: contact.key } },
              body: { fields: collected.fields },
            }),
          )
        : await unwrap(
            client.POST("/api/v1/workspaces/{ws}/contacts", {
              params: { path: { ws: workspaceId } },
              body: { key: key.trim(), fields: collected.fields },
            }),
          );
      onSaved(saved);
    } catch (error) {
      showFailure(error);
      setBusy(false);
    }
  }

  function showFailure(error: unknown) {
    if (error instanceof ApiError && error.status === 409) {
      setProblems({ "": "A contact with this key already exists." });
      return;
    }
    if (error instanceof ApiError && error.errors.length > 0) {
      setProblems(Object.fromEntries(error.errors.map((entry) => [entry.field, reasonText(entry.reason)])));
      return;
    }
    setFailure(describeError(error));
  }

  const keyProblem = problems[""] ?? problems["key"];
  // Problems the form has no input for, such as a field the definition no longer has.
  const stray = Object.entries(problems).filter(
    ([name]) => name !== "" && name !== "key" && !schema.fields.some((field) => field.key === name),
  );

  return (
    <Dialog title={contact ? `Edit ${contact.key}` : "New contact"} onClose={onClose}>
      <form className="stack" onSubmit={(event) => void submit(event)} noValidate>
        {contact === undefined ? (
          <TextField
            label="Key (required)"
            mono
            hint="Your own ID for this contact, up to 256 characters. It cannot be changed later."
            value={key}
            onChange={(event) => setKey(event.target.value)}
            error={keyProblem}
            autoComplete="off"
          />
        ) : null}
        {!schema.defined ? (
          <p className="muted">
            This workspace has no contact definition yet, so a contact is only a key. A workspace admin can define the fields
            contacts carry.
          </p>
        ) : null}
        {schema.fields.map((field) => {
          const hidden = hiddenKeys.has(field.key);
          const common = {
            label: labelFor(field),
            hint: hintFor(field, hidden),
            error: problems[field.key],
            value: values[field.key] ?? "",
          };
          return field.type === "boolean" ? (
            <SelectField key={field.key} {...common} onChange={(event) => set(field.key, event.target.value)}>
              <option value="">{hidden ? "Hidden (keep)" : "Not set"}</option>
              <option value="true">Yes</option>
              <option value="false">No</option>
            </SelectField>
          ) : (
            <TextField
              key={field.key}
              {...common}
              onChange={(event) => set(field.key, event.target.value)}
              placeholder={hidden ? "Hidden" : undefined}
              autoComplete="off"
            />
          );
        })}
        {stray.map(([name, message]) => (
          <InlineError key={name} title={name} detail={message} />
        ))}
        {failure ? <InlineError {...failure} /> : null}
        <div className="dialog-actions">
          <Button onClick={onClose}>Cancel</Button>
          <Button type="submit" variant="primary" disabled={busy}>
            {busy ? "Saving" : contact ? "Save changes" : "Create contact"}
          </Button>
        </div>
      </form>
    </Dialog>
  );
}

