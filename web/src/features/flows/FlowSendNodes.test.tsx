import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it } from "vitest";

import {
  DESTINATION_ID,
  deliveryRoutes,
  destinationOptions,
  FLOW_ID,
  flowRoutes,
  HOOK_TEMPLATE_ID,
  sendGraph,
  TEMPLATE_ID,
  TOPIC_ID,
  versionBody,
  WEBHOOK_DESTINATION_ID,
  WS,
} from "../../test/flowsApi";
import { lastGraph, mountEditor as mount, openSheetOf } from "../../test/flowsEditor";
import { json, mockApi, problem, type Handler } from "../../test/mockApi";
import { stubReactFlow } from "../../test/reactFlowStubs";

beforeEach(() => {
  stubReactFlow();
});

const SAVE = { timeout: 3000 };
const FLOW = `${WS}/flows/${FLOW_ID}`;

function routes(extra: Record<string, Handler> = {}, role: "writer" | "member" | "reader" = "writer"): Record<string, Handler> {
  return { ...flowRoutes({ role, flow: { graph: sendGraph() } }), ...deliveryRoutes(), ...extra };
}

function node(calls: Parameters<typeof lastGraph>[0], id: string) {
  return lastGraph(calls)?.nodes.find((entry) => entry.id === id);
}

