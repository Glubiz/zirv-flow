import { useState, type ReactNode, type SyntheticEvent } from "react";

import type { components } from "../../api";
import { Button, SelectField, TextField } from "../../ui";
import { IDENTIFIER_KINDS } from "./labels";

export type PersonTarget = components["schemas"]["PersonTarget"];

interface TargetFormProps {
  /** Names the form for assistive technology, such as "Erase a person". */
  legend: string;
  submitLabel: string;
  busy: boolean;
  onSubmit: (target: PersonTarget) => void;
  children?: ReactNode;
}

/** Asks which person a privacy request is about: by id, or by one of their identifiers. */
export function TargetForm({ legend, submitLabel, busy, onSubmit, children }: TargetFormProps) {
  const [by, setBy] = useState<"person" | "identifier">("identifier");
  const [personId, setPersonId] = useState("");
  const [kind, setKind] = useState("email");
  const [namespace, setNamespace] = useState("");
  const [value, setValue] = useState("");

  const identifierKind = kind === "external" ? `external:${namespace.trim()}` : kind;
  const complete =
    by === "person"
      ? personId.trim() !== ""
      : value.trim() !== "" && (kind !== "external" || namespace.trim() !== "");

  function submit(event: SyntheticEvent) {
    event.preventDefault();
    if (!complete) return;
    onSubmit(
      by === "person"
        ? { person_id: personId.trim() }
        : { identifier: { kind: identifierKind, value: value.trim() } },
    );
  }

  return (
    <form className="stack" onSubmit={submit} aria-label={legend} noValidate>
      <SelectField label="Find the person by" value={by} onChange={(event) => setBy(event.target.value as typeof by)}>
        <option value="identifier">An identifier</option>
        <option value="person">Their person id</option>
      </SelectField>
      {by === "person" ? (
        <TextField
          label="Person id"
          mono
          value={personId}
          onChange={(event) => setPersonId(event.target.value)}
          autoComplete="off"
          required
        />
      ) : (
        <>
          <SelectField label="Kind" value={kind} onChange={(event) => setKind(event.target.value)}>
            {IDENTIFIER_KINDS.map((option) => (
              <option key={option.value} value={option.value}>
                {option.label}
              </option>
            ))}
          </SelectField>
          {kind === "external" ? (
            <TextField
              label="Namespace"
              hint="The outside system, such as crm."
              value={namespace}
              onChange={(event) => setNamespace(event.target.value)}
              autoComplete="off"
              required
            />
          ) : null}
          <TextField
            label="Identifier"
            value={value}
            onChange={(event) => setValue(event.target.value)}
            autoComplete="off"
            required
          />
        </>
      )}
      {children}
      <div>
        <Button type="submit" variant="primary" disabled={busy || !complete}>
          {submitLabel}
        </Button>
      </div>
    </form>
  );
}
