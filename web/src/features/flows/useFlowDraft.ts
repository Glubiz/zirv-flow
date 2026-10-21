import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import { client, describeError, unwrap, type ErrorSummary } from "../../api";
import { sameJson, TRIGGER_ID, type Draft, type FlowDetail, type FlowIssue } from "./model";

/** How long after the last edit the draft is saved. */
const AUTOSAVE_DELAY_MS = 800;
/** Edits to one field closer together than this are one step of undo. */
const COALESCE_MS = 1000;
const UNDO_LIMIT = 100;

type SaveStatus = "saved" | "saving" | "error";

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
  /** Whether the draft's settings (trigger, goal, exit) differ from the saved ones, as after a failed save. */
  settingsUnsaved: boolean;
  /**
   * The issues of the saved draft, less those of a step or connection the draft no longer has; null
   * until the first check, and always null for a reader.
   */
  issues: FlowIssue[] | null;
  /** Whether an edit is waiting for the check of its save, so `issues` may be out of date. */
  checking: boolean;
  /**
   * Saves what is waiting and resolves once the server has it: true when it does, false when a save
   * failed. Resolves true at once when nothing is waiting.
   */
  saveNow: () => Promise<boolean>;
  /** Shows these issues as the check of the saved draft, as a refused publish reports them. */
  adoptIssues: (issues: FlowIssue[]) => void;
  /** How many saves have gone through since the editor opened. */
  saves: number;
  /**
   * Makes ready to replace the draft with one the server wrote: stops the waiting save and lets one
   * that is on its way finish. The unsaved draft and its undo history are kept until `adopt` replaces
   * them. Until `adopt` or `release` the editor takes no edits and starts no save, so nothing races
   * the replacement.
   */
  hold: () => Promise<void>;
  /** Whether a hold is on: the editor is read-only until the replacement lands or is given up. */
  held: boolean;
  /** Shows a draft the server already holds (the graph and settings) as both the draft and the saved state, and ends the hold. */
  adopt: (document: { graph: Draft["graph"]; settings: Draft["settings"] }) => void;
  /** Ends a hold without replacing anything: the draft is exactly as it was, and an edit that was waiting to be saved is saved. */
  release: () => void;
  /**
   * For an accept whose outcome is not known: reads the flow from the server and compares its draft
   * with the saved state from before the hold. Unchanged, the accept did not commit and the hold is
   * released. Changed, the accept did commit and its reply was lost, so the server's draft is adopted.
   * When the flow cannot be read either, the hold is released with the edits marked unsaved and
   * nothing is saved until the person retries.
   */
  reconcile: () => Promise<"unchanged" | "adopted" | "unknown">;
}

interface Options {
  workspaceId: string;
  flow: FlowDetail;
  /** Whether the person may change the flow. A reader's draft is never saved or checked. */
  editable: boolean;
}

/** Whether a flag is on; read through a function so a check after an await is not folded away. */
function isOn(ref: { current: boolean }): boolean {
  return ref.current;
}

/** Whether a ref holds something; read through a function so a check after an await is not folded away. */
function isSet(ref: { current: unknown }): boolean {
  return ref.current !== null;
}

/** The same JSON whatever the order of the keys, which a server need not keep. */
function canonical(value: unknown): string {
  if (Array.isArray(value)) return `[${value.map(canonical).join(",")}]`;
  if (typeof value === "object" && value !== null) {
    const record = value as Record<string, unknown>;
    return `{${Object.keys(record)
      .sort()
      .map((key) => `${JSON.stringify(key)}:${canonical(record[key])}`)
      .join(",")}}`;
  }
  return JSON.stringify(value);
}

