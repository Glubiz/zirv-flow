import { useState } from "react";
import { Link, useParams } from "react-router";

import {
  ApiError,
  client,
  describeError,
  offeredDestinations,
  unwrap,
  useAllowlistOptions,
  useDestinationOptions,
  useResource,
  type components,
  type ErrorSummary,
  type Resource,
} from "../../api";
import { Button, EmptyState, ErrorState, InlineError, LoadingState, ResourceView, SelectField } from "../../ui";
import { formatTime } from "../../ui/format";
import { useMembers } from "../members";
import { useTemplateAccess } from "./access";
import type { Template, TemplateDetail } from "./model";
import { labelOf } from "./PreviewPane";
import { LOG_PAGE, RecentTests } from "./RecentTests";
import { SenderSelect } from "./SenderPicker";
import { WithSenderOptions, type SenderOptions } from "./senderOptions";

type Quota = components["schemas"]["QuotaBody"];

const SAMPLE = "sample";

export function TestSendPage() {
  const { ws = "", id = "" } = useParams();
  const access = useTemplateAccess(ws);
  const detail = useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/templates/{id}", { params: { path: { ws, id } } })),
    [ws, id],
  );
  return (
    <ResourceView resource={access.ready ? detail : { status: "loading", reload: detail.reload }} loadingLabel="Loading the template">
      {(data) => <Gate key={`${ws}:${id}`} workspaceId={ws} template={data} canWrite={access.canWrite} canAdmin={access.canAdmin} />}
    </ResourceView>
  );
}

interface GateProps {
  workspaceId: string;
  template: TemplateDetail;
  canWrite: boolean;
  canAdmin: boolean;
}

function Gate(props: GateProps) {
  const { workspaceId, template, canWrite } = props;
  const archived = template.archived_at !== null && template.archived_at !== undefined;
  const reason = !canWrite
    ? "Only members who can edit templates can send a test."
    : template.channel !== "email"
      ? "Only an email template can be test-sent."
      : archived
        ? "This template is archived, so it cannot be test-sent."
        : null;
  if (reason !== null) {
    return (
      <div className="tp-page">
        <Link to={`/w/${workspaceId}/templates/${template.id}`}>← Back to editing</Link>
        <EmptyState title="No test can be sent">{reason}</EmptyState>
      </div>
    );
  }
  return <WithSenderOptions workspaceId={workspaceId}>{(senders) => <Page {...props} senders={senders} />}</WithSenderOptions>;
}

type Pick = { kind: "member" | "entry"; id: string; label: string };

const keyOf = (choice: Pick) => `${choice.kind}:${choice.id}`;

interface Attempt {
  key: string;
  label: string;
  state: "waiting" | "sending" | "queued" | "refused" | "stopped";
  detail?: string;
}

const STATE_WORDS: Record<Attempt["state"], string> = {
  waiting: "Waiting",
  sending: "Sending",
  queued: "Queued",
  refused: "Refused",
  stopped: "Not sent",
};

function initials(email: string): string {
  return email.slice(0, 2).toUpperCase();
}

