import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it } from "vitest";

import { FLOW_ID, flowBody, flowDetail, flowRoutes, impactBody, WS } from "../../test/flowsApi";
import { mountEditor as mount, openSheetOf } from "../../test/flowsEditor";
import { json, mockApi, problem, type Handler } from "../../test/mockApi";
import { stubReactFlow } from "../../test/reactFlowStubs";
import { SEGMENT_ID } from "../../test/segmentsApi";

beforeEach(() => {
  stubReactFlow();
});

const PUBLISH = `POST ${WS}/flows/${FLOW_ID}/publish`;

/** A publish endpoint: the dry run answers `impact`, the real publish answers `real`. */
function publishRoute(impact: Handler, real: Handler): Record<string, Handler> {
  return {
    [PUBLISH]: (request) => (new URL(request.url).searchParams.get("dry_run") === "true" ? impact(request) : real(request)),
  };
}

function published(version: number) {
  return json(
    {
      flow: flowBody({ state: "published", current_version: version }),
      version: {
        version,
        graph: { v: 1, nodes: [], edges: [] },
        settings: {},
        published_at: "2026-10-20T09:00:00Z",
        trace_id: "66666666-0000-4000-8000-000000000001",
      },
    },
    201,
  );
}

function refused(issues: { code: string; node?: string }[]) {
  return new Response(
    JSON.stringify({
      type: "about:blank",
      title: "The flow has issues",
      status: 422,
      detail: "The draft cannot be published.",
      code: "flow_invalid",
      flow_issues: issues,
    }),
    { status: 422, headers: { "content-type": "application/problem+json" } },
  );
}

async function openDialog(user: ReturnType<typeof userEvent.setup>) {
  await user.click(await screen.findByRole("button", { name: "Publish…" }));
  return screen.findByRole("dialog");
}