describe("the send and sync steps in the editor", () => {
  it("offers both kinds in the add-a-step menu", async () => {
    mockApi(routes());
    const user = userEvent.setup();
    mount();
    await screen.findByRole("group", { name: /^Wait: 1 day/ });

    await user.click(screen.getByRole("button", { name: "Add a step" }));

    expect(screen.getByRole("menuitem", { name: "Send an email" })).toBeInTheDocument();
    expect(screen.getByRole("menuitem", { name: "Sync a profile" })).toBeInTheDocument();
  });

  it("adds a send step that starts with nothing chosen", async () => {
    const calls = mockApi(routes());
    const user = userEvent.setup();
    mount();
    await screen.findByRole("group", { name: /^Wait: 1 day/ });

    await user.click(screen.getByRole("button", { name: "Add a step" }));
    await user.click(screen.getByRole("menuitem", { name: "Send an email" }));

    await waitFor(() => expect(node(calls, "n6")).toMatchObject({ type: "action.send" }), SAVE);
    expect(await screen.findByRole("group", { name: /^Send an email: Choose a template/ })).toBeInTheDocument();
  });

  it("edits a send step: template, destination, topic and dimension values are saved", async () => {
    const calls = mockApi(routes());
    const user = userEvent.setup();
    mount();

    const sheet = await openSheetOf(user, /^Send an email: Send an email/);
    expect(await within(sheet).findByLabelText("Template")).toHaveValue(TEMPLATE_ID);
    expect(within(sheet).queryByRole("option", { name: "Unpublished" })).not.toBeInTheDocument();
    expect(await within(sheet).findByLabelText("Destination")).toHaveValue(DESTINATION_ID);
    expect(within(sheet).getByLabelText("Topic")).toHaveValue(TOPIC_ID);
    const vertical = await within(sheet).findByLabelText("Vertical");
    expect(vertical).toHaveValue("retail");

    await user.selectOptions(vertical, "b2b");

    await waitFor(() => expect(node(calls, "n2")?.["dimensions"]).toEqual({ vertical: "b2b" }), SAVE);
    expect(node(calls, "n2")).toMatchObject({ template: TEMPLATE_ID, destination: DESTINATION_ID, topic: TOPIC_ID });
  });

  it("clears the topic and the dimensions out of the saved step", async () => {
    const calls = mockApi(routes());
    const user = userEvent.setup();
    mount();

    const sheet = await openSheetOf(user, /^Send an email: Send an email/);
    await user.selectOptions(await within(sheet).findByLabelText("Topic"), "");
    await user.selectOptions(await within(sheet).findByLabelText("Vertical"), "");

    await waitFor(() => {
      const saved = node(calls, "n2");
      expect(saved).toBeDefined();
      expect(saved).not.toHaveProperty("topic");
      expect(saved).not.toHaveProperty("dimensions");
    }, SAVE);
  });

  it("edits a sync step: removing the profile sends no traits", async () => {
    const calls = mockApi(routes());
    const user = userEvent.setup();
    mount();

    const sheet = await openSheetOf(user, /^Sync a profile: Update the profile/);
    expect(within(sheet).getByText("city")).toBeInTheDocument();
    await user.selectOptions(within(sheet).getByLabelText("Operation"), "remove");

    await waitFor(() => expect(node(calls, "n3")).toMatchObject({ operation: "remove", trait_keys: [] }), SAVE);
    expect(await screen.findByRole("group", { name: /^Sync a profile: Remove the profile/ })).toBeInTheDocument();
  });

  it("adds a trait to a sync step", async () => {
    const calls = mockApi(routes());
    const user = userEvent.setup();
    mount();

    const sheet = await openSheetOf(user, /^Sync a profile: /);
    await user.selectOptions(await within(sheet).findByLabelText("Add a trait"), "age");

    await waitFor(() => expect(node(calls, "n3")?.["trait_keys"]).toEqual(["city", "age"]), SAVE);
  });

  it("sets and clears the optional template of a webhook step", async () => {
    const calls = mockApi(routes());
    const user = userEvent.setup();
    mount();

    const sheet = await openSheetOf(user, /^Webhook: /);
    const select = await within(sheet).findByLabelText("Payload template");
    expect(select).toHaveValue(HOOK_TEMPLATE_ID);
    expect(within(select).queryByRole("option", { name: "Trial nudge" })).not.toBeInTheDocument();

    await user.selectOptions(select, "");

    await waitFor(() => {
      const saved = node(calls, "n4");
      expect(saved).toBeDefined();
      expect(saved).not.toHaveProperty("template");
    }, SAVE);
  });

  it("shows the chosen destination's id to a reader who cannot read the options", async () => {
    mockApi(routes({ [`GET ${WS}/destinations/options`]: () => problem(403, "Forbidden", "Needs the write role.") }, "member"));
    const user = userEvent.setup();
    mount();

    const sheet = await openSheetOf(user, /^Send an email: Send an email/);

    expect(await within(sheet).findByText(DESTINATION_ID)).toBeInTheDocument();
    expect(within(sheet).getByText("Needs the write role.")).toBeInTheDocument();
  });

  it("shows an error with a retry when the destinations cannot load", async () => {
    let attempts = 0;
    mockApi(
      routes({
        [`GET ${WS}/destinations/options`]: () => {
          attempts += 1;
          return attempts === 1 ? problem(500, "Internal Server Error", "internal error") : json(destinationOptions());
        },
      }),
    );
    const user = userEvent.setup();
    mount();

    const sheet = await openSheetOf(user, /^Send an email: Send an email/);
    const retries = await within(sheet).findAllByRole("button", { name: "Try again" });
    await user.click(retries[0] as HTMLElement);

    expect(await within(sheet).findByLabelText("Destination")).toHaveValue(DESTINATION_ID);
  });

  it("says Loading destinations while the options are on their way", async () => {
    mockApi(routes({ [`GET ${WS}/destinations/options`]: () => new Promise<Response>(() => undefined) }));
    const user = userEvent.setup();
    mount();

    const sheet = await openSheetOf(user, /^Send an email: Send an email/);

    expect(await within(sheet).findByRole("option", { name: "Loading destinations" })).toBeInTheDocument();
  });

  it("lists only the destinations that can send email in a send step, by name", async () => {
    mockApi(routes());
    const user = userEvent.setup();
    mount();

    const sheet = await openSheetOf(user, /^Send an email: Send an email/);
    const select = await within(sheet).findByLabelText("Destination");

    expect(within(select).getByRole("option", { name: "Transactional mail" })).toBeInTheDocument();
    expect(within(select).queryByRole("option", { name: "Order hook" })).not.toBeInTheDocument();
    expect(within(select).queryByRole("option", { name: "Profile store" })).not.toBeInTheDocument();
    expect(within(select).queryByRole("option", { name: "Old mail" })).not.toBeInTheDocument();
  });

  it("lists only the destinations that can sync profiles in a sync step", async () => {
    mockApi(routes());
    const user = userEvent.setup();
    mount();

    const sheet = await openSheetOf(user, /^Sync a profile: /);
    const select = await within(sheet).findByLabelText("Destination");

    expect(within(select).getByRole("option", { name: "Transactional mail" })).toBeInTheDocument();
    expect(within(select).getByRole("option", { name: "Profile store" })).toBeInTheDocument();
    expect(within(select).queryByRole("option", { name: "Order hook" })).not.toBeInTheDocument();
    expect(within(select).queryByRole("option", { name: "Old mail" })).not.toBeInTheDocument();
  });

  it("keeps the name of a chosen destination that can no longer send, with a warning", async () => {
    mockApi(
      routes({
        [`GET ${WS}/destinations/options`]: () =>
          json([{ id: DESTINATION_ID, name: "Transactional mail", archived: false, capabilities: { send: false, sync: true, erase: false } }]),
      }),
    );
    const user = userEvent.setup();
    mount();

    const sheet = await openSheetOf(user, /^Send an email: Send an email/);

    expect(await within(sheet).findByRole("option", { name: "Transactional mail" })).toBeInTheDocument();
    expect(within(sheet).getByText(/Transactional mail cannot send email/)).toBeInTheDocument();
  });

  it("says why when no destination can send email and shows a chosen unknown id", async () => {
    mockApi(
      routes({
        [`GET ${WS}/destinations/options`]: () =>
          json([{ id: WEBHOOK_DESTINATION_ID, name: "Order hook", archived: false, capabilities: { send: false, sync: false, erase: false } }]),
      }),
    );
    const user = userEvent.setup();
    mount();

    const sheet = await openSheetOf(user, /^Send an email: Send an email/);

    expect(await within(sheet).findByText(/No destination can send email/)).toBeInTheDocument();
    expect(within(sheet).getByRole("option", { name: DESTINATION_ID })).toBeInTheDocument();
  });

  it("shows an error with a retry when the templates cannot load", async () => {
    let attempts = 0;
    mockApi(
      routes({
        [`GET ${WS}/templates`]: () => {
          attempts += 1;
          return attempts === 1 ? problem(500, "Internal Server Error", "internal error") : json([]);
        },
      }),
    );
    const user = userEvent.setup();
    mount();

    const sheet = await openSheetOf(user, /^Send an email: Send an email/);
    const retries = await within(sheet).findAllByRole("button", { name: "Try again" });
    await user.click(retries[0] as HTMLElement);

    expect(await within(sheet).findByText(/No template is published yet/)).toBeInTheDocument();
  });
});

