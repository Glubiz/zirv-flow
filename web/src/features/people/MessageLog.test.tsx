import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { json, mockApi, OWNER, problem, session } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { MessageLog } from "./MessageLog";
import { explain, gateRows, resultOf, type Message } from "./messageView";

const PERSON = "aaaaaaaa-0000-4000-8000-000000000001";
const TRACE = "0123456789abcdef0123456789abcdef";
const MESSAGES = `GET /api/v1/workspaces/ws1/people/${PERSON}/messages`;
const TEMPLATES = "GET /api/v1/workspaces/ws1/templates";

function message(id: string, overrides: Record<string, unknown> = {}) {
  return {
    id,
    created_at: "2026-10-22T06:00:00Z",
    completed_at: "2026-10-22T06:00:01Z",
    template: { id: "t1", name: "Product news, October", version: 2, category: "marketing" },
    flow: { id: "f1", name: "Monthly newsletter", version: 3 },
    node: { id: "mail", name: "Send newsletter" },
    destination: { id: "d1", name: "Production relay" },
    status: "sent",
    reason: null,
    deciding_rule: null,
    consent_rule_id: null,
    consent_policy_id: null,
    gate_results: [
      { gate: "enrollment", result: "passed" },
      { gate: "consent", result: "passed" },
      { gate: "delivery", result: "passed" },
    ],
    deferred_until: null,
    deferred_why: null,
    trace_id: TRACE,
    ...overrides,
  };
}

const SKIPPED = message("00000000-0000-4000-8000-000000000001", {
  status: "skipped",
  reason: "no_consent",
  deciding_rule: "Cooldown after a complaint",
  consent_rule_id: "11111111-0000-4000-8000-000000000001",
  consent_policy_id: "22222222-0000-4000-8000-000000000001",
  gate_results: [
    { gate: "enrollment", result: "passed" },
    { gate: "template", result: "passed" },
    { gate: "sender", result: "passed" },
    { gate: "address", result: "passed" },
    { gate: "consent", result: "blocked", reason: "no_consent", rule_id: "11111111-0000-4000-8000-000000000001" },
  ],
});

const DEFERRED = message("00000000-0000-4000-8000-000000000002", {
  status: "pending",
  completed_at: null,
  template: { id: "t2", name: "Trial day 3 nudge", version: 4, category: "marketing" },
  gate_results: [{ gate: "quiet_hours", result: "deferred", run_at: "2026-10-23T06:00:00Z" }],
  deferred_until: "2026-10-23T06:00:00Z",
  deferred_why: "quiet_hours",
});

const FAILED = message("00000000-0000-4000-8000-000000000003", {
  status: "failed",
  reason: "render_error",
  gate_results: [{ gate: "enrollment", result: "passed" }, { gate: "render", result: "blocked", reason: "render_error" }],
});

const SENT = message("00000000-0000-4000-8000-000000000004");

function mount(routes: Parameters<typeof mockApi>[0] = {}) {
  const calls = mockApi({
    "GET /api/v1/session": () => json(session(OWNER, "csrf-messages")),
    [TEMPLATES]: () => json([{ id: "t1", name: "Product news, October" }]),
    ...routes,
  });
  renderRoute(`/w/ws1/people/${PERSON}`, "/w/:ws/people/:id", <MessageLog workspaceId="ws1" personId={PERSON} />);
  return calls;
}