describe("publish dialog", () => {
  it("shows a loading state while the dry run is on its way", async () => {
    mockApi({ ...flowRoutes(), ...publishRoute(() => new Promise<Response>(() => undefined), () => published(1)) });
    const user = userEvent.setup();
    mount();

    const dialog = await openDialog(user);

    expect(await within(dialog).findByText("Checking what publishing would do")).toBeInTheDocument();
    expect(within(dialog).queryByRole("button", { name: /^Publish version/ })).not.toBeInTheDocument();
  });

  it("saves a pending edit before it asks for the dry run", async () => {
    const calls = mockApi({ ...flowRoutes(), ...publishRoute(() => json(impactBody()), () => published(1)) });
    const user = userEvent.setup();
    mount();
    const sheet = await openSheetOf(user, /^Wait: 1 day/);
    await user.selectOptions(within(sheet).getByLabelText("Unit"), "hour");
    await user.click(screen.getByRole("button", { name: "Close" }));

    await openDialog(user);

    await screen.findByRole("button", { name: "Publish version 1" });
    const order = calls.map((call) => `${call.method} ${call.path.replace(`${WS}/flows/${FLOW_ID}`, "")}${call.search}`);
    const save = order.indexOf("PATCH ");
    const dry = order.indexOf("POST /publish?dry_run=true");
    expect(save).toBeGreaterThan(-1);
    expect(dry).toBeGreaterThan(save);
  });

  it("previews a valid publish: the version, who enters and who stays on older versions", async () => {
    const impact = impactBody({
      next_version: 4,
      audience: { segment_id: SEGMENT_ID, members: 12_480, computed_at: new Date(Date.now() - 3_600_000).toISOString() },
      enrollments_staying_on_older_versions: 312,
    });
    mockApi({ ...flowRoutes({ flow: { state: "published", current_version: 3 } }), ...publishRoute(() => json(impact), () => published(4)) });
    const user = userEvent.setup();
    mount();

    const dialog = await openDialog(user);

    expect(await within(dialog).findByRole("heading", { name: "Publish version 4 of Welcome series" })).toBeInTheDocument();
    expect(within(dialog).getByText("No issues stop publishing")).toBeInTheDocument();
    expect(within(dialog).getByText(/12,480 people/)).toBeInTheDocument();
    expect(within(dialog).getByText(/312 people/)).toBeInTheDocument();
    expect(within(dialog).getByRole("button", { name: "Publish version 4" })).toBeEnabled();
  });

  it("lists the issues in words and disables Publish with the reason", async () => {
    const impact = impactBody({
      valid: false,
      issues: [{ code: "port_not_wired", node: "n1", at: "/ports/next" }],
    });
    mockApi({ ...flowRoutes(), ...publishRoute(() => json(impact), () => published(1)) });
    const user = userEvent.setup();
    mount();

    const dialog = await openDialog(user);

    const list = await within(dialog).findByRole("list", { name: "Issues that stop publishing" });
    expect(within(list).getByText("The Next output is not connected to a next step.")).toBeInTheDocument();
    const publish = within(dialog).getByRole("button", { name: "Publish version 1" });
    expect(publish).toBeDisabled();
    expect(publish).toHaveAccessibleDescription("Fix the issue above first.");
  });

  it("says plainly when there is no audience estimate", async () => {
    mockApi({ ...flowRoutes(), ...publishRoute(() => json(impactBody({ audience: null })), () => published(1)) });
    const user = userEvent.setup();
    mount();

    const dialog = await openDialog(user);

    expect(await within(dialog).findByText(/^No estimate\./)).toBeInTheDocument();
  });

  it("shows stale and failing inputs as warnings that do not block publishing", async () => {
    const impact = impactBody({
      unhealthy_inputs: [
        {
          source_id: "77777777-0000-4000-8000-000000000001",
          name: "Billing DB · invoices",
          state: "stale",
          last_success_at: new Date(Date.now() - 3 * 3_600_000).toISOString(),
        },
        { source_id: "77777777-0000-4000-8000-000000000002", name: "CRM", state: "failing", last_success_at: null },
      ],
    });
    mockApi({ ...flowRoutes(), ...publishRoute(() => json(impact), () => published(1)) });
    const user = userEvent.setup();
    mount();

    const dialog = await openDialog(user);

    const warnings = await within(dialog).findByRole("list", { name: "Input warnings" });
    expect(within(warnings).getByText("Billing DB · invoices is stale.")).toBeInTheDocument();
    expect(within(warnings).getByText(/last synced 3 hours ago/)).toBeInTheDocument();
    expect(within(warnings).getByText("CRM is failing.")).toBeInTheDocument();
    expect(within(warnings).getByText(/never synced successfully/)).toBeInTheDocument();
    expect(within(dialog).getByRole("button", { name: "Publish version 1" })).toBeEnabled();
  });

  it("publishes, closes, updates the state pill and announces the result", async () => {
    const calls = mockApi({ ...flowRoutes(), ...publishRoute(() => json(impactBody()), () => published(1)) });
    const user = userEvent.setup();
    mount();
    const dialog = await openDialog(user);

    await user.click(await within(dialog).findByRole("button", { name: "Publish version 1" }));

    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
    expect(screen.getByText("Live")).toBeInTheDocument();
    expect(screen.getByText("v1 live")).toBeInTheDocument();
    expect(screen.getByText("Version 1 of Welcome series is published.")).toBeInTheDocument();
    expect(calls.filter((call) => call.method === "POST" && call.path.endsWith("/publish") && call.search === "")).toHaveLength(1);
  });

  it("keeps the dialog open when the publish is refused and shows the issues on the canvas", async () => {
    mockApi({
      ...flowRoutes(),
      ...publishRoute(
        () => json(impactBody()),
        () => refused([{ code: "port_not_wired", node: "n1" }]),
      ),
    });
    const user = userEvent.setup();
    mount();
    const dialog = await openDialog(user);

    await user.click(await within(dialog).findByRole("button", { name: "Publish version 1" }));

    const list = await within(dialog).findByRole("list", { name: "Issues that stop publishing" });
    expect(within(list).getByText(/not connected to a next step/)).toBeInTheDocument();
    expect(within(dialog).getByRole("button", { name: "Publish version 1" })).toBeDisabled();
    expect(screen.getByRole("group", { name: /^Wait: 1 day.*1 issue/ })).toBeInTheDocument();
  });

  it("shows an error with Retry, and loads again", async () => {
    let asked = 0;
    mockApi({
      ...flowRoutes(),
      ...publishRoute(
        () => {
          asked += 1;
          return asked === 1 ? problem(500, "Internal Server Error", "the dry run failed") : json(impactBody());
        },
        () => published(1),
      ),
    });
    const user = userEvent.setup();
    mount();
    const dialog = await openDialog(user);

    expect(await within(dialog).findByText("the dry run failed")).toBeInTheDocument();
    await user.click(within(dialog).getByRole("button", { name: "Retry" }));

    expect(await within(dialog).findByRole("button", { name: "Publish version 1" })).toBeEnabled();
    expect(asked).toBe(2);
  });

  it("shows the problem of a failed publish with Retry, and publishes on Retry", async () => {
    let tried = 0;
    mockApi({
      ...flowRoutes(),
      ...publishRoute(
        () => json(impactBody()),
        () => {
          tried += 1;
          return tried === 1 ? problem(500, "Internal Server Error", "the publish failed") : published(1);
        },
      ),
    });
    const user = userEvent.setup();
    mount();
    const dialog = await openDialog(user);

    await user.click(await within(dialog).findByRole("button", { name: "Publish version 1" }));
    expect(await within(dialog).findByText("the publish failed")).toBeInTheDocument();
    await user.click(within(dialog).getByRole("button", { name: "Retry" }));

    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
    expect(tried).toBe(2);
  });

  it("closes on an issue and focuses the step it is about", async () => {
    const impact = impactBody({ valid: false, issues: [{ code: "port_not_wired", node: "n1", at: "/ports/next" }] });
    mockApi({ ...flowRoutes(), ...publishRoute(() => json(impact), () => published(1)) });
    const user = userEvent.setup();
    mount();
    const dialog = await openDialog(user);

    await user.click(await within(dialog).findByRole("button", { name: /not connected to a next step/ }));

    await waitFor(() => expect(screen.queryByRole("dialog", { name: /^Publish/ })).not.toBeInTheDocument());
    await waitFor(() => expect(screen.getByRole("group", { name: /^Wait: 1 day/ })).toHaveFocus());
  });

  it("traps focus in the dialog, closes on Escape and returns focus to Publish…", async () => {
    mockApi({ ...flowRoutes(), ...publishRoute(() => json(impactBody()), () => published(1)) });
    const user = userEvent.setup();
    mount();
    const opener = await screen.findByRole("button", { name: "Publish…" });
    await user.click(opener);
    const dialog = await screen.findByRole("dialog");
    await within(dialog).findByRole("button", { name: "Publish version 1" });

    for (let press = 0; press < 6; press += 1) {
      await user.tab();
      expect(dialog).toContainElement(document.activeElement as HTMLElement);
    }
    await user.keyboard("{Escape}");

    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
    expect(opener).toHaveFocus();
  });
});