function Page({ workspaceId, template, canAdmin, senders }: GateProps & { senders: SenderOptions }) {
  const destinations = useDestinationOptions(workspaceId);
  const members = useMembers(workspaceId);
  const allowlist = useAllowlistOptions(workspaceId);
  const quota = useResource(() => unwrap(client.GET("/api/v1/workspaces/{ws}/test-sends/quota", { params: { path: { ws: workspaceId } } })), [workspaceId]);
  const log = useResource(
    () =>
      unwrap(
        client.GET("/api/v1/workspaces/{ws}/test-sends", {
          params: { path: { ws: workspaceId }, query: { template_id: template.id, limit: LOG_PAGE } },
        }),
      ),
    [workspaceId, template.id],
  );

  const [picked, setPicked] = useState<Record<string, boolean>>({});
  const [destination, setDestination] = useState("");
  const [sender, setSender] = useState(template.sender_identity_id ?? "");
  const [content, setContent] = useState<"draft" | "live">("draft");
  const [person, setPerson] = useState(SAMPLE);
  const [busy, setBusy] = useState(false);
  const [attempts, setAttempts] = useState<Attempt[]>([]);
  const [stoppedAt, setStoppedAt] = useState<string | null | undefined>(undefined);
  const [resetKey, setResetKey] = useState(0);

  const sources = [destinations, senders, members, allowlist];
  const loading = sources.some((entry) => entry.status === "loading");
  const loadFailed = sources.find((entry) => entry.status === "error");
  const sendable = destinations.status === "ready" ? offeredDestinations(destinations.data, "send") : [];
  const chosenDestination = destination !== "" ? destination : sendable.length === 1 ? (sendable[0]?.id ?? "") : "";

  const choices: Pick[] = [
    ...(members.status === "ready" ? members.data.map((entry) => ({ kind: "member" as const, id: entry.user_id, label: entry.email })) : []),
    ...(allowlist.status === "ready" ? allowlist.data.map((entry) => ({ kind: "entry" as const, id: entry.id, label: entry.name })) : []),
  ];
  const chosen = choices.filter((choice) => picked[keyOf(choice)]);
  const left = quota.status === "ready" ? Math.max(0, quota.data.member.limit - quota.data.member.used) : null;
  const overQuota = left !== null && chosen.length > left;
  const blocked = busy || chosen.length === 0 || overQuota || chosenDestination === "" || sender === "";

  function toggle(choice: Pick) {
    const key = keyOf(choice);
    setPicked((current) => ({ ...current, [key]: !current[key] }));
  }

  function update(key: string, patch: Partial<Attempt>) {
    setAttempts((current) => current.map((entry) => (entry.key === key ? { ...entry, ...patch } : entry)));
  }

  async function send() {
    if (blocked) return;
    const queue = chosen;
    setBusy(true);
    setStoppedAt(undefined);
    setAttempts(queue.map((choice) => ({ key: keyOf(choice), label: choice.label, state: "waiting" })));
    for (const [index, choice] of queue.entries()) {
      const key = keyOf(choice);
      update(key, { state: "sending" });
      try {
        await unwrap(
          client.POST("/api/v1/workspaces/{ws}/templates/{id}/test-send", {
            params: { path: { ws: workspaceId, id: template.id } },
            body: {
              destination_id: chosenDestination,
              sender_identity_id: sender,
              to: choice.kind === "member" ? { member: choice.id } : { allowlist_entry: choice.id },
              ...(content === "live" && template.current_version ? { version: template.current_version } : {}),
              ...(person !== SAMPLE ? { person_id: person } : {}),
            },
          }),
        );
        update(key, { state: "queued" });
        setResetKey((value) => value + 1);
        log.reload();
        quota.reload();
      } catch (error) {
        if (error instanceof ApiError && error.status === 429) {
          const rest = new Set(queue.slice(index).map(keyOf));
          setAttempts((current) => current.map((entry) => (rest.has(entry.key) ? { ...entry, state: "stopped" } : entry)));
          setStoppedAt(await resetTime(workspaceId));
          quota.reload();
          break;
        }
        const summary = describeError(error);
        update(key, { state: "refused", detail: summary.detail || summary.title });
      }
    }
    setBusy(false);
  }

  function retryAll() {
    for (const entry of sources) entry.reload();
  }

  return (
    <div className="tp-page ts-page">
      <div className="tp-head">
        <Link to={`/w/${workspaceId}/templates/${template.id}`}>← Back to editing</Link>
        <div className="row-grow">
          <p className="faint">{template.name}</p>
          <h1 className="fl-name-read">Send a test</h1>
        </div>
        <QuotaLine quota={quota} />
      </div>
      <div className="ts-split">
        <section className="panel panel-pad ts-form">
          {loading ? <LoadingState label="Loading destinations and recipients" /> : null}
          {loadFailed?.status === "error" ? <ErrorState {...describeError(loadFailed.error)} onRetry={retryAll} /> : null}
          {!loading && !loadFailed ? (
            <form
              className="stack"
              onSubmit={(event) => {
                event.preventDefault();
                void send();
              }}
              noValidate
              aria-label="Test send"
            >
              <h2 className="panel-title">Who gets it</h2>
              <fieldset className="ts-group">
                <legend className="field-label">Team members</legend>
                {members.status === "ready" && members.data.length === 0 ? <p className="faint">No members yet.</p> : null}
                {choices
                  .filter((choice) => choice.kind === "member")
                  .map((choice) => (
                    <label className="ts-recipient" key={choice.id}>
                      <input type="checkbox" checked={picked[keyOf(choice)] ?? false} onChange={() => toggle(choice)} />
                      <span className="ts-avatar" aria-hidden="true">
                        {initials(choice.label)}
                      </span>
                      <span>{choice.label}</span>
                    </label>
                  ))}
              </fieldset>
              <fieldset className="ts-group">
                <legend className="field-label">Test inboxes</legend>
                {allowlist.status === "ready" && allowlist.data.length === 0 ? <p className="faint">The allowlist is empty.</p> : null}
                {choices
                  .filter((choice) => choice.kind === "entry")
                  .map((choice) => (
                    <label className="ts-recipient" key={choice.id}>
                      <input type="checkbox" checked={picked[keyOf(choice)] ?? false} onChange={() => toggle(choice)} />
                      <span>{choice.label}</span>
                    </label>
                  ))}
                <p className="faint">
                  Only members and allowlisted inboxes can get tests. <Link to={`/w/${workspaceId}/settings/sending`}>Manage the allowlist</Link>
                </p>
              </fieldset>

              <h2 className="panel-title">What they get</h2>
              <div className="field">
                <span className="field-label" id="ts-content-label">
                  Content
                </span>
                <div className="seg-tabs" role="group" aria-labelledby="ts-content-label">
                  <button type="button" className={content === "draft" ? "seg seg-on" : "seg"} aria-pressed={content === "draft"} onClick={() => setContent("draft")}>
                    Draft
                  </button>
                  {template.current_version ? (
                    <button type="button" className={content === "live" ? "seg seg-on" : "seg"} aria-pressed={content === "live"} onClick={() => setContent("live")}>
                      Version {template.current_version} · live
                    </button>
                  ) : null}
                </div>
              </div>
              {canAdmin ? (
                <FillAs workspaceId={workspaceId} template={template} value={person} onChange={setPerson} />
              ) : (
                <SelectField label="Fill fields as" value={SAMPLE} onChange={() => setPerson(SAMPLE)} hint="Real people's values: owners and admins only.">
                  <option value={SAMPLE}>Sample person</option>
                </SelectField>
              )}
              <div className="ts-pair">
                <SelectField
                  label="Send through"
                  value={chosenDestination}
                  onChange={(event) => setDestination(event.target.value)}
                  hint={destinations.status === "ready" && sendable.length === 0 ? "No destination can send email. An admin adds one under destinations." : undefined}
                >
                  <option value="">Choose a destination</option>
                  {sendable.map((entry) => (
                    <option key={entry.id} value={entry.id}>
                      {entry.name}
                    </option>
                  ))}
                </SelectField>
                <SenderSelect senders={senders} value={sender} onChange={setSender} disabled={false} label="From" />
              </div>
              <div className="ts-note">
                <span>
                  The subject starts with <span className="strong">[TEST]</span>. Links open but record no clicks, and unsubscribe links change nothing.
                </span>
                <span>Consent, quiet hours and caps are skipped. Suppressions still apply.</span>
              </div>
              <div className="row">
                <Button type="submit" variant="primary" disabled={blocked}>
                  {busy ? "Sending" : chosen.length === 0 ? "Pick a recipient" : chosen.length === 1 ? "Send 1 test" : `Send ${chosen.length} tests`}
                </Button>
                <span className="faint">{overQuota ? `Only ${left} ${left === 1 ? "test" : "tests"} left this hour.` : "Each recipient gets its own message."}</span>
              </div>
              <Results attempts={attempts} stoppedAt={stoppedAt} />
            </form>
          ) : null}
        </section>
        <RecentTests workspaceId={workspaceId} templateId={template.id} log={log} resetKey={resetKey} />
      </div>
    </div>
  );
}

