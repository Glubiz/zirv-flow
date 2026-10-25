import { useState, type SyntheticEvent } from "react";

import { client, describeError, unwrap, useResource, type ErrorSummary } from "../../api";
import { Button, InlineError, Panel, TextAreaField, TextField } from "../../ui";
import { checkUtm, exampleLink, hasUtmProblems, parseDomains, sameUtm, UTM_FIELDS, UTM_PLACEHOLDERS, type UtmSettings } from "./utm";

interface Props {
  workspaceId: string;
  workspaceSlug: string;
  stored: UtmSettings;
  canManage: boolean;
  onSaved: () => void;
}

/** The "UTM parameters" section of the Sending screen: the toggle, four patterns, the domains and a live example link. */
export function UtmSection({ workspaceId, workspaceSlug, stored, canManage, onSaved }: Props) {
  const [original, setOriginal] = useState<UtmSettings>(stored);
  const [draft, setDraft] = useState<UtmSettings>(stored);
  const [domainText, setDomainText] = useState(stored.domains.join("\n"));
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [tried, setTried] = useState(false);
  const domains = useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/sending-domains", { params: { path: { ws: workspaceId } } })),
    [workspaceId],
  );
  const problems = checkUtm(draft);
  const dirty = !sameUtm(draft, original);
  const readOnly = !canManage;
  const suggestions =
    domains.status === "ready" ? domains.data.filter((entry) => entry.status === "verified" && !draft.domains.includes(entry.domain)) : [];

  function edit(change: Partial<UtmSettings>) {
    setNotice(null);
    setDraft((current) => ({ ...current, ...change }));
  }

  function setDomains(text: string) {
    setDomainText(text);
    edit({ domains: parseDomains(text) });
  }

  async function submit(event: SyntheticEvent) {
    event.preventDefault();
    setTried(true);
    setNotice(null);
    if (hasUtmProblems(problems)) return;
    setBusy(true);
    setFailure(null);
    try {
      const saved = await unwrap(client.PATCH("/api/v1/workspaces/{ws}", { params: { path: { ws: workspaceId } }, body: { utm: draft } }));
      setOriginal(saved.utm);
      setDraft(saved.utm);
      setDomainText(saved.utm.domains.join("\n"));
      setTried(false);
      setNotice(saved.utm.enabled ? "Saved. Links to the listed domains are tagged from the next send." : "Saved. Links are not tagged.");
      onSaved();
    } catch (error) {
      setFailure(describeError(error));
    } finally {
      setBusy(false);
    }
  }

  const hasExample = UTM_FIELDS.every(({ id }) => problems.fields[id] === undefined);

  return (
    <form className="stack" onSubmit={(event) => void submit(event)} noValidate aria-label="UTM parameters">
      <Panel title="UTM parameters">
        <div className="stack">
          <p className="muted">
            Tag the links in your emails with UTM parameters, so you do not tag them by hand. Only links to the listed domains (and their
            subdomains) are tagged. Unsubscribe and preference links, mail and phone links and anchors never are, and a UTM parameter already
            on a link is kept. Put <span className="mono">data-zf-utm=&quot;off&quot;</span> on a link to skip it.
          </p>
          <label className="check">
            <input type="checkbox" checked={draft.enabled} disabled={readOnly} onChange={(event) => edit({ enabled: event.target.checked })} />
            <span>Add UTM parameters to email links</span>
          </label>
          <p className="muted">
            Patterns can use {UTM_PLACEHOLDERS.map((name) => `{${name}}`).join(", ")}. Values are lower-cased and hyphenated, and a pattern that
            comes out empty leaves its parameter out. Nothing about the person is ever used.
          </p>
          {UTM_FIELDS.map(({ id, param, label }) => (
            <TextField
              key={id}
              label={label}
              hint={param}
              mono
              value={draft[id]}
              onChange={(event) => edit({ [id]: event.target.value })}
              disabled={readOnly}
              error={tried ? problems.fields[id] : undefined}
            />
          ))}
          <TextAreaField
            label="Link domains"
            hint="One per line. Subdomains are included."
            rows={3}
            value={domainText}
            onChange={(event) => setDomains(event.target.value)}
            disabled={readOnly}
            error={tried ? problems.domains : undefined}
          />
          {readOnly || suggestions.length === 0 ? null : (
            <div className="row" role="group" aria-label="Verified sending domains">
              {suggestions.map((entry) => (
                <Button key={entry.id} onClick={() => setDomains([...draft.domains, entry.domain].join("\n"))}>
                  Add {entry.domain}
                </Button>
              ))}
            </div>
          )}
          <div className="stack">
            <span className="field-label">Example link</span>
            {hasExample ? (
              <code className="mono" aria-label="Example link">
                {exampleLink(draft, workspaceSlug)}
              </code>
            ) : (
              <p className="faint">Fix the patterns to see an example link.</p>
            )}
            <span className="field-hint">
              Built from the current values with a sample flow and template. A preview or a test send has no flow, so the flow placeholders are
              empty there.
            </span>
          </div>
        </div>
      </Panel>
      {failure ? <InlineError {...failure} /> : null}
      {notice ? (
        <p className="notice" role="status">
          {notice}
        </p>
      ) : null}
      {readOnly ? (
        <p className="muted">The UTM parameters can be changed by workspace admins and the instance owner only.</p>
      ) : (
        <div className="row">
          <Button type="submit" variant="primary" disabled={busy || !dirty}>
            {busy ? "Saving" : "Save UTM parameters"}
          </Button>
        </div>
      )}
    </form>
  );
}