describe("publishing is for admins", () => {
  it("offers a member who may edit no way to publish", async () => {
    mockApi(flowRoutes({ role: "member", flow: { state: "published", current_version: 2 } }));
    mount();

    await screen.findByRole("group", { name: /^Wait: 1 day/ });
    expect(screen.queryByRole("button", { name: "Publish…" })).not.toBeInTheDocument();
  });
});

describe("publish in flight and refused", () => {
  it("stays open on Escape and Cancel while the publish is on its way, and shows the new version when it lands", async () => {
    let release: (response: Response) => void = () => undefined;
    const pending = new Promise<Response>((resolve) => {
      release = resolve;
    });
    mockApi({ ...flowRoutes(), ...publishRoute(() => json(impactBody()), () => pending) });
    const user = userEvent.setup();
    mount();
    const dialog = await openDialog(user);
    await user.click(await within(dialog).findByRole("button", { name: "Publish version 1" }));
    await within(dialog).findByRole("button", { name: "Publishing…" });

    await user.keyboard("{Escape}");
    await user.click(within(dialog).getByRole("button", { name: "Cancel" }));

    expect(within(dialog).getByRole("button", { name: "Cancel" })).toBeDisabled();
    expect(screen.getByRole("dialog")).toBeInTheDocument();
    release(published(1));
    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
    expect(screen.getByText("Live")).toBeInTheDocument();
    expect(screen.getByText("v1 live")).toBeInTheDocument();
  });

  it("refetches the flow on a 409, closes the dialog, makes the editor read-only and says so", async () => {
    const routes = flowRoutes();
    const load = routes[`GET ${WS}/flows/${FLOW_ID}`]!;
    let loads = 0;
    mockApi({
      ...routes,
      [`GET ${WS}/flows/${FLOW_ID}`]: (request) => {
        loads += 1;
        return loads === 1 ? load(request) : json(flowDetail({ state: "archived" }));
      },
      ...publishRoute(
        () => json(impactBody()),
        () => problem(409, "Conflict", "The flow is archived."),
      ),
    });
    const user = userEvent.setup();
    mount();
    const dialog = await openDialog(user);

    await user.click(await within(dialog).findByRole("button", { name: "Publish version 1" }));

    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
    expect(await screen.findByRole("alert")).toHaveTextContent("changed somewhere else");
    expect(screen.getByText("Archived")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Publish…" })).not.toBeInTheDocument();
    expect(screen.queryByRole("textbox", { name: "Flow name" })).not.toBeInTheDocument();
    expect(loads).toBe(2);
  });

  it("speaks the reason a publish is blocked through a region that was there before the words", async () => {
    const impact = impactBody({ valid: false, issues: [{ code: "port_not_wired", node: "n1", at: "/ports/next" }] });
    let release: () => void = () => undefined;
    const gate = new Promise<void>((resolve) => {
      release = resolve;
    });
    mockApi({
      ...flowRoutes(),
      ...publishRoute(
        async () => {
          await gate;
          return json(impact);
        },
        () => published(1),
      ),
    });
    const user = userEvent.setup();
    mount();
    const dialog = await openDialog(user);

    const region = dialog.querySelector(".pub-announce");
    expect(region).not.toBeNull();
    expect(region).toHaveAttribute("role", "status");
    expect(region).toBeEmptyDOMElement();
    release();
    await within(dialog).findByRole("list", { name: "Issues that stop publishing" });

    await waitFor(() => expect(region).toHaveTextContent("Fix the issue above first."));
    expect(dialog.querySelector(".pub-announce")).toBe(region);
  });
});