function QuotaLine({ quota }: { quota: Resource<Quota> }) {
  if (quota.status === "loading") return <span className="faint">Checking this hour's tests</span>;
  if (quota.status === "error") return <span className="faint">The hourly count is unavailable.</span>;
  const { member, workspace } = quota.data;
  const left = Math.max(0, member.limit - member.used);
  return (
    <span className="faint" role="status">
      {left} of your {member.limit} tests left this hour · workspace {workspace.used} of {workspace.limit}
    </span>
  );
}

function Results({ attempts, stoppedAt }: { attempts: Attempt[]; stoppedAt: string | null | undefined }) {
  if (attempts.length === 0) return null;
  return (
    <div className="stack-tight" role="status" aria-label="Results">
      <ul className="ts-results">
        {attempts.map((entry) => (
          <li key={entry.key}>
            <span className="strong">{entry.label}</span>: {STATE_WORDS[entry.state]}
            {entry.detail ? <span className="faint"> {entry.detail}</span> : null}
          </li>
        ))}
      </ul>
      {stoppedAt !== undefined ? (
        <p className="notice">
          The hourly limit is reached, so the rest were not sent.{" "}
          {stoppedAt === null ? "Try again a little later." : `A slot frees at ${formatTime(stoppedAt)}. Try again after that.`}
        </p>
      ) : null}
    </div>
  );
}

/** When the oldest counted test leaves the hour, or null when the count cannot be read. */
async function resetTime(workspaceId: string): Promise<string | null> {
  try {
    const quota = await unwrap(client.GET("/api/v1/workspaces/{ws}/test-sends/quota", { params: { path: { ws: workspaceId } } }));
    return quota.resets_at ?? null;
  } catch {
    return null;
  }
}

/** The admin's "Fill fields as": the sample person and the people saved on the template. */
function FillAs({ workspaceId, template, value, onChange }: { workspaceId: string; template: Template; value: string; onChange: (id: string) => void }) {
  const saved = useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/templates/{id}/preview-people", { params: { path: { ws: workspaceId, id: template.id } } })),
    [workspaceId, template.id],
  );
  const failure: ErrorSummary | null = saved.status === "error" ? describeError(saved.error) : null;
  return (
    <>
      <SelectField label="Fill fields as" value={value} onChange={(event) => onChange(event.target.value)} hint="Real people's values: owners and admins only.">
        <option value={SAMPLE}>Sample person</option>
        {saved.status === "ready"
          ? saved.data.map((entry) => (
              <option key={entry.person_id} value={entry.person_id}>
                {labelOf(entry.person_id, entry.display)}
              </option>
            ))
          : null}
      </SelectField>
      {failure ? <InlineError {...failure} /> : null}
    </>
  );
}
