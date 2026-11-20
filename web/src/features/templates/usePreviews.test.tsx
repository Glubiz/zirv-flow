import { renderHook, waitFor } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { json, mockApi } from "../../test/mockApi";
import { previewBody, templateRoutes, TEMPLATE_ID, WS } from "../../test/templatesApi";
import { createLimiter, SAMPLE_SUBJECT } from "./reviewChecks";
import { usePreviews, type PreviewRequest } from "./usePreviews";

const PREVIEW = `POST ${WS}/templates/${TEMPLATE_ID}/preview`;
const WAIT = { timeout: 4000 };
const person = (id: string) => ({ key: id, subject: { key: id, label: id, personId: id } });
const SAMPLE: PreviewRequest = { key: "sample", subject: SAMPLE_SUBJECT };

describe("usePreviews", () => {
  it("never sends a request that was superseded while it waited, and answers the run that replaced it", async () => {
    const asked: string[] = [];
    let release: () => void = () => undefined;
    const gate = new Promise<void>((resolve) => {
      release = resolve;
    });
    mockApi({
      ...templateRoutes(),
      [PREVIEW]: async (request) => {
        const body = (await request.json()) as { person_id?: string };
        asked.push(body.person_id ?? "sample");
        if (asked.length === 1) await gate;
        return json(previewBody());
      },
    });
    const limit = createLimiter(1);
    const first = [SAMPLE, person("p-1"), person("p-2")];
    const second = [person("p-9")];
    const view = renderHook(({ requests }) => usePreviews("ws1", TEMPLATE_ID, requests, limit, 0), { initialProps: { requests: first } });
    await waitFor(() => expect(asked).toEqual(["sample"]), WAIT);

    view.rerender({ requests: second });
    release();

    await waitFor(() => expect(view.result.current["p-9"]?.status).toBe("ready"), WAIT);
    expect(asked).toEqual(["sample", "p-9"]);
  });

  it("aborts the request in flight when the page is left", async () => {
    let signal: AbortSignal | undefined;
    mockApi({
      ...templateRoutes(),
      [PREVIEW]: async (request) => {
        signal = request.signal;
        await new Promise((resolve) => setTimeout(resolve, 300));
        return json(previewBody());
      },
    });
    const view = renderHook(() => usePreviews("ws1", TEMPLATE_ID, [SAMPLE], createLimiter(3), 0));
    await waitFor(() => expect(signal).toBeDefined(), WAIT);
    expect(signal?.aborted).toBe(false);

    view.unmount();

    expect(signal?.aborted).toBe(true);
  });
});
