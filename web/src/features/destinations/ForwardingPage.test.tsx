import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";

import {
  destination,
  destinationKinds,
  DEST_ID,
  letter,
  LETTER_ID,
  OTHER_DEST_ID,
  OTHER_LETTER_ID,
  queue,
  rule,
  RULE_ID,
  WS,
} from "../../test/destinationsApi";
import { json, mockApi, problem } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { actor, catalogRoutes } from "../../test/segmentsApi";
import { ForwardingPage } from "./ForwardingPage";

function mount() {
  return renderRoute("/w/ws1/forwarding", "/w/:ws/forwarding", <ForwardingPage />);
}

type Route = () => Response | Promise<Response>;

function base(extra: Record<string, Route> = {}) {
  return {
    ...actor("writer"),
    ...catalogRoutes(),
    "GET /api/v1/destination-kinds": () => json(destinationKinds()),
    [`GET ${WS}/destinations`]: () =>
      json([destination(), destination({ id: OTHER_DEST_ID, name: "Mail relay", kind: "smtp", config: {} })]),
    [`GET ${WS}/forwarding/status`]: () => json({ destinations: [queue()] }),
    [`GET ${WS}/forwarding/rules`]: () => json([rule()]),
    [`GET ${WS}/forwarding/dead-letters`]: () => json({ letters: [letter(), letter({ id: OTHER_LETTER_ID, error: "egress_denied", attempts: 3 })], next_before: null }),
    ...extra,
  };
}

afterEach(() => vi.unstubAllGlobals());

