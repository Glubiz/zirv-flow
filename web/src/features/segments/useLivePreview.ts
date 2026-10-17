import { useCallback, useEffect, useRef, useState } from "react";

import { ApiError, client, describeError, unwrap, type components, type DefinitionIssue, type ErrorSummary } from "../../api";
import { toDefinition, type Node } from "./ast";

/** How long the definition rests before a count is asked for. */
export const PREVIEW_DELAY_MS = 600;

/**
 * What `POST /segments/preview` answers, as `PreviewBody` in `crates/zirv-flow-api/src/segments/preview.rs`.
 * The generated client types that path with the source-stream preview, because the two Rust structs
 * share a schema name in the OpenAPI document, so the answer is typed here instead.
 */
export interface PreviewResult {
  count: number;
  sample: { id: string; display?: components["schemas"]["DisplayBody"] | null }[];
  data_age: components["schemas"]["DataAgeBody"];
  compiled_at: string;
}

const SAMPLE_SIZE = 20;

async function previewSegment(workspaceId: string, root: Node, signal: AbortSignal): Promise<PreviewResult> {
  const body = { definition: toDefinition(root), sample_size: SAMPLE_SIZE };
  const answer: unknown = await unwrap(
    client.POST("/api/v1/workspaces/{ws}/segments/preview", {
      params: { path: { ws: workspaceId } },
      body: body as never,
      signal,
    }),
  );
  return answer as PreviewResult;
}

export type PreviewState =
  /** The definition is not filled in, so nothing was asked. */
  | { status: "incomplete" }
  /** A count is being fetched; `previous` is the last one, kept on screen meanwhile. */
  | { status: "loading"; previous: PreviewResult | null }
  | { status: "ready"; result: PreviewResult }
  | { status: "timeout"; previous: PreviewResult | null }
  | { status: "issues"; previous: PreviewResult | null }
  | { status: "error"; error: ErrorSummary; previous: PreviewResult | null };

type Outcome =
  | { status: "ready"; result: PreviewResult }
  | { status: "timeout" }
  | { status: "issues" }
  | { status: "error"; error: ErrorSummary };

interface Settled {
  key: string;
  attempt: number;
  outcome: Outcome;
}

export interface LivePreview {
  state: PreviewState;
  retry: () => void;
}

/**
 * Counts the people a complete definition matches. A pause of PREVIEW_DELAY_MS after the last edit
 * sends one request; a newer edit aborts the one in flight, and an answer from a request that was
 * aborted is dropped, so a slow old answer never replaces a newer one. `root` is null while the
 * definition is incomplete. A refusal with issues is passed to `onIssues`, an accepted one clears them.
 */
export function useLivePreview(
  workspaceId: string,
  root: Node | null,
  onIssues: (issues: readonly DefinitionIssue[]) => void,
): LivePreview {
  const [settled, setSettled] = useState<Settled | null>(null);
  const [last, setLast] = useState<PreviewResult | null>(null);
  const [attempt, setAttempt] = useState(0);
  const latest = useRef({ root, onIssues });
  const started = useRef(false);
  const immediate = useRef(false);
  const key = root === null ? null : JSON.stringify(toDefinition(root));

  useEffect(() => {
    latest.current = { root, onIssues };
  });

  useEffect(() => {
    const sending = latest.current.root;
    if (key === null || sending === null) return;
    const controller = new AbortController();
    const delay = started.current && !immediate.current ? PREVIEW_DELAY_MS : 0;
    immediate.current = false;
    const timer = setTimeout(() => {
      started.current = true;
      previewSegment(workspaceId, sending, controller.signal).then(
        (result) => {
          if (controller.signal.aborted) return;
          latest.current.onIssues([]);
          setLast(result);
          setSettled({ key, attempt, outcome: { status: "ready", result } });
        },
        (error: unknown) => {
          if (controller.signal.aborted) return;
          if (error instanceof ApiError && error.status === 422 && error.issues.length > 0) {
            latest.current.onIssues(error.issues);
            setSettled({ key, attempt, outcome: { status: "issues" } });
          } else if (error instanceof ApiError && error.code === "segment_preview_timeout") {
            setSettled({ key, attempt, outcome: { status: "timeout" } });
          } else {
            setSettled({ key, attempt, outcome: { status: "error", error: describeError(error) } });
          }
        },
      );
    }, delay);
    return () => {
      clearTimeout(timer);
      controller.abort();
    };
  }, [workspaceId, key, attempt]);

  const retry = useCallback(() => {
    immediate.current = true;
    setAttempt((value) => value + 1);
  }, []);

  if (key === null) return { state: { status: "incomplete" }, retry };
  if (settled?.key !== key || settled.attempt !== attempt) {
    return { state: { status: "loading", previous: last }, retry };
  }
  const outcome = settled.outcome;
  switch (outcome.status) {
    case "ready":
      return { state: outcome, retry };
    case "error":
      return { state: { status: "error", error: outcome.error, previous: last }, retry };
    default:
      return { state: { status: outcome.status, previous: last }, retry };
  }
}
