import { useState } from "react";

import { client, describeError, unwrap, useResource, type components, type ErrorSummary } from "../../api";
import { Button, CopyButton, Dialog, formatDateTime, InlineError, Panel, Pill, ResourceView } from "../../ui";

type Issued = components["schemas"]["IssuedSigningSecretBody"];

function graceText(seconds: number): string {
  if (seconds % 3600 === 0) {
    const hours = seconds / 3600;
    return `${hours} ${hours === 1 ? "hour" : "hours"}`;
  }
  return `${Math.round(seconds / 60)} minutes`;
}

/**
 * The workspace's webhook signing secret. The secret is returned once, by create and by rotate: it
 * lives only in this component's state while the dialog is open, and nowhere else.
 */
export function SigningSecretPanel({ workspaceId }: { workspaceId: string }) {
  const status = useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/webhook-signing-secret", { params: { path: { ws: workspaceId } } })),
    [workspaceId],
  );
  const [confirming, setConfirming] = useState(false);
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);
  const [issued, setIssued] = useState<Issued | null>(null);

  async function issue(rotate: boolean) {
    setBusy(true);
    setFailure(null);
    try {
      const params = { params: { path: { ws: workspaceId } } };
      const result = rotate
        ? await unwrap(client.POST("/api/v1/workspaces/{ws}/webhook-signing-secret/rotate", params))
        : await unwrap(client.POST("/api/v1/workspaces/{ws}/webhook-signing-secret", params));
      setConfirming(false);
      setIssued(result);
      status.reload();
    } catch (error) {
      setFailure(describeError(error));
    } finally {
      setBusy(false);
    }
  }

  return (
    <Panel title="Webhook signing secret">
      <ResourceView resource={status} loadingLabel="Loading the signing secret">
        {(data) => (
          <div className="stack">
            <p className="muted">
              Signs the calls of webhook nodes in flows and of forwarding to a webhook destination. A flow with a webhook node
              cannot be published without one. The secret is shown once, when it is made.
            </p>
            <div className="row">
              <Pill tone={data.configured ? "neutral" : "attention"}>{data.configured ? "Configured" : "Not set up"}</Pill>
              {data.created_at ? <span className="muted">Made {formatDateTime(data.created_at)}</span> : null}
              {data.previous_valid_until ? (
                <span className="muted">Previous secret still signs until {formatDateTime(data.previous_valid_until)}</span>
              ) : null}
            </div>
            {failure && !confirming ? <InlineError title={failure.title} detail={failure.detail} traceId={failure.traceId} /> : null}
            <div className="row">
              {data.configured ? (
                <Button onClick={() => setConfirming(true)}>Rotate the secret</Button>
              ) : (
                <Button variant="primary" onClick={() => void issue(false)} disabled={busy}>
                  Create the secret
                </Button>
              )}
            </div>
            {confirming ? (
              <Dialog title="Rotate the signing secret?" onClose={() => setConfirming(false)}>
                <div className="stack">
                  <p className="muted">
                    A new secret signs from now on. The previous one keeps signing for {graceText(data.grace_seconds)}, so
                    receivers can switch over without a gap.
                  </p>
                  {failure ? <InlineError title={failure.title} detail={failure.detail} traceId={failure.traceId} /> : null}
                  <div className="dialog-actions">
                    <Button onClick={() => setConfirming(false)}>Cancel</Button>
                    <Button variant="primary" onClick={() => void issue(true)} disabled={busy}>
                      Rotate now
                    </Button>
                  </div>
                </div>
              </Dialog>
            ) : null}
          </div>
        )}
      </ResourceView>
      {issued ? (
        <Dialog title="Your signing secret" onClose={() => setIssued(null)} wide>
          <div className="stack">
            <p className="notice" role="alert">
              Copy the secret now. It is not shown again; if you lose it, rotate to make a new one.
            </p>
            <div className="row">
              <span className="secret" data-testid="signing-secret">
                {issued.secret}
              </span>
              <CopyButton value={issued.secret} label="Copy the signing secret" />
            </div>
            {issued.previous_valid_until ? (
              <p className="muted">
                The previous secret keeps signing for {graceText(issued.grace_seconds)}, until{" "}
                {formatDateTime(issued.previous_valid_until)}.
              </p>
            ) : null}
            <div className="dialog-actions">
              <Button variant="primary" onClick={() => setIssued(null)}>
                I have saved the secret
              </Button>
            </div>
          </div>
        </Dialog>
      ) : null}
    </Panel>
  );
}
