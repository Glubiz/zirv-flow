import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { json, MEMBER, mockApi, problem, type Call } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { DESTINATION_ID, QUOTA, SENDER_ID, templateDetail, templateRoutes, TEMPLATE_ID, WS } from "../../test/templatesApi";
import { formatTime } from "../../ui/format";
import { TestSendPage } from "./TestSendPage";

const WAIT = { timeout: 4000 };
const SEND = `POST ${WS}/templates/${TEMPLATE_ID}/test-send`;
const LOG = `GET ${WS}/test-sends`;
const OTHER_MEMBER = "u-other";

function mount() {
  return renderRoute(`/w/ws1/templates/${TEMPLATE_ID}/test`, "/w/:ws/templates/:id/test", <TestSendPage />);
}

const TEAM = [
  { user_id: MEMBER.id, email: MEMBER.email, role: "member", added_at: "2026-10-01T09:00:00Z" },
  { user_id: OTHER_MEMBER, email: "other@example.test", role: "member", added_at: "2026-10-01T09:00:00Z" },
];

/** The writer's routes with two team members, so a test can pick several recipients. */
function routes(extra: Record<string, (request: Request) => Response | Promise<Response>> = {}, options: Parameters<typeof templateRoutes>[0] = {}) {
  return {
    ...templateRoutes(options),
    "GET /api/v1/workspaces/ws1/members": () => json(options.role === "reader" ? [] : TEAM),
    ...extra,
  };
}

function row(overrides: Record<string, unknown> = {}) {
  return {
    id: "ts-1",
    template_id: TEMPLATE_ID,
    destination_id: DESTINATION_ID,
    sender_identity_id: SENDER_ID,
    created_at: "2026-10-11T09:00:00Z",
    outcome: "queued",
    render_context: "sample",
    template_version: null,
    draft_revision: 1,
    recipient_display: MEMBER.email,
    sent_by_email: "owner@example.test",
    ...overrides,
  };
}

function sent(calls: Call[]) {
  return calls.filter((call) => call.method === "POST" && call.path.endsWith("/test-send")).map((call) => call.body as Record<string, unknown>);
}

async function pick(user: ReturnType<typeof userEvent.setup>, ...names: string[]) {
  for (const name of names) await user.click(await screen.findByRole("checkbox", { name }));
}

