import { useState, type SyntheticEvent } from "react";
import { useNavigate } from "react-router";

import { client, describeError, unwrap, type ErrorSummary } from "../../api";
import { Button, Dialog, InlineError, TextField } from "../../ui";
import { createFrom, readTemplate } from "./duplicate";
import type { TemplateListItem } from "./library";
import { asBody, MODES, startingDocument, type EditMode } from "./model";

interface Props {
  workspaceId: string;
  /** The live templates a new one can start from. */
  templates: readonly TemplateListItem[];
  onClose: () => void;
}

type Channel = "email" | "webhook";
type Category = "marketing" | "transactional";

const COMING: readonly string[] = ["SMS", "Push", "In-app"];

const CATEGORY_NOTES: Record<Category, string> = {
  marketing: "Marketing emails follow consent and need an unsubscribe link. The theme footer adds one.",
  transactional: "Transactional emails are the ones a person expects after an action, such as a receipt. They need no unsubscribe link.",
};

const SHOWN = 6;

export function NewTemplateDialog({ workspaceId, templates, onClose }: Props) {
  const navigate = useNavigate();
  const [name, setName] = useState("");
  const [channel, setChannel] = useState<Channel>("email");
  const [category, setCategory] = useState<Category>("marketing");
  const [mode, setMode] = useState<EditMode>("html");
  const [copying, setCopying] = useState(false);
  const [search, setSearch] = useState("");
  const [source, setSource] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);

  // A copy keeps the channel and category of the template it starts from.
  const candidates = templates.filter(
    (entry) => !entry.archived_at && entry.channel === channel && (channel === "webhook" || entry.category === category),
  );
  const term = search.trim().toLowerCase();
  const matches = candidates.filter((entry) => entry.name.toLowerCase().includes(term)).slice(0, SHOWN);
  const chosen = copying ? source : null;
  const ready = name.trim() !== "" && (!copying || (chosen !== null && candidates.some((entry) => entry.id === chosen)));

  async function submit(event: SyntheticEvent) {
    event.preventDefault();
    if (busy || !ready) return;
    setBusy(true);
    setFailure(null);
    try {
      let created;
      if (chosen !== null) {
        created = await createFrom(workspaceId, name.trim(), await readTemplate(workspaceId, chosen));
      } else {
        const document = startingDocument(channel, mode, category);
        created = await unwrap(
          client.POST("/api/v1/workspaces/{ws}/templates", {
            params: { path: { ws: workspaceId } },
            body: {
              name: name.trim(),
              channel,
              category: channel === "email" ? category : "transactional",
              ...(document ? { document: asBody(document) } : {}),
            },
          }),
        );
      }
      void navigate(`/w/${workspaceId}/templates/${created.id}`);
    } catch (error) {
      setFailure(describeError(error));
      setBusy(false);
    }
  }

  return (
    <Dialog title="New template" onClose={onClose}>
      <form className="stack" onSubmit={(event) => void submit(event)} noValidate>
        <p className="muted">Creates a draft. Nothing is sent until you publish it and a flow uses it.</p>
        <TextField
          label="Name"
          hint="Up to 100 characters. Only your team sees it."
          value={name}
          onChange={(event) => setName(event.target.value)}
          maxLength={100}
          autoComplete="off"
          required
        />
        <fieldset className="fl-fieldset">
          <legend className="field-label">Channel</legend>
          <div className="tl-chans">
            {(["email", "webhook"] as const).map((entry) => (
              <button
                key={entry}
                type="button"
                className="tl-chan"
                aria-pressed={channel === entry}
                onClick={() => {
                  setChannel(entry);
                  setSource(null);
                }}
              >
                <b>{entry === "email" ? "Email" : "Webhook"}</b>
              </button>
            ))}
            {COMING.map((label) => (
              <button key={label} type="button" className="tl-chan" disabled>
                <b>{label}</b>
                <span className="field-hint">After 1.0</span>
              </button>
            ))}
          </div>
          <p className="field-hint">It cannot be changed later.</p>
        </fieldset>
        {channel === "email" ? (
          <fieldset className="fl-fieldset">
            <legend className="field-label">Category</legend>
            <div className="seg-tabs">
              {(["marketing", "transactional"] as const).map((entry) => (
                <button
                  key={entry}
                  type="button"
                  className={category === entry ? "seg seg-on" : "seg"}
                  aria-pressed={category === entry}
                  onClick={() => {
                    setCategory(entry);
                    setSource(null);
                  }}
                >
                  {entry === "marketing" ? "Marketing" : "Transactional"}
                </button>
              ))}
            </div>
            <p className="field-hint">{CATEGORY_NOTES[category]} It cannot be changed later.</p>
          </fieldset>
        ) : (
          <p className="muted">A webhook template holds headers and a JSON body. The destination in the flow supplies the URL and the signing.</p>
        )}
        <fieldset className="fl-fieldset">
          <legend className="field-label">Start from</legend>
          <label className="check">
            <input type="radio" name="template-start" checked={!copying} onChange={() => setCopying(false)} />
            <span>
              A blank template <span className="faint">Write it yourself.</span>
            </span>
          </label>
          <label className="check">
            <input type="radio" name="template-start" checked={copying} onChange={() => setCopying(true)} />
            <span>
              An existing template <span className="faint">Copies its draft.</span>
            </span>
          </label>
          {copying ? (
            <div className="tl-start">
              <TextField label="Search templates to copy" value={search} onChange={(event) => setSearch(event.target.value)} autoComplete="off" />
              {matches.length === 0 ? (
                <p className="muted">No {channel === "email" ? category : "webhook"} template matches.</p>
              ) : (
                <ul className="tl-picks" aria-label="Templates to copy">
                  {matches.map((entry) => (
                    <li key={entry.id}>
                      <label className="check">
                        <input type="radio" name="template-source" checked={source === entry.id} onChange={() => setSource(entry.id)} />
                        <span>{entry.name}</span>
                      </label>
                    </li>
                  ))}
                </ul>
              )}
            </div>
          ) : null}
        </fieldset>
        {channel === "email" && !copying ? (
          <fieldset className="fl-fieldset">
            <legend className="field-label">How you write it</legend>
            <p className="field-hint">Chosen now and kept for the life of the template.</p>
            {MODES.map((entry) => (
              <label key={entry.id} className="check">
                <input
                  type="radio"
                  name="template-mode"
                  value={entry.id}
                  checked={mode === entry.id}
                  disabled={!entry.available}
                  onChange={() => setMode(entry.id)}
                />
                <span>
                  {entry.label}
                  {entry.available ? null : <span className="faint"> (not available yet)</span>}
                  <span className="faint"> {entry.hint}</span>
                </span>
              </label>
            ))}
          </fieldset>
        ) : null}
        {failure ? <InlineError {...failure} /> : null}
        <div className="dialog-actions">
          <Button onClick={onClose}>Cancel</Button>
          <Button type="submit" variant="primary" disabled={busy || !ready}>
            {busy ? "Creating" : "Create template"}
          </Button>
        </div>
      </form>
    </Dialog>
  );
}
