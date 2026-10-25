import { useRef, useState } from "react";

import { Button, TextAreaField, TextField } from "../../ui";
import { FieldIssues, IssueList } from "./IssueList";
import { BlockEditor } from "./blocks/BlockEditor";
import { MergeTagPicker } from "./MergeTagPicker";
import { SenderPicker } from "./SenderPicker";
import { TabBar } from "./TabBar";
import { issuesAt, type Editable, type TemplateIssue } from "./model";

type EmailTab = "subject" | "source" | "text" | "settings";
type WebhookTab = "headers" | "body";

interface Props {
  workspaceId: string;
  doc: Editable;
  onChange: (next: Editable) => void;
  readOnly: boolean;
  onConvert?: () => void;
  issues: readonly TemplateIssue[];
  checked: boolean;
}

/** The editor pane: the code fields of the template, the merge-tag picker and the validation list. */
export function SourcePane(props: Props) {
  const { doc } = props;
  if (doc.kind === "unsupported") {
    return (
      <section className="tp-pane" aria-label="Editor">
        <p className="notice">
          This template is written in {doc.mode === "blocks" ? "blocks" : "a mode"} this editor cannot change yet. You can still
          preview it and read its versions.
        </p>
        <IssueList issues={props.issues} checked={props.checked} />
      </section>
    );
  }
  if (doc.kind === "blocks") {
    return <BlockEditor workspaceId={props.workspaceId} doc={doc} onChange={props.onChange} readOnly={props.readOnly} issues={props.issues} checked={props.checked} onConvert={props.readOnly || doc.newer ? undefined : props.onConvert} />;
  }
  return doc.kind === "email" ? <EmailPane {...props} doc={doc} /> : <WebhookPane {...props} doc={doc} />;
}

type Field = HTMLTextAreaElement | HTMLInputElement;

/** Puts `tag` into the field at its caret, or at its end when it never had one. */
function insertInto(element: Field | null, value: string, tag: string, set: (next: string) => void) {
  const start = element?.selectionStart ?? value.length;
  const end = element?.selectionEnd ?? start;
  set(value.slice(0, start) + tag + value.slice(end));
  if (element) {
    const caret = start + tag.length;
    window.requestAnimationFrame(() => {
      element.focus();
      element.setSelectionRange(caret, caret);
    });
  }
}

function EmailPane({ workspaceId, doc, onChange, readOnly, issues, checked }: Props & { doc: Extract<Editable, { kind: "email" }> }) {
  const [tab, setTab] = useState<EmailTab>("source");
  const field = useRef<HTMLTextAreaElement>(null);
  const email = doc.email;
  const sourceLabel = doc.mode === "mjml" ? "MJML" : "HTML";
  const tabs: { id: EmailTab; label: string }[] = [
    { id: "subject", label: "Subject" },
    { id: "source", label: sourceLabel },
    { id: "text", label: "Text" },
    { id: "settings", label: "Settings" },
  ];
  const set = (patch: Partial<typeof email>) => onChange({ ...doc, email: { ...email, ...patch } });
  const editable = tab !== "settings" && !readOnly;
  const value = tab === "subject" ? email.subject : tab === "text" ? email.text : email.source;

  function insert(tag: string) {
    if (tab === "subject") insertInto(field.current, email.subject, tag, (subject) => set({ subject }));
    else if (tab === "text") insertInto(field.current, email.text, tag, (text) => set({ text }));
    else insertInto(field.current, email.source, tag, (source) => set({ source }));
  }

  return (
    <section className="tp-pane" aria-label="Editor">
      <p className="faint">Code template, {sourceLabel} and text. The mode was chosen when the template was made.</p>
      <TabBar label="Editor fields" tabs={tabs} tab={tab} onTab={setTab} />
      <div role="tabpanel" aria-label={tabs.find((entry) => entry.id === tab)?.label} className="stack">
        {tab === "settings" ? (
          <Settings workspaceId={workspaceId} doc={doc} onChange={set} readOnly={readOnly} issues={issues} />
        ) : (
          <>
            <TextAreaField
              key={tab}
              ref={field}
              label={tab === "subject" ? "Subject" : tab === "text" ? "Text part" : `${sourceLabel} source`}
              mono
              rows={tab === "subject" ? 2 : 18}
              spellCheck={false}
              value={value}
              readOnly={readOnly}
              onChange={(event) => {
                const next = event.target.value;
                if (tab === "subject") set({ subject: next });
                else if (tab === "text") set({ text: next });
                else set({ source: next });
              }}
              hint={tab === "text" ? "What a mail client shows when it cannot show HTML." : undefined}
            />
            <FieldIssues issues={issuesAt(issues, tab === "subject" ? "/email/subject" : tab === "text" ? "/email/text" : "/email/html")} />
          </>
        )}
      </div>
      <IssueList issues={issues} checked={checked} onGo={(where) => setTab(where === "Subject" ? "subject" : where === "Text" ? "text" : where === "Settings" ? "settings" : "source")} />
      {readOnly ? null : (
        <MergeTagPicker workspaceId={workspaceId} disabled={!editable} target={`the ${tabs.find((entry) => entry.id === tab)?.label.toLowerCase() ?? "field"}`} onInsert={insert} />
      )}
    </section>
  );
}