describe("forwarding status and rules", () => {
  it("shows each destination's depth, oldest lag and dead letters", async () => {
    mockApi(base());
    mount();

    const table = await screen.findByRole("table", { name: "Forwarding queue of each destination" });
    const row = within(table).getAllByRole("row")[1]!;
    expect(within(row).getByText("Order hook")).toBeInTheDocument();
    expect(within(row).getByText("4")).toBeInTheDocument();
    expect(within(row).getByText("2 min")).toBeInTheDocument();
    expect(within(row).getByText("2 dead")).toBeInTheDocument();
  });

  it("lists the rules with their event, destination and filter count", async () => {
    mockApi(base({ [`GET ${WS}/forwarding/rules`]: () => json([rule({ filter: [{ prop: "amount", op: "gt", value: 10 }] })]) }));
    mount();

    const table = await screen.findByRole("table", { name: "Forwarding rules" });
    const row = within(table).getAllByRole("row")[1]!;
    expect(within(row).getByText("Paid orders")).toBeInTheDocument();
    expect(within(row).getByText("purchase")).toBeInTheDocument();
    expect(within(row).getByText("Order hook")).toBeInTheDocument();
    expect(within(row).getByText("1 filter")).toBeInTheDocument();
    expect(within(row).getByText("Active")).toBeInTheDocument();
  });

  it("creates a rule with a typed filter and a mapping of event fields", async () => {
    const calls = mockApi(base({ [`POST ${WS}/forwarding/rules`]: () => json(rule(), 201) }));
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "New rule" }));
    const dialog = await screen.findByRole("dialog", { name: "New forwarding rule" });
    await user.type(within(dialog).getByLabelText("Name"), "Big orders");
    await user.selectOptions(within(dialog).getByLabelText("Destination"), DEST_ID);
    await user.selectOptions(await within(dialog).findByLabelText("Event"), "purchase");

    await user.click(await within(dialog).findByRole("button", { name: "Add a property filter" }));
    await user.selectOptions(await within(dialog).findByLabelText("Property of filter 1"), "amount");
    await user.selectOptions(within(dialog).getByLabelText("Comparison of filter 1"), "gt");
    await user.type(within(dialog).getByLabelText("Value of filter 1"), "10");

    await user.selectOptions(within(dialog).getByLabelText("Source of row 1"), "event.properties");
    await user.type(within(dialog).getByLabelText("Property pointer of row 1"), "/amount");
    await user.click(within(dialog).getByRole("button", { name: "Create the rule" }));

    await waitFor(() => expect(calls.some((call) => call.method === "POST")).toBe(true));
    expect(calls.find((call) => call.method === "POST")?.body).toEqual({
      name: "Big orders",
      destination_id: DEST_ID,
      event_name: "purchase",
      active: true,
      filter: [{ prop: "amount", op: "gt", value: 10 }],
      mapping: { id: { from: "event.properties", pointer: "/amount" } },
    });
  });

  it("shows the destination of a rule being changed read-only and leaves it out of the change", async () => {
    const calls = mockApi(base({ [`PATCH ${WS}/forwarding/rules/${RULE_ID}`]: () => json(rule()) }));
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Change Paid orders" }));
    const dialog = await screen.findByRole("dialog", { name: "Change Paid orders" });
    expect(within(dialog).queryByLabelText("Destination")).not.toBeInTheDocument();
    expect(within(dialog).getByText("Order hook")).toBeInTheDocument();
    await user.clear(within(dialog).getByLabelText("Name"));
    await user.type(within(dialog).getByLabelText("Name"), "Big orders");
    await user.click(within(dialog).getByRole("button", { name: "Save the rule" }));

    await waitFor(() => expect(calls.some((call) => call.method === "PATCH")).toBe(true));
    const body = calls.find((call) => call.method === "PATCH")?.body as Record<string, unknown>;
    expect(body.name).toBe("Big orders");
    expect(body).not.toHaveProperty("destination_id");
  });

  it("offers only destinations that carry events, and points to a webhook when there is none", async () => {
    mockApi(
      base({
        [`GET ${WS}/destinations`]: () => json([destination({ id: OTHER_DEST_ID, name: "Mail relay", kind: "smtp", config: {} })]),
        [`GET ${WS}/forwarding/rules`]: () => json([]),
        [`GET ${WS}/forwarding/status`]: () => json({ destinations: [] }),
      }),
    );
    mount();

    expect(await screen.findByText("No forwarding rules yet")).toBeInTheDocument();
    expect(screen.getByRole("link", { name: "Add one" })).toHaveAttribute("href", "/w/ws1/destinations/new");
    expect(screen.getByRole("button", { name: "New rule" })).toBeDisabled();
    expect(screen.getByText("Nothing is queued")).toBeInTheDocument();
  });
});

