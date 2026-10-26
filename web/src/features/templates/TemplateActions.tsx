import { useEffect, useState } from "react";
import { useNavigate } from "react-router";

import { ApiError, client, describeError, unwrap, type ErrorSummary } from "../../api";
import { Button, Dialog, InlineError, MenuButton, type MenuEntry } from "../../ui";
import { plural } from "../../ui/format";
import type { TemplateListItem } from "./library";

/** "Used by A, B. Remove it from the flows first." */
export function usedByLine(names: readonly string[]): string {
  return `Used by ${names.join(", ")}. Remove it from the flows first.`;
}

/**
 * The names of the active flows that send a template, read once the row's menu is asked for. `null`
 * until they are in; the count in the list is enough for the line before that.
 */
function useFlowNames(workspaceId: string, item: TemplateListItem, wanted: boolean): string[] | null {
  const [names, setNames] = useState<string[] | null>(null);
  const needed = wanted && item.active_flows > 0;
  useEffect(() => {
    if (!needed) return;
    let live = true;
    unwrap(client.GET("/api/v1/workspaces/{ws}/templates/{id}/where-used", { params: { path: { ws: workspaceId, id: item.id } } })).then(
      (uses) => {
        if (live) setNames(uses.filter((use) => use.active && use.flow_state === "published").map((use) => use.flow_name));
      },
      () => undefined,
    );
    return () => {
      live = false;
    };
  }, [needed, workspaceId, item.id]);
  return names;
}

interface MenuProps {
  workspaceId: string;
  item: TemplateListItem;
  canWrite: boolean;
  canAdmin: boolean;
  onDuplicate: () => void;
  onArchive: () => void;
}

/** The ⋯ menu of a template. */
export function TemplateMenu({ workspaceId, item, canWrite, canAdmin, onDuplicate, onArchive }: MenuProps) {
  const navigate = useNavigate();
  const [asked, setAsked] = useState(false);
  const names = useFlowNames(workspaceId, item, asked);
  const base = `/w/${workspaceId}/templates/${item.id}`;
  const archived = item.archived_at !== null && item.archived_at !== undefined;
  const entries: MenuEntry[] = [
    { key: "open", label: "Open", onSelect: () => void navigate(base) },
    ...(canWrite && !archived ? [{ key: "duplicate", label: "Duplicate", onSelect: onDuplicate }] : []),
    ...(canWrite && !archived ? [{ key: "test", label: "Send a test…", onSelect: () => void navigate(`${base}/test`) }] : []),
    { key: "used", label: "Where it is used", onSelect: () => void navigate(`${base}?tab=used`) },
    ...(canAdmin && !archived
      ? [
          item.active_flows > 0
            ? {
                key: "archive",
                label: "Archive",
                onSelect: onArchive,
                disabled: true,
                reason: names && names.length > 0 ? usedByLine(names) : `Used by ${plural(item.active_flows, "flow", "flows")}. Remove it from the flows first.`,
              }
            : { key: "archive", label: "Archive", onSelect: onArchive },
        ]
      : []),
  ];
  return (
    <span onClickCapture={() => setAsked(true)}>
      <MenuButton
        label={`Actions for ${item.name}`}
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
    </span>
  );
}

interface ArchiveProps {
  workspaceId: string;
  item: TemplateListItem;
  onClose: () => void;
  onDone: () => void;
}

/** Archiving asks first. If a flow started sending the template since the list was read, the server's refusal names the flows. */
export function ArchiveTemplateDialog({ workspaceId, item, onClose, onDone }: ArchiveProps) {
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);
  const [flows, setFlows] = useState<string[]>([]);

  async function archive() {
    setBusy(true);
    setFailure(null);
    try {
      await unwrap(client.DELETE("/api/v1/workspaces/{ws}/templates/{id}", { params: { path: { ws: workspaceId, id: item.id } } }));
      onDone();
    } catch (error) {
      if (error instanceof ApiError && error.code === "template_in_use") {
        setFlows(error.flows.map((flow) => flow.name));
      } else {
        setFailure(describeError(error));
      }
      setBusy(false);
    }
  }

  return (
    <Dialog title={`Archive “${item.name}”?`} onClose={onClose}>
      <div className="stack">
        <p>It leaves the library and cannot be used in a send step. Its versions and draft stay readable, and its name is free for another template.</p>
        {flows.length > 0 ? (
          <p role="alert" className="field-error">
            {usedByLine(flows)}
          </p>
        ) : null}
        {failure ? <InlineError {...failure} /> : null}
        <div className="dialog-actions">
          <Button onClick={onClose} disabled={busy}>
            Cancel
          </Button>
          <Button variant="primary" onClick={() => void archive()} disabled={busy || flows.length > 0}>
            {busy ? "Archiving" : "Archive template"}
          </Button>
        </div>
      </div>
    </Dialog>
  );
}
