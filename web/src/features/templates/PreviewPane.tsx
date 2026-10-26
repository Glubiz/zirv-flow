import { useState, type SyntheticEvent } from "react";

import { client, describeError, unwrap, unwrapEmpty, useResource, type ErrorSummary } from "../../api";
import { Button, ErrorState, InlineError, LoadingState, SelectField, TextField } from "../../ui";
import type { TemplatePreview } from "./model";

export type PreviewState =
  | { status: "loading" }
  | { status: "ready"; data: TemplatePreview }
  | { status: "failed"; error: ErrorSummary };

export interface PersonChoice {
  id: string;
  label: string;
}

type Width = "desktop" | "mobile";
type View = "html" | "text";

interface Props {
  workspaceId: string;
  templateId: string;
  state: PreviewState;
  refreshing: boolean;
  onRetry: () => void;
  canAdmin: boolean;
  person: PersonChoice | null;
  onPerson: (person: PersonChoice | null) => void;
}

/** The server-rendered preview. The HTML goes into an iframe with every sandbox permission off; the SPA never renders it. */
export function PreviewPane({ workspaceId, templateId, state, refreshing, onRetry, canAdmin, person, onPerson }: Props) {
  const [width, setWidth] = useState<Width>("desktop");
  const [view, setView] = useState<View>("html");

  return (
    <section className="tp-preview" aria-label="Preview" aria-busy={refreshing || undefined}>
      <div className="row">
        {canAdmin ? (
          <AdminPreviewAs workspaceId={workspaceId} templateId={templateId} person={person} onPerson={onPerson} />
        ) : (
          <SelectField label="Preview as" value="sample" onChange={() => onPerson(null)}>
            <option value="sample">Sample person</option>
          </SelectField>
        )}
        <div className="seg-tabs" role="group" aria-label="Preview width">
          {(["desktop", "mobile"] as const).map((entry) => (
            <button key={entry} type="button" className={width === entry ? "seg seg-on" : "seg"} aria-pressed={width === entry} onClick={() => setWidth(entry)}>
              {entry === "desktop" ? "Desktop" : "Mobile"}
            </button>
          ))}
        </div>
      </div>
      {!canAdmin ? <p className="faint">Only admins can preview as a real person.</p> : null}
      {refreshing ? (
        <p className="faint" role="status">
          Updating the preview
        </p>
      ) : null}
      {state.status === "loading" ? <LoadingState label="Rendering the preview" /> : null}
      {state.status === "failed" ? (
        <ErrorState title="The preview could not be rendered" detail={state.error.detail || state.error.title} traceId={state.error.traceId} onRetry={onRetry} />
      ) : null}
      {state.status === "ready" ? <Rendered data={state.data} width={width} view={view} onView={setView} /> : null}
      <p className="faint">Rendered on the server by the renderer a send uses. Links are inert in the preview.</p>
    </section>
  );
}

const SAMPLE = "sample";
const OTHER = "other";

function labelOf(id: string, display: { value?: string | null } | null | undefined): string {
  return display?.value ? display.value : `Person ${id.slice(0, 8)}`;
}

/** The admin's "Preview as" select: the sample person, the people saved on the template, and anyone else through the picker. */
function AdminPreviewAs({ workspaceId, templateId, person, onPerson }: { workspaceId: string; templateId: string; person: PersonChoice | null; onPerson: (person: PersonChoice | null) => void }) {
  const [asking, setAsking] = useState(false);
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);
  const path = { ws: workspaceId, id: templateId };
  const saved = useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/templates/{id}/preview-people", { params: { path } })),
    [workspaceId, templateId],
  );
  const people: PersonChoice[] = saved.status === "ready" ? saved.data.map((entry) => ({ id: entry.person_id, label: labelOf(entry.person_id, entry.display) })) : [];
  const isSaved = person !== null && people.some((entry) => entry.id === person.id);

  async function change(action: () => Promise<void>) {
    if (busy) return;
    setBusy(true);
    setFailure(null);
    try {
      await action();
      saved.reload();
    } catch (error) {
      setFailure(describeError(error));
    } finally {
      setBusy(false);
    }
  }

  const save = () =>
    change(async () => {
      if (person) await unwrap(client.POST("/api/v1/workspaces/{ws}/templates/{id}/preview-people", { params: { path }, body: { person_id: person.id } }));
    });
  const remove = () =>
    change(async () => {
      if (person) await unwrapEmpty(client.DELETE("/api/v1/workspaces/{ws}/templates/{id}/preview-people/{person_id}", { params: { path: { ...path, person_id: person.id } } }));
    });

  return (
    <div className="stack">
      <div className="row">
      <SelectField
        label="Preview as"
        value={asking ? OTHER : person ? person.id : SAMPLE}
        onChange={(event) => {
          const value = event.target.value;
          if (value === OTHER) {
            setAsking(true);
          } else {
            setAsking(false);
            onPerson(value === SAMPLE ? null : (people.find((entry) => entry.id === value) ?? null));
          }
        }}
      >
        <option value={SAMPLE}>Sample person</option>
        {person && !isSaved ? <option value={person.id}>{person.label}</option> : null}
        {people.map((entry) => (
          <option key={entry.id} value={entry.id}>
            {entry.label}
          </option>
        ))}
        <option value={OTHER}>Pick another person…</option>
      </SelectField>
      {person && !asking ? (
        <Button aria-disabled={busy || undefined} onClick={() => void (isSaved ? remove() : save())}>
          {isSaved ? "Remove" : "Save"}
        </Button>
      ) : null}
      </div>
      {saved.status === "error" ? <InlineError {...describeError(saved.error)} /> : null}
      {failure ? <InlineError {...failure} /> : null}
      {asking ? (
        <PersonPicker
          workspaceId={workspaceId}
          chosen={person}
          onChoose={(choice) => {
            setAsking(false);
            onPerson(choice);
          }}
        />
      ) : null}
    </div>
  );
}