describe("dead letters", () => {
  it("lists them with the error code and attempts", async () => {
    mockApi(base());
    mount();

    const table = await screen.findByRole("table", { name: "Dead letters, newest first" });
    const rows = within(table).getAllByRole("row");
    expect(within(rows[1]!).getByText("http_4xx")).toBeInTheDocument();
    expect(within(rows[1]!).getByText("8")).toBeInTheDocument();
    expect(within(rows[2]!).getByText("egress_denied")).toBeInTheDocument();
  });

  it("re-drives one row only after the confirmation", async () => {
    const calls = mockApi(base({ [`POST ${WS}/forwarding/re-drive`]: () => json({ redriven: 1 }) }));
    const user = userEvent.setup();
    mount();

    const table = await screen.findByRole("table", { name: "Dead letters, newest first" });
    await user.click(within(table).getAllByRole("button", { name: /Re-drive the dead letter/ })[0]!);

    const dialog = await screen.findByRole("dialog", { name: "Re-drive dead letters?" });
    expect(calls.some((call) => call.method === "POST" && call.path.endsWith("/re-drive"))).toBe(false);
    await user.click(within(dialog).getByRole("button", { name: "Re-drive now" }));

    await waitFor(() => expect(calls.some((call) => call.path.endsWith("/re-drive"))).toBe(true));
    expect(calls.find((call) => call.path.endsWith("/re-drive"))?.body).toEqual({ id: LETTER_ID });
    expect(await screen.findByText("1 row put back in the queue.")).toBeInTheDocument();
  });

  it("does nothing when the confirmation is cancelled", async () => {
    const calls = mockApi(base({ [`POST ${WS}/forwarding/re-drive`]: () => json({ redriven: 2 }) }));
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Re-drive all dead letters" }));
    const dialog = await screen.findByRole("dialog", { name: "Re-drive dead letters?" });
    expect(within(dialog).getByText(/every dead letter of the workspace/)).toBeInTheDocument();
    await user.click(within(dialog).getByRole("button", { name: "Cancel" }));

    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(calls.some((call) => call.path.endsWith("/re-drive"))).toBe(false);
  });

  it("re-drives all of the workspace with an empty body, and one destination by its id", async () => {
    const calls = mockApi(base({ [`POST ${WS}/forwarding/re-drive`]: () => json({ redriven: 2 }) }));
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Re-drive all dead letters" }));
    await user.click(await screen.findByRole("button", { name: "Re-drive now" }));
    expect(await screen.findByText("2 rows put back in the queue.")).toBeInTheDocument();

    await user.selectOptions(screen.getByLabelText("Destination", { selector: "select" }), DEST_ID);
    await user.click(await screen.findByRole("button", { name: "Re-drive this destination" }));
    await user.click(await screen.findByRole("button", { name: "Re-drive now" }));

    await waitFor(() => expect(calls.filter((call) => call.path.endsWith("/re-drive"))).toHaveLength(2));
    const bodies = calls.filter((call) => call.path.endsWith("/re-drive")).map((call) => call.body);
    expect(bodies[0]).toEqual({});
    expect(bodies[1]).toEqual({ destination_id: DEST_ID });
  });

  it("shows an empty state when nothing is dead", async () => {
    mockApi(base({ [`GET ${WS}/forwarding/dead-letters`]: () => json({ letters: [], next_before: null }) }));
    mount();

    expect(await screen.findByText("No dead letters")).toBeInTheDocument();
  });

  it("shows a loading state while the dead letters load", async () => {
    mockApi(base({ [`GET ${WS}/forwarding/dead-letters`]: () => new Promise<Response>(() => undefined) }));
    mount();

    expect(await screen.findByText("Loading dead letters")).toBeInTheDocument();
  });

  it("shows an error with a retry for the dead letters", async () => {
    let attempts = 0;
    mockApi(
      base({
        [`GET ${WS}/forwarding/dead-letters`]: () => {
          attempts += 1;
          return attempts === 1 ? problem(500, "Internal Server Error", "internal error") : json({ letters: [letter()], next_before: null });
        },
      }),
    );
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Try again" }));
    expect(await screen.findByText("http_4xx")).toBeInTheDocument();
  });

  it("loads the next page of dead letters", async () => {
    const calls = mockApi(
      base({
        [`GET ${WS}/forwarding/dead-letters`]: (() => {
          let page = 0;
          return () => {
            page += 1;
            return page === 1
              ? json({ letters: [letter()], next_before: "cursor-1" })
              : json({ letters: [letter({ id: OTHER_LETTER_ID, error: "egress_denied" })], next_before: null });
          };
        })(),
      }),
    );
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Show more" }));

    expect(await screen.findByText("egress_denied")).toBeInTheDocument();
    expect(screen.getByText("http_4xx")).toBeInTheDocument();
    expect(calls.filter((call) => call.path.endsWith("/dead-letters")).at(-1)?.search).toContain("before=cursor-1");
    expect(screen.queryByRole("button", { name: "Show more" })).not.toBeInTheDocument();
  });
});

describe("forwarding access", () => {
  it("sends someone who is not an admin away from the page", async () => {
    mockApi({ ...base(), ...actor("reader") });
    const { router } = mount();

    await waitFor(() => expect(router.state.location.pathname).toBe("/w/ws1/members"));
    expect(screen.queryByRole("table")).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Re-drive all dead letters" })).not.toBeInTheDocument();
  });
});