describe("the message log of a person", () => {
  it("says why a skipped message was not sent, naming the rule, and a failed one its reason", async () => {
    mount({ [MESSAGES]: () => json({ messages: [SKIPPED, FAILED, SENT], next_before: null }) });

    const table = await screen.findByRole("table", { name: /Messages to this person/ });
    const skipped = within(table).getByRole("row", { name: /Product news, October.*Skipped/ });
    expect(skipped).toHaveTextContent("Skipped: no_consent, rule Cooldown after a complaint");
    expect(within(table).getAllByRole("row")[2]).toHaveTextContent("Failed: render_error");
    expect(within(table).getAllByRole("row")[3]).toHaveTextContent("Sent");
    expect(within(skipped).getByRole("link", { name: "Monthly newsletter" })).toHaveAttribute("href", "/w/ws1/flows/f1");
    expect(screen.getByText("Newest first. Addresses are never shown here.")).toBeInTheDocument();
  });

  it("shows when a deferred message runs again and why", async () => {
    mount({ [MESSAGES]: () => json({ messages: [DEFERRED], next_before: null }) });

    const row = await screen.findByRole("row", { name: /Trial day 3 nudge/ });
    expect(row).toHaveTextContent(/Deferred until .*\(quiet hours\)/);
    expect(row).toHaveTextContent("Every check runs again then.");
  });

  it("expands a row to the checks in order, stops at the consent check and links the trace", async () => {
    const user = userEvent.setup();
    mount({ [MESSAGES]: () => json({ messages: [SKIPPED], next_before: null }) });

    const toggle = await screen.findByRole("button", { name: "Show the checks for Product news, October" });
    expect(toggle).toHaveAttribute("aria-expanded", "false");
    await user.click(toggle);

    const hide = screen.getByRole("button", { name: "Hide the checks for Product news, October" });
    expect(hide).toHaveAttribute("aria-expanded", "true");
    const checks = screen.getByRole("list", { name: "Send checks, in the order they run" });
    const items = within(checks).getAllByRole("listitem");
    expect(items.map((item) => item.querySelector(".strong")?.textContent)).toEqual([
      "Enrollment active",
      "Template",
      "Sender",
      "Address",
      "Consent",
      "Suppression",
      "Quiet hours",
      "Frequency cap",
      "Render",
      "Rate limit",
      "Deliver",
    ]);
    expect(items[0]).toHaveTextContent("Passed");
    expect(items[4]).toHaveTextContent("Stopped here");
    expect(items[4]).toHaveTextContent("no_consent");
    expect(items[5]).toHaveTextContent("Not reached");
    expect(items[10]).toHaveTextContent("Not reached");
    expect(screen.getByText(/The consent policy rule "Cooldown after a complaint" matched/)).toBeInTheDocument();
    expect(screen.getByRole("link", { name: "Open in Traces" })).toHaveAttribute("href", `/w/ws1/traces/${TRACE}`);
    expect(hide).toHaveAttribute("aria-controls", `message-${SKIPPED.id}-checks`);
    expect(document.getElementById(`message-${SKIPPED.id}-checks`)).not.toBeNull();

    await user.click(hide);

    expect(screen.getByRole("button", { name: "Show the checks for Product news, October" })).toHaveAttribute("aria-expanded", "false");
    expect(screen.queryByRole("list", { name: "Send checks, in the order they run" })).not.toBeInTheDocument();
  });

  it("asks for the filters and pages older messages without repeating one", async () => {
    const user = userEvent.setup();
    const calls = mount({
      [MESSAGES]: (request) => {
        const query = new URL(request.url).searchParams;
        if (query.get("before") === "cursor-1") return json({ messages: [SENT], next_before: null });
        return json({ messages: [SKIPPED], next_before: "cursor-1" });
      },
    });

    await screen.findByRole("row", { name: /Skipped/ });
    await user.click(screen.getByRole("button", { name: "Load older" }));

    await waitFor(() => expect(screen.getAllByRole("row")).toHaveLength(3));
    expect(screen.queryByRole("button", { name: "Load older" })).not.toBeInTheDocument();
    await user.selectOptions(screen.getByLabelText("Result"), "skipped");
    await user.selectOptions(await screen.findByRole("combobox", { name: "Template" }), "t1");

    await waitFor(() => {
      const last = calls.filter((call) => call.path.endsWith("/messages")).at(-1);
      expect(last?.search).toContain("result=skipped");
      expect(last?.search).toContain("template_id=t1");
    });
  });

  it("gives every expand button a details region that exists, hidden, while the row is collapsed", async () => {
    mount({ [MESSAGES]: () => json({ messages: [SKIPPED, SENT], next_before: null }) });

    const toggles = await screen.findAllByRole("button", { name: /Show the checks for/ });
    expect(toggles).toHaveLength(2);
    for (const toggle of toggles) {
      const id = toggle.getAttribute("aria-controls");
      expect(id).toMatch(/^message-.+-checks$/);
      const region = document.getElementById(id ?? "");
      expect(region).not.toBeNull();
      expect(region).not.toBeVisible();
    }
  });

  it("moves focus to the first row that Load older brought in", async () => {
    const user = userEvent.setup();
    mount({
      [MESSAGES]: (request) => {
        if (new URL(request.url).searchParams.get("before") === "cursor-1") return json({ messages: [SENT], next_before: null });
        return json({ messages: [SKIPPED], next_before: "cursor-1" });
      },
    });

    await screen.findByRole("row", { name: /Skipped/ });
    await user.click(screen.getByRole("button", { name: "Load older" }));

    await waitFor(() => expect(screen.getAllByRole("row")).toHaveLength(3));
    const rows = screen.getAllByRole("button", { name: /Show the checks for/ });
    await waitFor(() => expect(rows[1]).toHaveFocus());
  });

  it("keeps focus on Load older when the next page brings nothing new", async () => {
    const user = userEvent.setup();
    mount({
      [MESSAGES]: (request) => {
        if (new URL(request.url).searchParams.get("before") === "cursor-1") return json({ messages: [], next_before: "cursor-2" });
        return json({ messages: [SKIPPED], next_before: "cursor-1" });
      },
    });

    await screen.findByRole("row", { name: /Skipped/ });
    await user.click(screen.getByRole("button", { name: "Load older" }));

    await waitFor(() => expect(screen.getByRole("button", { name: "Load older" })).toHaveFocus());
  });

  it("offers In progress in the Result filter and asks for it", async () => {
    const user = userEvent.setup();
    const calls = mount({ [MESSAGES]: () => json({ messages: [SENT], next_before: null }) });

    await screen.findByRole("row", { name: /Sent/ });
    await user.selectOptions(screen.getByLabelText("Result"), "In progress");

    await waitFor(() => expect(calls.filter((call) => call.path.endsWith("/messages")).at(-1)?.search).toContain("result=in_progress"));
  });

  it("says so when there are no messages", async () => {
    mount({ [MESSAGES]: () => json({ messages: [], next_before: null }) });

    expect(await screen.findByText("No messages to this person yet.")).toBeInTheDocument();
  });

  it("shows a loading state, then an error with a retry", async () => {
    const user = userEvent.setup();
    let attempts = 0;
    mount({
      [MESSAGES]: () => {
        attempts += 1;
        return attempts === 1 ? problem(503, "Search timed out", "Try a narrower filter.") : json({ messages: [SENT], next_before: null });
      },
    });

    expect(await screen.findByText("Loading the messages")).toBeInTheDocument();
    const alert = await screen.findByRole("alert");
    expect(alert).toHaveTextContent("Search timed out");
    await user.click(within(alert).getByRole("button", { name: "Try again" }));

    expect(await screen.findByRole("table", { name: /Messages to this person/ })).toBeInTheDocument();
  });
});