function Rendered({ data, width, view, onView }: { data: TemplatePreview; width: Width; view: View; onView: (view: View) => void }) {
  if (!data.rendered) {
    return (
      <p className="notice" role="status">
        Nothing was rendered because the draft has blocking issues. Fix them and the preview returns.
      </p>
    );
  }
  if (data.body !== null && data.body !== undefined) {
    return (
      <div className="stack">
        <h3 className="field-label">Request body</h3>
        <pre className="fl-code tp-text">{data.body}</pre>
        <h3 className="field-label">Headers</h3>
        {data.headers.length === 0 ? <p className="faint">None.</p> : null}
        <ul className="list-plain">
          {data.headers.map((header) => (
            <li key={header.name}>
              <span className="mono">{header.name}</span>: <span className="mono">{header.value}</span>
            </li>
          ))}
        </ul>
      </div>
    );
  }
  return (
    <div className="stack">
      <dl className="facts">
        <div className="fact">
          <dt>Subject</dt>
          <dd>{data.subject ?? ""}</dd>
        </div>
        <div className="fact">
          <dt>Rendered for</dt>
          <dd>{data.person_id ? "A real person" : "A sample person"}</dd>
        </div>
      </dl>
      <div className="seg-tabs" role="group" aria-label="Preview part">
        <button type="button" className={view === "html" ? "seg seg-on" : "seg"} aria-pressed={view === "html"} onClick={() => onView("html")}>
          HTML
        </button>
        <button type="button" className={view === "text" ? "seg seg-on" : "seg"} aria-pressed={view === "text"} onClick={() => onView("text")}>
          Text
        </button>
      </div>
      {view === "html" ? (
        <div className={width === "mobile" ? "tp-frame tp-frame-mobile" : "tp-frame"}>
          <iframe title="Preview of the message" sandbox="" srcDoc={data.html ?? ""} className="tp-iframe" />
        </div>
      ) : (
        <pre className="fl-code tp-text" aria-label="Text part">
          {data.text ?? ""}
        </pre>
      )}
    </div>
  );
}

function PersonPicker({ workspaceId, chosen, onChoose }: { workspaceId: string; chosen: PersonChoice | null; onChoose: (person: PersonChoice) => void }) {
  const [value, setValue] = useState("");
  const [searching, setSearching] = useState(false);
  const [found, setFound] = useState<PersonChoice[] | null>(null);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);

  async function search(event: SyntheticEvent) {
    event.preventDefault();
    if (searching || value.trim() === "") return;
    setSearching(true);
    setFailure(null);
    try {
      const body = await unwrap(
        client.POST("/api/v1/workspaces/{ws}/people/search", {
          params: { path: { ws: workspaceId } },
          body: { identifier: { kind: "email", value: value.trim() }, limit: 10 },
        }),
      );
      setFound(body.people.map((hit) => ({ id: hit.id, label: hit.display?.value ? hit.display.value : `Person ${hit.id.slice(0, 8)}` })));
    } catch (error) {
      setFound(null);
      setFailure(describeError(error));
    } finally {
      setSearching(false);
    }
  }

  return (
    <form className="stack" aria-label="Find a person to preview as" onSubmit={(event) => void search(event)}>
      <div className="row">
        <TextField label="Person's email address" value={value} autoComplete="off" onChange={(event) => setValue(event.target.value)} />
        <Button type="submit" aria-disabled={searching || undefined}>
          {searching ? "Searching" : "Search"}
        </Button>
      </div>
      {failure ? <InlineError {...failure} /> : null}
      {found !== null && found.length === 0 ? <p className="muted">No one matches that.</p> : null}
      {found !== null && found.length > 0 ? (
        <ul className="list-plain" aria-label="People found">
          {found.map((hit) => (
            <li key={hit.id} className="row">
              <span>{hit.label}</span>
              <Button aria-label={`Preview as ${hit.label}`} aria-pressed={chosen?.id === hit.id} onClick={() => onChoose(hit)}>
                {chosen?.id === hit.id ? "Chosen" : "Use"}
              </Button>
            </li>
          ))}
        </ul>
      ) : null}
      {chosen ? (
        <p>
          Previewing as <span className="strong">{chosen.label}</span>
        </p>
      ) : null}
    </form>
  );
}
