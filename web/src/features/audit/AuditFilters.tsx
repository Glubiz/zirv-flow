import { useState, type SyntheticEvent } from "react";

import { Button, SelectField, TextField } from "../../ui";
import { actionLabel, groupAreas } from "./areas";
import { ACTOR_KINDS, NO_FILTERS, hasFilters, type Filters } from "./model";
import type { Catalogue } from "./useAuditEntries";

interface Props {
  applied: Filters;
  catalogue: Catalogue | null;
  onApply: (filters: Filters) => void;
}

function ActionOptions({ catalogue, current }: { catalogue: Catalogue | null; current: string }) {
  const groups = groupAreas(catalogue?.areas ?? []);
  const known =
    current === "" ||
    (catalogue?.areas ?? []).some((entry) => current === `${entry.area}.` || entry.actions.includes(current));
  return (
    <>
      <option value="">All actions</option>
      {known ? null : <option value={current}>{current}</option>}
      {groups.map((group) => (
        <optgroup key={group.group} label={group.group}>
          {group.areas.flatMap((entry) => [
            entry.actions.length > 1 ? (
              <option key={`${entry.area}.`} value={`${entry.area}.`}>
                {`Any ${entry.area.replace(/_/g, " ")} action`}
              </option>
            ) : null,
            ...entry.actions.map((action) => (
              <option key={action} value={action}>
                {`${actionLabel(action)} (${action})`}
              </option>
            )),
          ])}
        </optgroup>
      ))}
    </>
  );
}

/** The filter bar. Nothing is requested until Apply, which puts the filters in the URL. */
export function AuditFilters({ applied, catalogue, onApply }: Props) {
  const [draft, setDraft] = useState<Filters>(applied);
  const set = (key: keyof Filters) => (value: string) => setDraft((current) => ({ ...current, [key]: value }));

  function submit(event: SyntheticEvent) {
    event.preventDefault();
    onApply(draft);
  }

  return (
    <form className="panel panel-pad audit-filters" onSubmit={submit} aria-label="Filter the audit log">
      <SelectField label="Action" value={draft.action} onChange={(event) => set("action")(event.target.value)}>
        <ActionOptions catalogue={catalogue} current={draft.action} />
      </SelectField>
      <SelectField label="Actor kind" value={draft.actorKind} onChange={(event) => set("actorKind")(event.target.value)}>
        <option value="">Any actor</option>
        {ACTOR_KINDS.map((kind) => (
          <option key={kind.value} value={kind.value}>
            {kind.label}
          </option>
        ))}
      </SelectField>
      <TextField label="Actor id" mono value={draft.actorId} onChange={(event) => set("actorId")(event.target.value)} />
      <TextField label="Object kind" value={draft.objectKind} onChange={(event) => set("objectKind")(event.target.value)} />
      <TextField label="Object id" mono value={draft.objectId} onChange={(event) => set("objectId")(event.target.value)} />
      <TextField label="From" type="date" value={draft.from} onChange={(event) => set("from")(event.target.value)} />
      <TextField label="To" type="date" value={draft.to} onChange={(event) => set("to")(event.target.value)} />
      <TextField label="Request id" mono value={draft.requestId} onChange={(event) => set("requestId")(event.target.value)} />
      <div className="audit-filter-actions">
        <Button type="submit" variant="primary">
          Apply
        </Button>
        {hasFilters(draft) || hasFilters(applied) ? (
          <Button
            onClick={() => {
              setDraft(NO_FILTERS);
              onApply(NO_FILTERS);
            }}
          >
            Clear
          </Button>
        ) : null}
      </div>
    </form>
  );
}