function Settings({
  workspaceId,
  doc,
  onChange,
  readOnly,
  issues,
}: {
  workspaceId: string;
  doc: Extract<Editable, { kind: "email" }>;
  onChange: (patch: Partial<Extract<Editable, { kind: "email" }>["email"]>) => void;
  readOnly: boolean;
  issues: readonly TemplateIssue[];
}) {
  const email = doc.email;
  return (
    <div className="stack">
      <SenderPicker workspaceId={workspaceId} value={email.sender} onChange={(sender) => onChange({ sender })} disabled={readOnly} />
      <FieldIssues issues={issuesAt(issues, "/email/sender")} />
      <TextField
        label="Language"
        value={email.lang}
        onChange={(event) => onChange({ lang: event.target.value })}
        readOnly={readOnly}
        hint="A language code such as en or da."
        autoComplete="off"
      />
      <FieldIssues issues={issuesAt(issues, "/email/lang")} />
    </div>
  );
}

function WebhookPane({ workspaceId, doc, onChange, readOnly, issues, checked }: Props & { doc: Extract<Editable, { kind: "webhook" }> }) {
  const [tab, setTab] = useState<WebhookTab>("body");
  const field = useRef<HTMLTextAreaElement>(null);
  const hook = doc.webhook;
  const tabs: { id: WebhookTab; label: string }[] = [
    { id: "headers", label: "Headers" },
    { id: "body", label: "Body" },
  ];
  const set = (patch: Partial<typeof hook>) => onChange({ ...doc, webhook: { ...hook, ...patch } });

  return (
    <section className="tp-pane" aria-label="Editor">
      <p className="faint">Webhook template. Merge tags are allowed inside string values only, so the body stays valid JSON.</p>
      <TabBar label="Editor fields" tabs={tabs} tab={tab} onTab={setTab} />
      <div role="tabpanel" aria-label={tabs.find((entry) => entry.id === tab)?.label} className="stack">
        {tab === "body" ? (
          <>
            <TextAreaField
              ref={field}
              label="JSON body"
              mono
              rows={16}
              spellCheck={false}
              value={hook.body}
              readOnly={readOnly}
              onChange={(event) => set({ body: event.target.value })}
            />
            <FieldIssues issues={issuesAt(issues, "/webhook/body")} />
          </>
        ) : (
          <>
            {hook.headers.length === 0 ? <p className="faint">No headers.</p> : null}
            {hook.headers.map((header, index) => (
              <div key={index} className="fl-pair">
                <TextField
                  label={`Header ${index + 1} name`}
                  value={header.name}
                  readOnly={readOnly}
                  onChange={(event) => set({ headers: hook.headers.map((entry, at) => (at === index ? { ...entry, name: event.target.value } : entry)) })}
                  autoComplete="off"
                />
                <TextField
                  label={`Header ${index + 1} value`}
                  value={header.value}
                  readOnly={readOnly}
                  onChange={(event) => set({ headers: hook.headers.map((entry, at) => (at === index ? { ...entry, value: event.target.value } : entry)) })}
                  autoComplete="off"
                />
                {readOnly ? null : (
                  <Button aria-label={`Remove header ${index + 1}`} onClick={() => set({ headers: hook.headers.filter((_, at) => at !== index) })}>
                    Remove
                  </Button>
                )}
              </div>
            ))}
            {readOnly ? null : <Button onClick={() => set({ headers: [...hook.headers, { name: "", value: "" }] })}>Add a header</Button>}
            <FieldIssues issues={issuesAt(issues, "/webhook/headers")} />
          </>
        )}
      </div>
      <IssueList issues={issues} checked={checked} onGo={(where) => setTab(where === "Headers" ? "headers" : "body")} />
      {readOnly ? null : (
        <MergeTagPicker
          workspaceId={workspaceId}
          disabled={tab !== "body"}
          target="the body"
          onInsert={(tag) => insertInto(field.current, hook.body, tag, (body) => set({ body }))}
        />
      )}
    </section>
  );
}