describe("delivery counts on the canvas", () => {
  function countsRoutes(): Record<string, Handler> {
    const node = (node_id: string, node_type: string, entered: number, delivery: Record<string, unknown> = {}) => ({
      node_id,
      node_type,
      entered,
      completed: entered,
      waiting: 0,
      failed: 0,
      goal_conversions: 0,
      sent: 0,
      send_failed: 0,
      skipped: [],
      ...delivery,
    });
    return {
      ...flowRoutes({ flow: { state: "published", current_version: 2, graph: sendGraph() } }),
      ...deliveryRoutes(),
      [`GET ${FLOW}/versions`]: () => json([versionBody(2, { graph: sendGraph() })]),
      [`GET ${FLOW}/versions/2/counts`]: () =>
        json({
          version: 2,
          nodes: [
            node("n1", "wait.duration", 30),
            node("n2", "action.send", 30, {
              sent: 20,
              send_failed: 3,
              skipped: [
                { reason: "no_consent", count: 5 },
                { reason: "suppressed", count: 2 },
              ],
            }),
            node("n3", "action.sync", 20),
            node("n4", "action.webhook", 20),
            node("n5", "exit", 20),
          ],
        }),
      [`GET ${FLOW}/enrollments`]: () => json({ enrollments: [], next_before: null }),
    };
  }

  async function openCounts(user: ReturnType<typeof userEvent.setup>) {
    await screen.findByRole("group", { name: /^Wait: 1 day/ });
    await user.click(screen.getByRole("tab", { name: "Enrollments" }));
    await screen.findByRole("dialog", { name: "Enrollments" });
  }

  it("shows sent, skipped by reason, and failed sends on the send step", async () => {
    mockApi(countsRoutes());
    const user = userEvent.setup();
    mount();
    await openCounts(user);

    const send = await screen.findByRole("group", { name: /^Send an email.*30 entered/ });
    expect(within(send).getByText("20 sent")).toBeInTheDocument();
    expect(within(send).getByText("5 skipped: There is no consent to send this message")).toBeInTheDocument();
    expect(within(send).getByText("2 skipped: The address is suppressed")).toBeInTheDocument();
    expect(within(send).getByText("3 send failed")).toHaveClass("fl-count-bad");
    expect(send).toHaveAccessibleName(/20 sent, 5 skipped, There is no consent to send this message/);
  });

  it("makes the card tall enough for its badge grid, so the badges sit below the text", async () => {
    mockApi(countsRoutes());
    const user = userEvent.setup();
    mount();
    await openCounts(user);

    // A wait card is 92 px; "30 entered" is one grid line: 22 px plus the 4 px gap above it.
    const wait = await screen.findByRole("group", { name: /^Wait: 1 day.*30 entered/ });
    expect((wait.closest(".react-flow__node") as HTMLElement).style.height).toBe("118px");
    const row = wait.querySelector(".fl-counts") as HTMLElement;
    expect(row).toHaveStyle({ "--count-row": "22px", "--count-gap": "4px" });

    // The send card is 92 px: sent, send failed and entered make two lines, two skips two more, each 22 px with 4 px gaps.
    const send = await screen.findByRole("group", { name: /^Send an email.*30 entered/ });
    expect((send.closest(".react-flow__node") as HTMLElement).style.height).toBe(`${92 + 4 * 22 + 3 * 4 + 4}px`);
    const long = within(send).getByText("5 skipped: There is no consent to send this message");
    expect(long).toHaveClass("fl-count-wide");
    expect(long).toHaveAttribute("title", "5 skipped: There is no consent to send this message");
    expect(within(send).getByText("20 sent")).not.toHaveClass("fl-count-wide");
  });

  it("shows no delivery counts on a step that sends nothing", async () => {
    mockApi(countsRoutes());
    const user = userEvent.setup();
    mount();
    await openCounts(user);

    const wait = await screen.findByRole("group", { name: /^Wait: 1 day.*30 entered/ });
    expect(within(wait).queryByText(/sent|skipped/)).not.toBeInTheDocument();
  });

  it("offers a retry when the counts cannot be read", async () => {
    let attempts = 0;
    const base = countsRoutes();
    mockApi({
      ...base,
      [`GET ${FLOW}/versions/2/counts`]: (request) => {
        attempts += 1;
        return attempts === 1 ? problem(500, "Internal Server Error", "internal error") : (base[`GET ${FLOW}/versions/2/counts`] as Handler)(request);
      },
    });
    const user = userEvent.setup();
    mount();
    await openCounts(user);

    const card = await screen.findByRole("group", { name: "Counts and versions" });
    await user.click(await within(card).findByRole("button", { name: "Try again" }));

    expect(await screen.findByRole("group", { name: /^Send an email.*30 entered/ })).toBeInTheDocument();
  });

  it("lists the delivery counts in words in the table", async () => {
    mockApi(countsRoutes());
    const user = userEvent.setup();
    mount();
    await openCounts(user);

    const card = await screen.findByRole("group", { name: "Counts and versions" });
    await user.click(within(card).getByText("Counts as a list"));

    const table = within(card).getByRole("table");
    expect(within(table).getByRole("columnheader", { name: "Delivery" })).toBeInTheDocument();
    expect(within(table).getByText(/20 sent; 5 skipped: There is no consent to send this message; 2 skipped/)).toBeInTheDocument();
  });
});