describe("the words of a message", () => {
  it("marks the held gate of a message that waits and gives the cap and rate wait their own words", () => {
    const held = DEFERRED as unknown as Message;

    expect(gateRows(held).find((row) => row.state === "held")?.code).toBe("quiet_hours");
    expect(resultOf(held).kind).toBe("deferred");
    expect(explain(held)).toMatch(/Held by the quiet hours until .* Every check runs again then\./);
    expect(resultOf({ ...held, deferred_until: null }).kind).toBe("waiting");
  });

  it("shows a rate-limit hold on the Rate limit check, with the checks before it passed", async () => {
    const user = userEvent.setup();
    const rateLimited = message("00000000-0000-4000-8000-000000000005", {
      status: "pending",
      completed_at: null,
      gate_results: [],
      deferred_until: "2026-10-23T06:00:00Z",
      deferred_why: "rate_limit",
    });
    mount({ [MESSAGES]: () => json({ messages: [rateLimited], next_before: null }) });

    await user.click(await screen.findByRole("button", { name: /Show the checks for/ }));

    const items = within(screen.getByRole("list", { name: "Send checks, in the order they run" })).getAllByRole("listitem");
    const rate = items.findIndex((item) => item.textContent.includes("Rate limit"));
    expect(items[rate]).toHaveTextContent("Held here");
    expect(items.slice(0, rate).every((item) => item.textContent.includes("Passed"))).toBe(true);
    expect(items.slice(rate + 1).every((item) => item.textContent.includes("Not reached"))).toBe(true);
    expect(gateRows(rateLimited as unknown as Message).find((row) => row.state === "held")?.code).toBe("rate_limit");
  });

  it("explains a recorded-consent skip with the built-in rule, not a policy rule", () => {
    const skipped = { ...SKIPPED, deciding_rule: "recorded_consent" } as unknown as Message;

    expect(explain(skipped)).toContain("The recorded consent of the person decided.");
    expect(explain(skipped)).toContain("The send stopped at the consent check and nothing after it ran.");
  });
});
