import { useState } from "react";

import { ApiError } from "../../api";
import { Button, Dialog, InlineError, previewDocument } from "../../ui";
import { AssistantPanel, DraftChanges, assistantGone, failureText, type Failure, type AcceptedDraft, type Draft, type TemplateTask } from "../assistant";
import type { Editable, TemplateDetail } from "./model";

type Width = "desktop" | "mobile";
type Render = { subject?: string | null; html?: string | null } | null | undefined;

/** Whether the assistant works on this document: only an email template, in blocks or code mode. */
export function assistable(doc: Editable): boolean {
  return doc.kind === "blocks" || doc.kind === "email";
}

/** The tasks a template's mode allows: a block layout needs blocks. */
function tasksOf(doc: Editable): readonly TemplateTask[] {
  return doc.kind === "blocks" ? ["copy", "subject_variants", "block_layout", "translation"] : ["copy", "subject_variants", "translation"];
}

interface Props {
  workspaceId: string;
  templateId: string;
  doc: Editable;
  /** Whether the editor holds edits the server does not have. */
  unsaved: boolean;
  /** Saves what is waiting and waits for any save in flight; false when a save failed. */
  flush: () => Promise<boolean>;
  /** Stops the editor saving while the accept runs. */
  hold: () => Promise<void>;
  release: () => void;
  /** The server wrote the draft: the editor shows it, with the chosen subject line when it is not the first. */
  onAccepted: (detail: TemplateDetail, subject: string | null) => void;
  /** After a lost reply: reads the template again and adopts its draft when the accept landed. */
  onReconcile: (subject: string | null) => Promise<boolean>;
  /** Replaces the editor's copy with the server's draft. */
  onReload: () => Promise<void>;
  onClose: () => void;
}

/** The left column of the template editor while the assistant is open. */
export function TemplateAssistant({ workspaceId, templateId, doc, unsaved, flush, hold, release, onAccepted, onReconcile, onReload, onClose }: Props) {
  const [replacing, setReplacing] = useState<((replace: boolean) => void) | null>(null);
  const [picked, setPicked] = useState<{ id: string; subject: string } | null>(null);
  const [epoch, setEpoch] = useState(0);
  const [reloading, setReloading] = useState(false);
  const [reloadFailure, setReloadFailure] = useState<Failure | null>(null);

  /** The subject line chosen from the variants of this draft, when it is not the first. */
  const pickedFor = (id: string) => (picked?.id === id ? picked.subject : null);

  async function confirmAccept(): Promise<boolean> {
    if (unsaved) {
      const replace = await new Promise<boolean>((resolve) => setReplacing(() => resolve));
      if (!replace) return false;
    }
    await hold();
    return true;
  }

  async function acceptFailed(error: unknown, draft: Draft): Promise<boolean> {
    // An answer from the server means nothing was written; only a lost reply can hide an accept.
    const landed = error instanceof ApiError ? false : await onReconcile(pickedFor(draft.id));
    if (!landed) release();
    return landed;
  }

  function accepted(_content: unknown, body: AcceptedDraft) {
    if (!body.template) {
      void reload();
      return;
    }
    onAccepted(body.template, pickedFor(body.id));
  }

  async function reload() {
    setReloading(true);
    setReloadFailure(null);
    try {
      await onReload();
      setEpoch((value) => value + 1);
    } catch (error) {
      release();
      setReloadFailure(failureText(error));
    } finally {
      setReloading(false);
    }
  }

  function answer(replace: boolean) {
    replacing?.(replace);
    setReplacing(null);
  }

  return (
    <section className="stack" aria-label="Template assistant">
      <div className="row">
        <h2 className="tp-assistant-title">Assistant</h2>
        <Button variant="quiet" onClick={onClose} aria-label="Close the assistant">
          ×
        </Button>
      </div>
      <AssistantPanel
        key={epoch}
        workspaceId={workspaceId}
        kind="template"
        templateId={templateId}
        templateTasks={tasksOf(doc)}
        onFailure={(error) => {
          // The assistant was turned off, or is no longer this person's: the panel reads its setting again.
          if (assistantGone(error)) setEpoch((value) => value + 1);
        }}
        prepare={flush}
        onAccept={accepted}
        confirmAccept={confirmAccept}
        onAcceptFailed={acceptFailed}
        renderChanges={(changes, draft) => <DraftReview changes={changes} draft={draft} onPick={(subject) => setPicked({ id: draft.id, subject })} />}
        renderAcceptAction={(code) =>
          code === "template_draft_changed" ? (
            <>
              <Button onClick={() => void reload()} disabled={reloading}>
                {reloading ? "Reloading" : "Reload the draft"}
              </Button>
              {reloadFailure ? <InlineError {...reloadFailure} /> : null}
            </>
          ) : null
        }
      />
      {replacing ? (
        <Dialog title="Replace your unsaved changes?" onClose={() => answer(false)}>
          <p className="muted">The draft has changes that are not saved yet. Accepting the assistant&rsquo;s draft replaces them.</p>
          <div className="dialog-actions">
            <Button onClick={() => answer(false)}>Keep my changes</Button>
            <Button variant="primary" onClick={() => answer(true)}>
              Replace them
            </Button>
          </div>
        </Dialog>
      ) : null}
    </section>
  );
}

