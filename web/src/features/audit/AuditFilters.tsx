import { useId, useState, type SyntheticEvent } from "react";

import { describeError, type Resource } from "../../api";
import { Button, InlineError, SelectField, TextField } from "../../ui";
import { actionLabel, groupAreas } from "./areas";
import { ACTOR_KINDS, NO_FILTERS, hasFilters, type Filters } from "./model";
import type { Catalogue } from "./useAuditEntries";

interface Props {
  applied: Filters;
  catalogue: Resource<Catalogue>;
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

/** The Action field: the catalogue grouped, with its loading, empty and failed states shown in place and a retry for the failure. */
function ActionField({ catalogue, value, onChange }: { catalogue: Resource<Catalogue>; value: string; onChange: (value: string) => void }) {
  const ready = catalogue.status === "ready" ? catalogue.data : null;
  const empty = ready !== null && ready.areas.length === 0;
  return (
    <div className="audit-action">
      <SelectField
        label="Action"
        value={value}
        disabled={catalogue.status === "loading"}
        onChange={(event) => onChange(event.target.value)}
        hint={catalogue.status === "loading" ? "Loading the actions" : empty ? "No actions are recorded yet" : undefined}
      >
        <ActionOptions catalogue={ready} current={value} />
      </SelectField>
      {catalogue.status === "error" ? (
        <div className="audit-action-error">
          <InlineError {...describeError(catalogue.error)} />
          <Button onClick={catalogue.reload}>Retry actions</Button>
        </div>
      ) : null}
    </div>
  );
}

/** The filter bar. Nothing is requested until Apply, which puts the filters in the URL. On a phone it folds behind a Filters button. */
export function AuditFilters({ applied, catalogue, onApply }: Props) {
  const [draft, setDraft] = useState<Filters>(applied);
  const [open, setOpen] = useState(false);
  const [moreOpen] = useState(applied.objectKind !== "" || applied.requestId !== "");
  const formId = useId();
  const set = (key: keyof Filters) => (value: string) => setDraft((current) => ({ ...current, [key]: value }));
  const showActorId = draft.actorKind !== "" || draft.actorId !== "";

  function submit(event: SyntheticEvent) {
    event.preventDefault();
    setOpen(false);
    onApply(draft);
  }

  return (
    <div className="audit-filter-wrap">
      <Button className="audit-filters-toggle" aria-expanded={open} aria-controls={formId} onClick={() => setOpen(!open)}>
        Filters
      </Button>
      <form
        id={formId}
        className={open ? "panel panel-pad audit-filters" : "panel panel-pad audit-filters audit-filters-folded"}
        onSubmit={submit}
        aria-label="Filter the audit log"
      >
        <ActionField catalogue={catalogue} value={draft.action} onChange={set("action")} />
        <fieldset className="field audit-actor-field">
          <legend className="field-label">Actor</legend>
          <div className="audit-actor-row">
            <select
              className="select"
              aria-label="Actor kind"
              value={draft.actorKind}
              onChange={(event) => set("actorKind")(event.target.value)}
            >
              <option value="">Any actor</option>
              {ACTOR_KINDS.map((kind) => (
                <option key={kind.value} value={kind.value}>
                  {kind.label}
                </option>
              ))}
            </select>
            {showActorId ? (
              <input
                className="input input-mono"
                aria-label="Actor id"
                placeholder="Id"
                value={draft.actorId}
                onChange={(event) => set("actorId")(event.target.value)}
              />
            ) : null}
          </div>
        </fieldset>
        <TextField label="Object id" mono value={draft.objectId} onChange={(event) => set("objectId")(event.target.value)} />
        <TextField label="From" type="date" value={draft.from} onChange={(event) => set("from")(event.target.value)} />
        <TextField label="To" type="date" value={draft.to} onChange={(event) => set("to")(event.target.value)} />
        <div className="audit-filter-actions">
          <Button type="submit" variant="primary">
            Apply
          </Button>
          {hasFilters(draft) || hasFilters(applied) ? (
            <Button
              onClick={() => {
                setDraft(NO_FILTERS);
                setOpen(false);
                onApply(NO_FILTERS);
              }}
            >
              Clear
            </Button>
          ) : null}
        </div>
        <details className="audit-more" open={moreOpen || undefined}>
          <summary>More filters</summary>
          <div className="audit-more-fields">
            <TextField label="Object kind" value={draft.objectKind} onChange={(event) => set("objectKind")(event.target.value)} />
            <TextField label="Request id" mono value={draft.requestId} onChange={(event) => set("requestId")(event.target.value)} />
          </div>
        </details>
      </form>
    </div>
  );
}
