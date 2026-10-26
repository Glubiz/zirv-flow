import { useState } from "react";

import { client, describeError, unwrap, type ErrorSummary } from "../../api";
import { Button, Dialog, ErrorState, InlineError, LoadingState, MenuButton, type MenuEntry } from "../../ui";
import { plural } from "../../ui/format";
import type { Theme } from "./model";
import { useThemeUsedBy } from "./useThemes";

interface Named {
  id: string;
  name: string;
}

/** The ⋯ menu of a theme: Set as default and Archive, for admins only. */
export function ThemeMenu({
  theme,
  canAdmin,
  onSetDefault,
  onArchive,
}: {
  theme: Pick<Theme, "name" | "is_default">;
  canAdmin: boolean;
  onSetDefault: () => void;
  onArchive: () => void;
}) {
  if (!canAdmin) return null;
  const entries: MenuEntry[] = [
    ...(theme.is_default ? [] : [{ key: "default", label: "Set as default", onSelect: onSetDefault }]),
    {
      key: "archive",
      label: "Archive",
      onSelect: onArchive,
      disabled: theme.is_default,
      reason: theme.is_default ? "The default theme cannot be archived. Make another theme the default first." : undefined,
    },
  ];
  return (
    <MenuButton
      label={`More actions for ${theme.name}`}
      buttonClassName="btn btn-ghost th-more"
      buttonContent={
        <svg aria-hidden="true" width="16" height="16" viewBox="0 0 16 16" fill="currentColor">
          <circle cx="3.5" cy="8" r="1.3" />
          <circle cx="8" cy="8" r="1.3" />
          <circle cx="12.5" cy="8" r="1.3" />
        </svg>
      }
      entries={entries}
    />
  );
}

function Followers({ workspaceId, current }: { workspaceId: string; current: Named }) {
  const used = useThemeUsedBy(workspaceId, current.id);
  if (used.status === "loading") return <LoadingState label="Checking which templates follow the default" />;
  if (used.status === "error") return <ErrorState {...describeError(used.error)} onRetry={used.reload} />;
  return (
    <p>
      {plural(used.data.default_templates.length, "template", "templates")} follow the default today. They use the new default from their next send.
    </p>
  );
}

/** Making a theme the default moves every email that follows the default to it. */
export function SetDefaultDialog({
  workspaceId,
  theme,
  current,
  onClose,
  onDone,
}: {
  workspaceId: string;
  theme: Named;
  /** The theme that is the default now, if there is one. */
  current: Named | null;
  onClose: () => void;
  onDone: (theme: Theme) => void;
}) {
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);

  async function confirm() {
    setBusy(true);
    setFailure(null);
    try {
      onDone(await unwrap(client.PUT("/api/v1/workspaces/{ws}/themes/default", { params: { path: { ws: workspaceId } }, body: { theme_id: theme.id } })));
    } catch (error) {
      setFailure(describeError(error));
      setBusy(false);
    }
  }

  return (
    <Dialog title={`Make ${theme.name} the default?`} onClose={onClose}>
      <div className="stack">
        {current ? <Followers workspaceId={workspaceId} current={current} /> : <p>No theme is the default yet.</p>}
        {failure ? <InlineError {...failure} /> : null}
        <div className="dialog-actions">
          <Button onClick={onClose}>Cancel</Button>
          <Button variant="primary" onClick={() => void confirm()} disabled={busy}>
            {busy ? "Saving" : "Make default"}
          </Button>
        </div>
      </div>
    </Dialog>
  );
}

export function ArchiveDialog({ workspaceId, theme, onClose, onArchived }: { workspaceId: string; theme: Named; onClose: () => void; onArchived: () => void }) {
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);

  async function archive() {
    setBusy(true);
    setFailure(null);
    try {
      await unwrap(client.DELETE("/api/v1/workspaces/{ws}/themes/{id}", { params: { path: { ws: workspaceId, id: theme.id } } }));
      onArchived();
    } catch (error) {
      setFailure(describeError(error));
      setBusy(false);
    }
  }

  return (
    <Dialog title={`Archive ${theme.name}?`} onClose={onClose}>
      <div className="stack">
        <p className="muted">An archived theme is kept, with its versions, but cannot be edited or chosen by a template.</p>
        {failure ? <InlineError {...failure} /> : null}
        <div className="dialog-actions">
          <Button onClick={onClose}>Cancel</Button>
          <Button variant="primary" onClick={() => void archive()} disabled={busy}>
            {busy ? "Archiving" : "Archive"}
          </Button>
        </div>
      </div>
    </Dialog>
  );
}