/** What the assistant changed: the subject lines to choose from, the field diff and the two renders. */
function DraftReview({ changes, draft, onPick }: { changes: Draft["changes"]; draft: Draft; onPick: (subject: string) => void }) {
  const [width, setWidth] = useState<Width>("desktop");
  const [chosen, setChosen] = useState(0);
  const variants = draft.subject_variants;
  const renders = draft.renders;
  const repaired = draft.repaired ?? [];
  return (
    <div className="stack">
      {repaired.length > 0 ? <p className="faint">The assistant repaired {repaired.length === 1 ? "a field" : "fields"} before you saw this draft.</p> : null}
      {variants.length > 0 ? (
        <fieldset className="stack-tight">
          <legend className="field-label">Subject line</legend>
          {variants.map((subject, index) => (
            <label key={subject} className="row">
              <input
                type="radio"
                name={`subject-${draft.id}`}
                checked={chosen === index}
                onChange={() => {
                  setChosen(index);
                  onPick(subject);
                }}
              />
              <span>{subject}</span>
            </label>
          ))}
        </fieldset>
      ) : null}
      <DraftChanges changes={changes} repaired={repaired} />
      {renders ? (
        <div className="stack-tight">
          <div className="seg-tabs" role="group" aria-label="Width">
            {(["desktop", "mobile"] as const).map((option) => (
              <button key={option} type="button" className={width === option ? "seg seg-on" : "seg"} aria-pressed={width === option} onClick={() => setWidth(option)}>
                {option === "desktop" ? "Desktop" : "Mobile"}
              </button>
            ))}
          </div>
          <div className="tp-compare">
            <Frame title="Before" render={renders.before} width={width} missing="The current draft does not render, so there is nothing to compare." />
            <Frame title="After" render={renders.after} width={width} subject={variants[chosen]} missing="The drafted version does not render." />
          </div>
        </div>
      ) : null}
    </div>
  );
}

function Frame({ title, render, width, subject, missing }: { title: string; render: Render; width: Width; subject?: string; missing: string }) {
  return (
    <figure className="tp-compare-side">
      <figcaption className="field-label">{title}</figcaption>
      {render ? (
        <>
          <p className="faint">{subject ?? render.subject ?? ""}</p>
          <div className={width === "mobile" ? "tp-frame tp-frame-mobile" : "tp-frame"}>
            <iframe title={`${title} render`} sandbox="" srcDoc={previewDocument(render.html ?? "")} className="tp-iframe" />
          </div>
        </>
      ) : (
        <p className="faint">{missing}</p>
      )}
    </figure>
  );
}
