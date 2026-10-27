import { useState } from "react";

import { ApiError, client, describeError, unwrap, type ErrorSummary } from "../../api";
import { Button, Dialog, InlineError } from "../../ui";
import type { TemplateDetail } from "./model";

interface Props {
  workspaceId: string;
  templateId: string;
  /** Saves any pending edit first; resolves false when it could not. */
  flush: () => Promise<boolean>;
  /** The draft revision the editor holds, read once the pending edit is saved. */
  revision: () => number;
  /** The draft was saved by someone else first; nothing was converted. */
  onConflict: () => void;
  onClose: () => void;
  onConverted: (detail: TemplateDetail) => void;
}

/** Converting is one way, so the dialog says that before it does anything. */
export function ConvertDialog({ workspaceId, templateId, flush, revision, onConflict, onClose, onConverted }: Props) {
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);

  async function convert() {
    setBusy(true);
    setFailure(null);
    try {
      if (!(await flush())) {
        setFailure({ title: "The draft could not be saved", detail: "Fix that, then convert again.", traceId: null });
        setBusy(false);
        return;
      }
      const detail = await unwrap(
        client.POST("/api/v1/workspaces/{ws}/templates/{id}/draft/convert-to-mjml", { params: { path: { ws: workspaceId, id: templateId } }, body: { revision: revision() } }),
      );
      onConverted(detail);
    } catch (error) {
      if (error instanceof ApiError && error.code === "template_draft_revision_mismatch") {
        onConflict();
        onClose();
        return;
      }
      setFailure(describeError(error));
      setBusy(false);
    }
  }

  return (
    <Dialog title="Convert to MJML code?" onClose={onClose}>
      <div className="stack">
        <p>
          The draft becomes MJML code with your merge tags kept. <strong>This is one way:</strong> code cannot become blocks again, and you will edit the template as code from now on.
        </p>
        <p className="muted">Published versions are not changed. Only the draft is converted.</p>
        {failure ? <InlineError {...failure} /> : null}
        <div className="dialog-actions">
          <Button onClick={onClose} disabled={busy}>
            Cancel
          </Button>
          <Button variant="primary" onClick={() => void convert()} disabled={busy}>
            {busy ? "Converting" : "Convert to MJML"}
          </Button>
        </div>
      </div>
    </Dialog>
  );
}