describe("the Send a test page", () => {
  it("shows the quota of the caller and of the workspace", async () => {
    mockApi(routes());
    mount();

    expect(await screen.findByText("7 of your 10 tests left this hour · workspace 64 of 100")).toBeInTheDocument();
  });

  it("sends to several recipients one after another and shows each result as it lands", async () => {
    let inFlight = 0;
    let overlapped = false;
    const answers: (() => void)[] = [];
    const calls = mockApi(
      routes({
        [SEND]: () => {
          inFlight += 1;
          overlapped ||= inFlight > 1;
          return new Promise<Response>((resolve) => {
            answers.push(() => {
              inFlight -= 1;
              resolve(json(row(), 202));
            });
          });
        },
      }),
    );
    const user = userEvent.setup();
    mount();

    await screen.findByRole("form", { name: "Test send" });
    await user.selectOptions(screen.getByLabelText("Send through"), DESTINATION_ID);
    await pick(user, MEMBER.email, "other@example.test", "q***@example.com");
    await user.click(screen.getByRole("button", { name: "Send 3 tests" }));

    await waitFor(() => expect(answers).toHaveLength(1), WAIT);
    const results = screen.getByRole("status", { name: "Results" });
    expect(within(results).getByText(MEMBER.email, { exact: false }).closest("li")).toHaveTextContent("Sending");
    expect(within(results).getByText("other@example.test").closest("li")).toHaveTextContent("Waiting");
    answers[0]?.();
    await waitFor(() => expect(answers).toHaveLength(2), WAIT);
    expect(within(results).getByText(MEMBER.email, { exact: false }).closest("li")).toHaveTextContent("Queued");
    answers[1]?.();
    await waitFor(() => expect(answers).toHaveLength(3), WAIT);
    answers[2]?.();

    await waitFor(() => expect(within(results).getAllByText("Queued", { exact: false })).toHaveLength(3), WAIT);
    expect(overlapped).toBe(false);
    expect(sent(calls)).toEqual([
      { destination_id: DESTINATION_ID, sender_identity_id: SENDER_ID, to: { member: MEMBER.id } },
      { destination_id: DESTINATION_ID, sender_identity_id: SENDER_ID, to: { member: OTHER_MEMBER } },
      { destination_id: DESTINATION_ID, sender_identity_id: SENDER_ID, to: { allowlist_entry: "a1" } },
    ]);
    expect(calls.filter((call) => call.path === `${WS}/test-sends/quota`).length).toBeGreaterThan(1);
  });

  it("stops sending when the page is left", async () => {
    const answers: (() => void)[] = [];
    const calls = mockApi(
      routes({
        [SEND]: () => new Promise<Response>((resolve) => answers.push(() => resolve(json(row(), 202)))),
      }),
    );
    const user = userEvent.setup();
    const view = mount();

    await screen.findByRole("form", { name: "Test send" });
    await user.selectOptions(screen.getByLabelText("Send through"), DESTINATION_ID);
    await pick(user, MEMBER.email, "other@example.test", "q***@example.com");
    await user.click(screen.getByRole("button", { name: "Send 3 tests" }));
    await waitFor(() => expect(answers).toHaveLength(1), WAIT);

    view.unmount();
    answers[0]?.();
    await new Promise((resolve) => setTimeout(resolve, 100));

    expect(sent(calls)).toHaveLength(1);
    expect(answers).toHaveLength(1);
  });

  it("stops the rest at a 429 and shows the reset time", async () => {
    let asked = 0;
    const calls = mockApi(
      routes({
        [SEND]: () => {
          asked += 1;
          return asked === 1 ? json(row(), 202) : problem(429, "Too many requests", "Too many test sends.", undefined, "test_send_rate_limited");
        },
      }),
    );
    const user = userEvent.setup();
    mount();

    await screen.findByRole("form", { name: "Test send" });
    await user.selectOptions(screen.getByLabelText("Send through"), DESTINATION_ID);
    await pick(user, MEMBER.email, "other@example.test", "q***@example.com");
    await user.click(screen.getByRole("button", { name: "Send 3 tests" }));

    const results = await screen.findByRole("status", { name: "Results" });
    expect(await within(results).findByText(new RegExp(`A slot frees at ${formatTime(QUOTA.resets_at).replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}`), undefined, WAIT)).toBeInTheDocument();
    expect(within(results).getByText("other@example.test").closest("li")).toHaveTextContent("Not sent");
    expect(within(results).getByText("q***@example.com").closest("li")).toHaveTextContent("Not sent");
    expect(sent(calls)).toHaveLength(2);
  });

  it("goes on after a refused recipient and shows why", async () => {
    let asked = 0;
    const calls = mockApi(
      routes({
        [SEND]: () => {
          asked += 1;
          return asked === 1 ? problem(422, "Unprocessable", "That recipient is not allowed.", undefined, "recipient_not_allowed") : json(row(), 202);
        },
      }),
    );
    const user = userEvent.setup();
    mount();

    await screen.findByRole("form", { name: "Test send" });
    await user.selectOptions(screen.getByLabelText("Send through"), DESTINATION_ID);
    await pick(user, MEMBER.email, "other@example.test");
    await user.click(screen.getByRole("button", { name: "Send 2 tests" }));

    const results = await screen.findByRole("status", { name: "Results" });
    await waitFor(() => expect(within(results).getByText("other@example.test").closest("li")).toHaveTextContent("Queued"), WAIT);
    expect(within(results).getByText(MEMBER.email, { exact: false }).closest("li")).toHaveTextContent("That recipient is not allowed.");
    expect(sent(calls)).toHaveLength(2);
  });

  it("sends the live version as a saved person and stays disabled until a recipient is picked", async () => {
    const calls = mockApi(
      routes(
        { [SEND]: () => json(row({ template_version: 2, render_context: "person" }), 202) },
        {
          saved: [{ person_id: "p-1", display: { value: "Mia Jensen" } }],
          detail: templateDetail({ current_version: 2 }),
        },
      ),
    );
    const user = userEvent.setup();
    mount();

    await screen.findByRole("form", { name: "Test send" });
    expect(screen.getByRole("button", { name: "Pick a recipient" })).toBeDisabled();
    await user.click(screen.getByRole("button", { name: "Version 2 · live" }));
    await user.selectOptions(screen.getByLabelText("Fill fields as"), "p-1");
    await user.selectOptions(screen.getByLabelText("Send through"), DESTINATION_ID);
    await pick(user, MEMBER.email);
    await user.click(screen.getByRole("button", { name: "Send 1 test" }));

    await waitFor(() => expect(sent(calls)).toHaveLength(1), WAIT);
    expect(sent(calls)[0]).toEqual({
      destination_id: DESTINATION_ID,
      sender_identity_id: SENDER_ID,
      to: { member: MEMBER.id },
      version: 2,
      person_id: "p-1",
    });
  });

  it("offers real people to an admin and only the sample person to a writer", async () => {
    mockApi(routes({}, { saved: [{ person_id: "p-1", display: { value: "Mia Jensen" } }] }));
    const admin = mount();
    const select = await screen.findByLabelText("Fill fields as");
    expect(await within(select).findByRole("option", { name: "Mia Jensen" })).toBeInTheDocument();
    admin.unmount();

    const calls = mockApi(routes({}, { role: "member", saved: [{ person_id: "p-1", display: { value: "Mia Jensen" } }] }));
    mount();
    const writerSelect = await screen.findByLabelText("Fill fields as");

    expect(within(writerSelect).getAllByRole("option").map((option) => option.textContent)).toEqual(["Sample person"]);
    expect(screen.getByText("Real people's values: owners and admins only.")).toBeInTheDocument();
    expect(calls.map((call) => call.path)).not.toContain(`${WS}/templates/${TEMPLATE_ID}/preview-people`);
  });

  it("blocks a send that is more than the tests left this hour", async () => {
    mockApi(routes({ [`GET ${WS}/test-sends/quota`]: () => json({ ...QUOTA, member: { used: 9, limit: 10 } }) }));
    const user = userEvent.setup();
    mount();

    await screen.findByRole("form", { name: "Test send" });
    await user.selectOptions(screen.getByLabelText("Send through"), DESTINATION_ID);
    await pick(user, MEMBER.email, "other@example.test");

    expect(screen.getByRole("button", { name: "Send 2 tests" })).toBeDisabled();
    expect(screen.getByText("Only 1 test left this hour.")).toBeInTheDocument();
  });

  it("leaves archived destinations and destinations that cannot send out of the choice", async () => {
    mockApi(routes());
    mount();

    const select = await screen.findByLabelText("Send through");

    expect(within(select).getByRole("option", { name: "Transactional mail" })).toBeInTheDocument();
    expect(within(select).queryByRole("option", { name: "Old mail" })).not.toBeInTheDocument();
    expect(within(select).queryByRole("option", { name: "Order hook" })).not.toBeInTheDocument();
  });

  it("links back to the editor and to the allowlist", async () => {
    mockApi(routes());
    mount();

    expect(await screen.findByRole("link", { name: /Back to editing/ })).toHaveAttribute("href", `/w/ws1/templates/${TEMPLATE_ID}`);
    expect(await screen.findByRole("link", { name: "Manage the allowlist" })).toBeInTheDocument();
  });

  it("explains why a reader or an archived template cannot send", async () => {
    mockApi(routes({}, { role: "reader" }));
    const reader = mount();
    expect(await screen.findByText("Only members who can edit templates can send a test.")).toBeInTheDocument();
    reader.unmount();

    mockApi(routes({}, { detail: templateDetail({ archived_at: "2026-10-05T09:00:00Z" }) }));
    mount();
    expect(await screen.findByText("This template is archived, so it cannot be test-sent.")).toBeInTheDocument();
  });

  it("shows an error with a retry when the recipients cannot load", async () => {
    let attempts = 0;
    mockApi(
      routes({
        [`GET ${WS}/destinations/options`]: () => {
          attempts += 1;
          return attempts === 1 ? problem(500, "Internal Server Error", "internal error") : json([]);
        },
      }),
    );
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Try again" }));

    expect(await screen.findByRole("form", { name: "Test send" })).toBeInTheDocument();
  });
});

