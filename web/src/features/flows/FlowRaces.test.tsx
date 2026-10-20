import { act, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it } from "vitest";

import { FLOW_ID, flowDetail, flowRoutes, impactBody, WS } from "../../test/flowsApi";
import { mountEditor as mount, type User } from "../../test/flowsEditor";
import { json, mockApi, problem, type Handler } from "../../test/mockApi";
import { stubReactFlow } from "../../test/reactFlowStubs";

beforeEach(() => {
  stubReactFlow();
});

const PUBLISH = `POST ${WS}/flows/${FLOW_ID}/publish`;
const FLOW = `GET ${WS}/flows/${FLOW_ID}`;

function deferred() {
  let release: (response: Response) => void = () => undefined;
  const pending = new Promise<Response>((resolve) => {
    release = resolve;
  });
  return { pending, release: (response: Response) => release(response) };
}

function publishRoute(real: Handler): Record<string, Handler> {
  return {
    [PUBLISH]: (request) => (new URL(request.url).searchParams.get("dry_run") === "true" ? json(impactBody()) : real(request)),
  };
}

/** Starts a publish that hangs, and closes its dialog. */
async function hangingPublish(user: User) {
  await user.click(await screen.findByRole("button", { name: "Publish…" }));
  const dialog = await screen.findByRole("dialog");
  await user.click(await within(dialog).findByRole("button", { name: "Publish version 1" }));
  await within(dialog).findByRole("button", { name: "Publishing…" });
  await user.keyboard("{Escape}");
  await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
}

describe("lifecycle actions and a publish in flight", () => {
  it("disables pause and archive while a publish runs with its dialog closed, and publish while one of them runs", async () => {
    const publish = deferred();
    const calls = mockApi({
      ...flowRoutes({ flow: { state: "published", current_version: 2 } }),
      ...publishRoute(() => publish.pending),
      [`POST ${WS}/flows/${FLOW_ID}/pause`]: () => json(flowDetail({ state: "paused", current_version: 2 })),
    });
    const user = userEvent.setup();
    mount();
    await hangingPublish(user);

    const pause = screen.getByRole("button", { name: "Pause" });
    const archive = screen.getByRole("button", { name: "Archive…" });
    expect(pause).toHaveAttribute("aria-disabled", "true");
    expect(archive).toHaveAttribute("aria-disabled", "true");
    await user.click(pause);
    await user.click(archive);
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(calls.filter((call) => call.path.endsWith("/pause"))).toHaveLength(0);

    publish.release(problem(500, "Boom", "The publish failed."));
    await waitFor(() => expect(screen.getByRole("button", { name: "Pause" })).not.toHaveAttribute("aria-disabled"));
  });

  it("does not start a publish while a lifecycle action runs", async () => {
    const pause = deferred();
    const calls = mockApi({
      ...flowRoutes({ flow: { state: "published", current_version: 2 } }),
      [`POST ${WS}/flows/${FLOW_ID}/pause`]: () => pause.pending,
    });
    const user = userEvent.setup();
    mount();
    await user.click(await screen.findByRole("button", { name: "Pause" }));

    const publish = screen.getByRole("button", { name: "Publish…" });
    expect(publish).toBeDisabled();
    await user.click(publish);
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(calls.filter((call) => call.method === "POST" && call.path.endsWith("/publish"))).toHaveLength(0);

    pause.release(json(flowDetail({ state: "paused", current_version: 2 })));
    expect(await screen.findByText("Paused")).toBeInTheDocument();
  });

  it("does not let a slow refetch that arrives last overwrite a newer applied state", async () => {
    const slow = deferred();
    const routes = flowRoutes({ flow: { state: "published", current_version: 2 } });
    const load = routes[FLOW]!;
    let loads = 0;
    mockApi({
      ...routes,
      [FLOW]: (request) => {
        loads += 1;
        return loads === 1 ? load(request) : slow.pending;
      },
      ...publishRoute(() => problem(409, "Conflict", "The flow changed.")),
      [`POST ${WS}/flows/${FLOW_ID}/pause`]: () => json(flowDetail({ state: "paused", current_version: 2 })),
    });
    const user = userEvent.setup();
    mount();
    await user.click(await screen.findByRole("button", { name: "Publish…" }));
    const dialog = await screen.findByRole("dialog");
    await user.click(await within(dialog).findByRole("button", { name: "Publish version 1" }));
    await waitFor(() => expect(loads).toBe(2));
    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());

    await user.click(await screen.findByRole("button", { name: "Pause" }));
    expect(await screen.findByText("Paused")).toBeInTheDocument();
    slow.release(json(flowDetail({ state: "published", current_version: 2 })));
    await act(async () => {
      await new Promise((resolve) => setTimeout(resolve, 30));
    });

    expect(screen.getByText("Paused")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Resume" })).toBeInTheDocument();
    expect(screen.queryByText(/changed somewhere else/)).not.toBeInTheDocument();
  });
});
