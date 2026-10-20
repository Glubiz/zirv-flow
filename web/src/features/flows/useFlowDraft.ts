import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import { client, describeError, unwrap, type ErrorSummary } from "../../api";
import { sameJson, TRIGGER_ID, type Draft, type FlowDetail, type FlowIssue } from "./model";

/** How long after the last edit the draft is saved. */
const AUTOSAVE_DELAY_MS = 800;
/** Edits to one field closer together than this are one step of undo. */
const COALESCE_MS = 1000;
const UNDO_LIMIT = 100;

type SaveStatus = "saved" | "saving" | "error";

interface Meta {
  state: FlowDetail["state"];
  currentVersion: number | null;
}

interface FlowDraft {
  draft: Draft;
  /** Changes the draft. Edits to the same `key` in quick succession count as one step of undo. */
  edit: (change: (draft: Draft) => Draft, key?: string) => void;
  undo: () => void;
  canUndo: boolean;
  status: SaveStatus;
  failure: ErrorSummary | null;
  retry: () => void;
  /** Whether the draft holds changes the server does not have yet. */
  unsaved: boolean;
  /**
   * The issues of the saved draft, less those of a step or connection the draft no longer has; null
   * until the first check, and always null for a reader.
   */
  issues: FlowIssue[] | null;
  /** Whether an edit is waiting for the check of its save, so `issues` may be out of date. */
  checking: boolean;
  meta: Meta;
  /** How many saves have gone through since the editor opened. */
  saves: number;
}

interface Options {
  workspaceId: string;
  flow: FlowDetail;
  /** Whether the person may change the flow. A reader's draft is never saved or checked. */
  editable: boolean;
}

/** Whether a ref holds something; read through a function so a check after an await is not folded away. */
function isSet(ref: { current: unknown }): boolean {
  return ref.current !== null;
}

function draftOf(flow: FlowDetail): Draft {
  return { name: flow.name, graph: flow.graph, settings: flow.settings };
}

/**
 * The draft of a flow being edited: every edit updates it at once and a debounced PATCH saves it,
 * one request at a time with the latest draft winning. After each save that leaves nothing newer
 * waiting, the saved draft is checked with the validate endpoint.
 */