describe("the recent tests", () => {
  it("shows each outcome as its word, with the reason of a failure in plain words", async () => {
    mockApi(
      routes({
        [LOG]: () =>
          json({
            test_sends: [
              row({ id: "a", outcome: "queued" }),
              row({ id: "b", outcome: "sent" }),
              row({ id: "c", outcome: "suppressed", reason: "suppressed" }),
              row({ id: "d", outcome: "unconfirmed", reason: "delivery_unconfirmed" }),
              row({ id: "e", outcome: "failed", reason: "destination_unavailable" }),
              row({ id: "f", outcome: "render_failed", reason: "person_unavailable" }),
              row({ id: "g", outcome: "sender_unverified", reason: "sender_unverified" }),
            ],
            next_before: null,
          }),
      }),
    );
    mount();

    const table = await screen.findByRole("table", { name: "Recent tests of this template" });

    expect(within(table).getByText("Queued")).toBeInTheDocument();
    expect(within(table).getByText("Sent")).toBeInTheDocument();
    expect(within(table).getByText("Suppressed")).toBeInTheDocument();
    expect(within(table).getByText("No answer")).toBeInTheDocument();
    expect(within(table).getAllByText("Failed")).toHaveLength(3);
    expect(within(table).getByText("The destination is archived or missing.")).toBeInTheDocument();
    expect(within(table).getByText("The person whose values fill the fields no longer exists.")).toBeInTheDocument();
    expect(within(table).getByText("The sender's domain is not verified.")).toBeInTheDocument();
  });

  it("shows who it went to, who sent it, the content and the person", async () => {
    mockApi(
      routes({
        [LOG]: () =>
          json({
            test_sends: [
              row({ id: "a", recipient_display: "q***@example.com", sent_by_email: null, template_version: 4, render_context: "person" }),
              row({ id: "b", recipient_display: "removed address" }),
            ],
            next_before: null,
          }),
      }),
    );
    mount();

    const table = await screen.findByRole("table", { name: "Recent tests of this template" });

    expect(within(table).getByText("q***@example.com")).toBeInTheDocument();
    expect(within(table).getByText("removed address")).toBeInTheDocument();
    expect(within(table).getByText("by owner@example.test")).toBeInTheDocument();
    expect(within(table).getByText("Version 4")).toBeInTheDocument();
    expect(within(table).getByText("as a saved person")).toBeInTheDocument();
    expect(within(table).getByText("as the sample person")).toBeInTheDocument();
  });

  it("pages older tests with the before cursor", async () => {
    const calls = mockApi(
      routes({
        [LOG]: (request) => {
          const before = new URL(request.url).searchParams.get("before");
          return before === null
            ? json({ test_sends: [row({ id: "new", recipient_display: "newest@example.test" })], next_before: "CURSOR" })
            : json({ test_sends: [row({ id: "old", recipient_display: "oldest@example.test" })], next_before: null });
        },
      }),
    );
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Show older tests" }));

    expect(await screen.findByText("oldest@example.test")).toBeInTheDocument();
    expect(screen.getByText("newest@example.test")).toBeInTheDocument();
    expect(calls.some((call) => call.search.includes("before=CURSOR"))).toBe(true);
    expect(screen.queryByRole("button", { name: "Show older tests" })).not.toBeInTheDocument();
  });

  it("says so when no test was sent yet", async () => {
    mockApi(routes());
    mount();

    expect(await screen.findByText("No tests sent yet")).toBeInTheDocument();
  });

  it("shows an error with a retry when the log cannot load", async () => {
    let attempts = 0;
    mockApi(
      routes({
        [LOG]: () => {
          attempts += 1;
          return attempts === 1 ? problem(500, "Internal Server Error", "internal error") : json({ test_sends: [], next_before: null });
        },
      }),
    );
    const user = userEvent.setup();
    mount();

    const section = (await screen.findByRole("heading", { name: "Recent tests" })).closest("section");
    expect(section).not.toBeNull();
    await user.click(await within(section as HTMLElement).findByRole("button", { name: "Try again" }));

    expect(await screen.findByText("No tests sent yet")).toBeInTheDocument();
  });
});
