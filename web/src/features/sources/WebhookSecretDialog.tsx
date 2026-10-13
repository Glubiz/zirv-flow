import { Button, CopyButton, Dialog } from "../../ui";
import { curlExample, webhookUrl, type Webhook } from "./labels";

interface WebhookSecretDialogProps {
  title: string;
  hook: Webhook;
  secret: string;
  onClose: () => void;
}

/** Shows the URL and the secret of a webhook. The secret is shown here once and cannot be read back. */
export function WebhookSecretDialog({ title, hook, secret, onClose }: WebhookSecretDialogProps) {
  const url = webhookUrl(window.location.origin, hook);
  const example = curlExample(url, hook, secret);
  return (
    <Dialog title={title} onClose={onClose} wide>
      <div className="stack">
        <p className="notice" role="alert">
          Copy the secret now. It is not shown again; if you lose it, issue a new one.
        </p>
        <dl className="facts">
          <div className="fact">
            <dt>URL</dt>
            <dd className="row">
              <span className="mono">{url}</span>
              <CopyButton value={url} label="Copy the URL" />
            </dd>
          </div>
          <div className="fact">
            <dt>Secret</dt>
            <dd className="row">
              <span className="mono">{secret}</span>
              <CopyButton value={secret} label="Copy the secret" />
            </dd>
          </div>
        </dl>
        <div className="stack-tight">
          <h3>Try it</h3>
          <p className="muted">This shell command signs a sample payload and sends it.</p>
          <pre className="mono" aria-label="Example command">
            {example}
          </pre>
          <div>
            <CopyButton value={example} label="Copy the example command" />
          </div>
        </div>
        <div className="dialog-actions">
          <Button variant="primary" onClick={onClose}>
            I have saved the secret
          </Button>
        </div>
      </div>
    </Dialog>
  );
}
