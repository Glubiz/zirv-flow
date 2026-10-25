import { useState } from "react";

import { client, describeError, unwrap, type ErrorSummary } from "../../api";
import { Button, Dialog, ErrorState, InlineError, LoadingState } from "../../ui";
import type { Template, TemplateVersion } from "./model";
import { UseList, useWhereUsed } from "./WhereUsed";

interface Props {
  workspaceId: string;
  template: Template;
  /** Saves any pending edit first; resolves false when it could not. */
  flush: () => Promise<boolean>;
  onClose: () => void;
  onPublished: (template: Template, version: TemplateVersion) => void;
}

/** Publishing makes a version that every send step of the template uses from its next send, so the dialog says which flows are running it. */
export function PublishDialog({ workspaceId, template, flush, onClose, onPublished }: Props) {
  const uses = useWhereUsed(workspaceId, template.id);
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);

  async function publish() {
    setBusy(true);
    setFailure(null);
    try {
      if (!(await flush())) {
        setFailure({ title: "The draft could not be saved", detail: "Fix that, then publish again.", traceId: null });
        setBusy(false);
        return;
      }
      const published = await unwrap(
        client.POST("/api/v1/workspaces/{ws}/templates/{id}/publish", { params: { path: { ws: workspaceId, id: template.id } } }),
      );
      onPublished(published.template, published.version);
    } catch (error) {
      setFailure(describeError(error));
      setBusy(false);
    }
  }

  const active = uses.status === "ready" ? uses.data.filter((use) => use.active) : [];
  const next = (template.current_version ?? 0) + 1;

  return (
    <Dialog title={`Publish version ${next}`} onClose={onClose}>
      <div className="stack">
        {uses.status === "loading" ? <LoadingState label="Checking which flows use this template" /> : null}
        {uses.status === "error" ? <ErrorState {...describeError(uses.error)} onRetry={uses.reload} /> : null}
        {uses.status === "ready" && active.length === 0 ? <p className="muted">No active flow uses this template yet.</p> : null}
        {active.length > 0 ? (
          <>
            <p className="muted">These active flows send this template. From the next send they use the version you publish.</p>
            <UseList workspaceId={workspaceId} uses={active} />
          </>
        ) : null}
        {failure ? <InlineError {...failure} /> : null}
        <div className="dialog-actions">
          <Button onClick={onClose}>Cancel</Button>
          <Button variant="primary" onClick={() => void publish()} disabled={busy || uses.status === "loading"}>
            {busy ? "Publishing" : `Publish version ${next}`}
          </Button>
        </div>
      </div>
    </Dialog>
  );
}
