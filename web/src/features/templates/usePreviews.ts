import { useEffect, useState } from "react";

import { client, describeError, unwrap } from "../../api";
import type { Check, Limiter, Subject } from "./reviewChecks";

export interface PreviewRequest {
  /** Where the answer is kept. */
  key: string;
  subject: Subject;
  /** A published version; absent for the draft. */
  version?: number;
}

interface Answers {
  run: string;
  checks: Record<string, Check | undefined>;
}

/**
 * Previews each request with the marked HTML, through the page's limiter. Changing the requests or `retry` starts over, and the answers of an earlier run are never returned.
 * Null asks for nothing.
 */
export function usePreviews(workspaceId: string, templateId: string, requests: readonly PreviewRequest[] | null, limit: Limiter, retry: number): Record<string, Check | undefined> {
  const run = `${workspaceId}|${templateId}|${retry}|${requests?.map((request) => `${request.key}@${request.version ?? "draft"}`).join(",") ?? ""}`;
  const [answers, setAnswers] = useState<Answers>({ run: "", checks: {} });

  useEffect(() => {
    if (requests === null) return;
    let alive = true;
    for (const request of requests) {
      const { subject, version } = request;
      const body = { annotate: true, ...(subject.personId === null ? {} : { person_id: subject.personId }), ...(version === undefined ? {} : { version }) };
      void limit(() => unwrap(client.POST("/api/v1/workspaces/{ws}/templates/{id}/preview", { params: { path: { ws: workspaceId, id: templateId } }, body })))
        .then<Check>((preview) => ({ status: "ready", preview }))
        .catch<Check>((error: unknown) => ({ status: "failed", error: describeError(error) }))
        .then((check) => {
          if (alive) setAnswers((current) => ({ run, checks: { ...(current.run === run ? current.checks : {}), [request.key]: check } }));
        });
    }
    return () => {
      alive = false;
    };
  }, [workspaceId, templateId, requests, limit, run]);

  return answers.run === run ? answers.checks : {};
}