export function useFlowDraft({ workspaceId, flow, editable }: Options): FlowDraft {
  const [draft, setDraft] = useState<Draft>(() => draftOf(flow));
  const [saved, setSaved] = useState<Draft>(() => draftOf(flow));
  const [status, setStatus] = useState<SaveStatus>("saved");
  const [failure, setFailure] = useState<ErrorSummary | null>(null);
  const [issues, setIssues] = useState<FlowIssue[] | null>(null);
  const [checking, setChecking] = useState(false);
  const [saves, setSaves] = useState(0);
  const [meta, setMeta] = useState<Meta>({ state: flow.state, currentVersion: flow.current_version ?? null });
  const history = useRef<Draft[]>([]);
  const [undoDepth, setUndoDepth] = useState(0);

  const draftRef = useRef(draft);
  const savedRef = useRef(saved);
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const inflight = useRef(false);
  const checks = useRef(0);
  /** Counts every change of the draft, so a save can tell whether a newer edit came in while it ran. */
  const edits = useRef(0);
  const lastEdit = useRef<{ key: string; at: number } | null>(null);
  const flowId = flow.id;
  const flushRef = useRef<() => Promise<void>>(() => Promise.resolve());

  const check = useCallback(async () => {
    checks.current += 1;
    const mine = checks.current;
    try {
      const body = await unwrap(
        client.POST("/api/v1/workspaces/{ws}/flows/{id}/validate", { params: { path: { ws: workspaceId, id: flowId } } }),
      );
      if (mine === checks.current) {
        setIssues(body.issues);
        setChecking(false);
      }
    } catch {
      // The editor stays usable without the check; the issues shown are those of the last one.
      if (mine === checks.current) setChecking(false);
    }
  }, [workspaceId, flowId]);

  const flush = useCallback(async () => {
    if (timer.current !== null) {
      clearTimeout(timer.current);
      timer.current = null;
    }
    if (inflight.current) return;
    const sent = draftRef.current;
    const base = savedRef.current;
    const name = sent.name.trim();
    const body = {
      ...(name !== "" && name !== base.name ? { name } : {}),
      ...(sameJson(sent.graph, base.graph) ? {} : { graph: sent.graph }),
      ...(sameJson(sent.settings, base.settings) ? {} : { settings: sent.settings }),
    };
    if (Object.keys(body).length === 0) {
      setStatus("saved");
      void check();
      return;
    }
    const sentEdit = edits.current;
    inflight.current = true;
    setStatus("saving");
    try {
      const updated = await unwrap(
        client.PATCH("/api/v1/workspaces/{ws}/flows/{id}", { params: { path: { ws: workspaceId, id: flowId } }, body }),
      );
      const now: Draft = { name: updated.name, graph: sent.graph, settings: sent.settings };
      savedRef.current = now;
      setSaved(now);
      setMeta({ state: updated.state, currentVersion: updated.current_version ?? null });
      setSaves((count) => count + 1);
      setFailure(null);
      inflight.current = false;
      const latest = draftRef.current;
      const newer =
        latest.name.trim() !== now.name ||
        !sameJson(latest.graph, now.graph) ||
        !sameJson(latest.settings, now.settings);
      if (isSet(timer)) return;
      if (newer) {
        void flushRef.current();
        return;
      }
      setStatus("saved");
      void check();
    } catch (error) {
      inflight.current = false;
      setFailure(describeError(error));
      setStatus("error");
      // A newer edit is still to be saved and checked, so the issues shown stay stale until then.
      if (edits.current === sentEdit) setChecking(false);
      else if (!isSet(timer)) void flushRef.current();
    }
  }, [workspaceId, flowId, check]);

  useEffect(() => {
    flushRef.current = flush;
  });

  const schedule = useCallback(() => {
    if (timer.current !== null) clearTimeout(timer.current);
    setStatus("saving");
    timer.current = setTimeout(() => {
      timer.current = null;
      void flush();
    }, AUTOSAVE_DELAY_MS);
  }, [flush]);

  useEffect(() => {
    const first = editable ? setTimeout(() => void check(), 0) : null;
    return () => {
      if (first !== null) clearTimeout(first);
      checks.current += 1;
      if (timer.current !== null) clearTimeout(timer.current);
    };
  }, [editable, check]);

  const edit = useCallback(
    (change: (draft: Draft) => Draft, key?: string) => {
      if (!editable) return;
      const before = draftRef.current;
      const next = change(before);
      if (sameJson(next, before)) return;
      const now = Date.now();
      const last = lastEdit.current;
      const joins = key !== undefined && last !== null && last.key === key && now - last.at < COALESCE_MS;
      lastEdit.current = key === undefined ? null : { key, at: now };
      if (!joins) {
        history.current = [...history.current.slice(-(UNDO_LIMIT - 1)), before];
        setUndoDepth(history.current.length);
      }
      draftRef.current = next;
      edits.current += 1;
      setDraft(next);
      // Whatever check is on its way answers for a draft that is gone.
      checks.current += 1;
      setChecking(true);
      schedule();
    },
    [editable, schedule],
  );

  const undo = useCallback(() => {
    if (!editable) return;
    const previous = history.current.at(-1);
    if (previous === undefined) return;
    history.current = history.current.slice(0, -1);
    setUndoDepth(history.current.length);
    draftRef.current = previous;
    edits.current += 1;
    setDraft(previous);
    lastEdit.current = null;
    checks.current += 1;
    setChecking(true);
    schedule();
  }, [editable, schedule]);

  const retry = useCallback(() => {
    void flush();
  }, [flush]);

  const current = useMemo(() => {
    if (issues === null) return null;
    const nodes = new Set(draft.graph.nodes.map((node) => node.id));
    const edges = new Set(draft.graph.edges.map((edge) => edge.id));
    return issues.filter(
      (issue) =>
        (issue.node === undefined || issue.node === TRIGGER_ID || nodes.has(issue.node)) &&
        (issue.edge === undefined || edges.has(issue.edge)),
    );
  }, [issues, draft.graph]);

  return {
    draft,
    edit,
    undo,
    canUndo: undoDepth > 0,
    status,
    failure,
    retry,
    unsaved:
      draft.name.trim() !== saved.name || !sameJson(draft.graph, saved.graph) || !sameJson(draft.settings, saved.settings),
    issues: editable ? current : null,
    checking: editable && checking,
    meta,
    saves,
  };
}