function sameCanonical(left: unknown, right: unknown): boolean {
  return canonical(left) === canonical(right);
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
  /** The save under way, so a caller can wait for it. */
  const running = useRef<Promise<void> | null>(null);
  /** Whether the last save failed. */
  const failed = useRef(false);
  /** Set between `hold` and `adopt`/`release`: no edit is taken and no save starts. */
  const holding = useRef(false);
  const [held, setHeld] = useState(false);
  /** The saved state when the hold began, to tell whether an accept that failed still committed. */
  const baseAtHold = useRef<Draft | null>(null);
  const mounted = useRef(true);
  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
    };
  }, []);

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

  const run = useCallback(async () => {
    if (timer.current !== null) {
      clearTimeout(timer.current);
      timer.current = null;
    }
    if (inflight.current || holding.current) return;
    const sent = draftRef.current;
    const base = savedRef.current;
    const name = sent.name.trim();
    const body = {
      ...(name !== "" && name !== base.name ? { name } : {}),
      ...(sameJson(sent.graph, base.graph) ? {} : { graph: sent.graph }),
      ...(sameJson(sent.settings, base.settings) ? {} : { settings: sent.settings }),
    };
    if (Object.keys(body).length === 0) {
      // The draft is the saved state, so an earlier failed save has nothing left to retry.
      failed.current = false;
      setFailure(null);
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
      setSaves((count) => count + 1);
      setFailure(null);
      failed.current = false;
      inflight.current = false;
      const latest = draftRef.current;
      const newer =
        latest.name.trim() !== now.name ||
        !sameJson(latest.graph, now.graph) ||
        !sameJson(latest.settings, now.settings);
      if (isSet(timer)) return;
      if (isOn(holding)) return;
      if (newer) {
        void flushRef.current();
        return;
      }
      setStatus("saved");
      void check();
    } catch (error) {
      inflight.current = false;
      failed.current = true;
      setFailure(describeError(error));
      setStatus("error");
      // A newer edit is still to be saved and checked, so the issues shown stay stale until then.
      if (edits.current === sentEdit) setChecking(false);
      else if (!isSet(timer) && !isOn(holding)) void flushRef.current();
    }
  }, [workspaceId, flowId, check]);

  const flush = useCallback(() => {
    if (inflight.current) return Promise.resolve();
    const promise = run();
    running.current = promise;
    void promise.then(() => {
      if (running.current === promise) running.current = null;
    });
    return promise;
  }, [run]);

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
      if (!editable || holding.current) return;
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
    if (!editable || holding.current) return;
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

  const saveNow = useCallback(async (): Promise<boolean> => {
    // An accepted draft is on its way: a save now could write the old graph over it.
    if (holding.current) return false;
    for (let round = 0; round < 20; round += 1) {
      if (running.current !== null) {
        await running.current;
      } else {
        const latest = draftRef.current;
        const base = savedRef.current;
        const waiting =
          isSet(timer) ||
          (latest.name.trim() !== "" && latest.name.trim() !== base.name) ||
          !sameJson(latest.graph, base.graph) ||
          !sameJson(latest.settings, base.settings);
        if (!waiting) return true;
        await flushRef.current();
      }
      if (isOn(holding) || failed.current) return false;
    }
    return false;
  }, []);

  const adoptIssues = useCallback((found: FlowIssue[]) => {
    checks.current += 1;
    setChecking(false);
    setIssues(found);
  }, []);

  const hold = useCallback(async () => {
    holding.current = true;
    setHeld(true);
    if (timer.current !== null) {
      clearTimeout(timer.current);
      timer.current = null;
    }
    while (running.current !== null) await running.current;
    baseAtHold.current = savedRef.current;
  }, []);

  const release = useCallback(() => {
    holding.current = false;
    setHeld(false);
    const latest = draftRef.current;
    const base = savedRef.current;
    const waiting =
      latest.name.trim() !== base.name || !sameJson(latest.graph, base.graph) || !sameJson(latest.settings, base.settings);
    if (waiting && mounted.current) schedule();
  }, [schedule]);

  const adopt = useCallback(
    (document: { graph: Draft["graph"]; settings: Draft["settings"] }) => {
      const before = draftRef.current;
      // The accepted draft carries no name, so an unsaved rename stays and is saved like any edit.
      const next: Draft = { name: before.name, graph: document.graph, settings: document.settings };
      const base: Draft = { ...next, name: savedRef.current.name };
      history.current = [...history.current.slice(-(UNDO_LIMIT - 1)), before];
      setUndoDepth(history.current.length);
      lastEdit.current = null;
      draftRef.current = next;
      savedRef.current = base;
      edits.current += 1;
      setDraft(next);
      setSaved(base);
      setSaves((count) => count + 1);
      failed.current = false;
      setFailure(null);
      setStatus("saved");
      holding.current = false;
      setHeld(false);
      void check();
      if (next.name.trim() !== base.name && mounted.current) schedule();
    },
    [check, schedule],
  );

  const reconcile = useCallback(async (): Promise<"unchanged" | "adopted" | "unknown"> => {
    const base = baseAtHold.current;
    try {
      const server = await unwrap(
        client.GET("/api/v1/workspaces/{ws}/flows/{id}", { params: { path: { ws: workspaceId, id: flowId } } }),
      );
      if (base !== null && sameCanonical(server.graph, base.graph) && sameCanonical(server.settings, base.settings)) {
        release();
        return "unchanged";
      }
      adopt({ graph: server.graph, settings: server.settings });
      return "adopted";
    } catch (error) {
      // Whether the accept committed is not known, so saving the old edits could overwrite it: they wait for a retry.
      holding.current = false;
      setHeld(false);
      failed.current = true;
      setFailure(describeError(error));
      setStatus("error");
      return "unknown";
    }
  }, [workspaceId, flowId, release, adopt]);

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
    settingsUnsaved: !sameJson(draft.settings, saved.settings),
    issues: editable ? current : null,
    checking: editable && checking,
    saveNow,
    adoptIssues,
    saves,
    hold,
    held,
    adopt,
    release,
    reconcile,
  };
}
