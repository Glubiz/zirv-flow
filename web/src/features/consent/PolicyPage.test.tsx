import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { json, MEMBER, mockApi, OWNER, problem, session, type Call, type Handler } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { catalogRoutes, trait, TRAITS, WS } from "../../test/segmentsApi";
import launchPolicy from "./__fixtures__/launch-policy.json";
import { PolicyPage } from "./PolicyPage";

const ROUTE = "/w/:ws/consent/policy";
const PERSON = "33333333-0000-4000-8000-000000000001";
const COOLDOWN_ID = "77777777-0000-4000-8000-000000000003";

const BUILTIN = { id: "recorded_consent", name: "Recorded consent", builtin: true, condition: null };

const TOPIC_DIMENSION = {
  key: "topic",
  name: "Topic",
  builtin: true,
  archived: false,
  values: [{ key: "offers", name: "Offers", archived: false }],
};
const VERTICAL = {
  key: "vertical",
  name: "Vertical",
  builtin: false,
  archived: false,
  values: [
    { key: "retail", name: "Retail", archived: false },
    { key: "sports", name: "Sports", archived: false },
  ],
};

function draftBody(rules: unknown[] = [BUILTIN], revision = 0, published: number | null = null) {
  return { rules, revision, published_version: published, updated_at: revision === 0 ? null : "2026-10-20T09:00:00Z" };
}

function members(role: "admin" | "member" | null) {
  return role === null ? [] : [{ user_id: MEMBER.id, email: MEMBER.email, role, added_at: "2026-10-01T09:00:00Z" }];
}

type Role = "admin" | "member" | "reader";

function routes(role: Role, extra: Record<string, Handler> = {}): Record<string, Handler> {
  const who = role === "admin" ? OWNER : MEMBER;
  return {
    "GET /api/v1/session": () => json(session(who)),
    "GET /api/v1/workspaces/ws1/members": () => json(members(role === "admin" ? "admin" : role === "member" ? "member" : null)),
    ...catalogRoutes(),
    [`GET ${WS}/traits`]: () => json([...TRAITS, trait("account_state", "string")]),
    [`GET ${WS}/consent/dimensions`]: () => json([TOPIC_DIMENSION, VERTICAL]),
    [`GET ${WS}/consent/policy/draft`]: () => json(draftBody()),
    [`GET ${WS}/consent/policy/versions`]: () => json([]),
    ...extra,
  };
}

function mount() {
  return renderRoute("/w/ws1/consent/policy", ROUTE, <PolicyPage />);
}

type User = ReturnType<typeof userEvent.setup>;

/** The rules the draft was last saved with, as the request carried them. */
function savedBodies(calls: Call[]): unknown[] {
  return calls.filter((call) => call.method === "PUT" && call.path === `${WS}/consent/policy/draft`).map((call) => call.body);
}

/** Answers a save by giving every new rule an id, as the server does. */
function savingRoutes(extra: Record<string, Handler> = {}): Record<string, Handler> {
  return {
    [`PUT ${WS}/consent/policy/draft`]: async (request) => {
      const body = (await request.json()) as { revision: number; rules: { id?: string; name: string; condition?: unknown }[] };
      const rules = body.rules.map((rule, at) =>
        rule.id === "recorded_consent"
          ? BUILTIN
          : { id: rule.id ?? `77777777-0000-4000-8000-00000000000${at + 1}`, name: rule.name, builtin: false, condition: rule.condition },
      );
      return json(draftBody(rules, body.revision + 1));
    },
    ...extra,
  };
}

async function addRule(user: User, name: string) {
  await user.click(await screen.findByRole("button", { name: /Add a rule/ }));
  const field = await screen.findByLabelText("Rule name");
  await user.clear(field);
  await user.type(field, name);
}

describe("consent policy builder", () => {
  it("builds recorded consent, a cooldown and an account state, and sends exactly the shared payload", async () => {
    const calls = mockApi(routes("admin", savingRoutes()));
    const user = userEvent.setup();
    mount();

    expect(await screen.findByRole("heading", { name: /Recorded consent/ })).toBeInTheDocument();

    // Rule 2: an account-state field, moved above the built-in rule.
    await addRule(user, "Account closed");
    const condition = screen.getByRole("group", { name: "Condition" });
    await user.selectOptions(within(condition).getByLabelText("Field"), "account_state");
    await user.type(within(condition).getByLabelText("Value"), "closed");
    await user.click(screen.getByRole("button", { name: "Move Account closed up" }));

    // Rule 3: a cooldown since the last message sent.
    await addRule(user, "Cooldown since the last message");
    const second = screen.getAllByRole("group", { name: "Condition" }).at(-1)!;
    await user.selectOptions(within(second).getByLabelText("Type"), "messages");
    await user.selectOptions(within(second).getByLabelText("What to look at"), "last_time");
    await user.selectOptions(within(second).getByLabelText("Comparison"), "within_last");
    await user.click(within(second).getByRole("button", { name: "Set Value to 1 day" }));

    await user.click(screen.getByRole("button", { name: "Save draft" }));

    await waitFor(() => expect(savedBodies(calls)).toHaveLength(1));
    expect(savedBodies(calls)[0]).toEqual(launchPolicy);
    expect(await screen.findByText("Draft saved.")).toBeInTheDocument();
  });

  it("changes the order that is sent when a rule moves, and never removes the built-in rule", async () => {
    const rules = [
      { id: "recorded_consent", name: "Recorded consent", builtin: true, condition: null },
      { id: COOLDOWN_ID, name: "Quiet week", builtin: false, condition: { v: 1, messages: { channel: "email", agg: { fn: "count", op: "gte", value: 1 }, window: { last: "P7D" } } } },
    ];
    const calls = mockApi(routes("admin", { [`GET ${WS}/consent/policy/draft`]: () => json(draftBody(rules, 4, 2)), ...savingRoutes() }));
    const user = userEvent.setup();
    mount();

    const builtin = (await screen.findByRole("heading", { name: /Recorded consent/ })).closest("li")!;
    expect(within(builtin).queryByRole("button", { name: /Delete/ })).not.toBeInTheDocument();
    await user.click(within(builtin).getByRole("button", { name: "Show how Recorded consent decides" }));
    expect(within(builtin).queryByRole("button", { name: /Delete/ })).not.toBeInTheDocument();
    expect(within(builtin).getByRole("button", { name: "Move Recorded consent up" })).toBeDisabled();

    await user.click(within(builtin).getByRole("button", { name: "Move Recorded consent down" }));
    expect(screen.getByText("Moved Recorded consent down to position 2 of 2.")).toBeInTheDocument();
    // The focus stays on the control that was used, or on its partner when it cannot go further.
    expect(screen.getByRole("button", { name: "Move Recorded consent up" })).toHaveFocus();

    await user.click(screen.getByRole("button", { name: "Save draft" }));
    await waitFor(() => expect(savedBodies(calls)).toHaveLength(1));
    expect(savedBodies(calls)[0]).toMatchObject({
      revision: 4,
      rules: [{ id: COOLDOWN_ID, name: "Quiet week" }, { id: "recorded_consent", name: "Recorded consent" }],
    });
    const sent = savedBodies(calls)[0] as { rules: { condition?: unknown }[] };
    expect(sent.rules[1]).not.toHaveProperty("condition");
  });

  it("offers the message leaves in the rule builder and the dimensions to filter by", async () => {
    mockApi(routes("admin"));
    const user = userEvent.setup();
    mount();

    await addRule(user, "Sports cap");
    const condition = screen.getByRole("group", { name: "Condition" });
    const kinds = within(within(condition).getByLabelText("Type")).getAllByRole("option").map((option) => option.textContent);
    expect(kinds).toContain("Messages sent");
    expect(kinds).toContain("Consent");

    await user.selectOptions(within(condition).getByLabelText("Type"), "consent");
    expect(within(condition).queryByLabelText("Channel")).not.toBeInTheDocument();
    expect(within(condition).getByLabelText("Status")).toHaveValue("none");

    await user.selectOptions(within(condition).getByLabelText("Type"), "messages");
    await user.click(within(condition).getByRole("button", { name: "Add a dimension filter" }));
    await user.selectOptions(within(condition).getByLabelText("Dimension of dimension 1"), "vertical");
    await user.selectOptions(within(condition).getByLabelText("Value of dimension 1"), "sports");
    expect(within(condition).getByLabelText("Value of dimension 1")).toHaveValue("sports");
  });

  it("shows the problems of a refused draft on the rule that has them", async () => {
    const calls = mockApi(
      routes("admin", {
        [`PUT ${WS}/consent/policy/draft`]: () =>
          new Response(
            JSON.stringify({
              type: "about:blank",
              title: "Unprocessable Entity",
              status: 422,
              detail: "the consent policy is not valid",
              code: "consent_policy_invalid",
              issues: [{ pointer: "/rules/1/condition/trait", code: "unknown_trait" }],
            }),
            { status: 422, headers: { "content-type": "application/problem+json" } },
          ),
      }),
    );
    const user = userEvent.setup();
    mount();

    await addRule(user, "Ghost field");
    await user.click(screen.getByRole("button", { name: "Save draft" }));

    await waitFor(() => expect(savedBodies(calls)).toHaveLength(1));
    const card = (await screen.findByRole("heading", { name: /Ghost field/ })).closest("li")!;
    expect(await within(card).findByText("This field does not exist in the workspace.")).toBeInTheDocument();
    expect(within(card).getByText("1 problem")).toBeInTheDocument();
  });

  it("publishes the saved revision for an admin and tells a stale draft apart", async () => {
    const rules = [BUILTIN];
    const calls = mockApi(
      routes("admin", {
        [`GET ${WS}/consent/policy/draft`]: () => json(draftBody(rules, 3, 1)),
        [`POST ${WS}/consent/policy/publish`]: () => json({ version: 2, rules, published_by: OWNER.id, published_at: "2026-10-24T09:00:00Z" }),
      }),
    );
    const user = userEvent.setup();
    mount();

    expect(await screen.findByText("Draft saved · v1 live")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Publish…" }));
    const dialog = await screen.findByRole("dialog", { name: "Publish the policy?" });
    await user.click(within(dialog).getByRole("button", { name: "Publish" }));

    expect(await screen.findByText(/Published as version 2/)).toBeInTheDocument();
    const published = calls.find((call) => call.method === "POST" && call.path === `${WS}/consent/policy/publish`);
    expect(published?.body).toEqual({ revision: 3 });
    expect(screen.getByText("Draft saved · v2 live")).toBeInTheDocument();
  });

  it("says when someone else saved the draft first, and loads theirs", async () => {
    let reads = 0;
    mockApi(
      routes("member", {
        [`GET ${WS}/consent/policy/draft`]: () => {
          reads += 1;
          return json(draftBody([BUILTIN], reads));
        },
        [`PUT ${WS}/consent/policy/draft`]: () => problem(409, "Conflict", "the draft changed", undefined, "consent_policy_revision_mismatch"),
      }),
    );
    const user = userEvent.setup();
    mount();

    await addRule(user, "Mine");
    await user.click(screen.getByRole("button", { name: "Save draft" }));

    expect(await screen.findByText(/saved by someone else/)).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Load the latest draft" }));
    await waitFor(() => expect(reads).toBe(2));
    await waitFor(() => expect(screen.queryByRole("heading", { name: /Mine/ })).not.toBeInTheDocument());
  });

  it("hides publishing, history and the check from a member, and editing from a reader", async () => {
    mockApi(routes("member"));
    const first = mount();
    expect(await screen.findByRole("button", { name: "Save draft" })).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Publish…" })).not.toBeInTheDocument();
    expect(screen.queryByRole("tab", { name: "Version history" })).not.toBeInTheDocument();
    expect(screen.queryByRole("heading", { name: "Check a person" })).not.toBeInTheDocument();
    first.unmount();

    mockApi(routes("reader"));
    mount();
    expect(await screen.findByRole("heading", { name: /Recorded consent/ })).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Save draft" })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /Add a rule/ })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /Move Recorded consent/ })).not.toBeInTheDocument();
    expect(screen.getByText(/You can read the policy/)).toBeInTheDocument();
  });

  it("shows loading, then an error with a retry", async () => {
    let fail = true;
    mockApi(
      routes("admin", {
        [`GET ${WS}/consent/policy/draft`]: () => (fail ? problem(500, "Internal Server Error", "internal error") : json(draftBody())),
      }),
    );
    const user = userEvent.setup();
    mount();

    expect(await screen.findByRole("button", { name: /try again/i })).toBeInTheDocument();
    fail = false;
    await user.click(screen.getByRole("button", { name: /try again/i }));
    expect(await screen.findByRole("heading", { name: /Recorded consent/ })).toBeInTheDocument();
  });

  it("checks a person against the published policy and marks the rule that decided", async () => {
    const rules = [
      BUILTIN,
      { id: COOLDOWN_ID, name: "Quiet week", builtin: false, condition: { v: 1, messages: { channel: "email", agg: { fn: "count", op: "gte", value: 1 }, window: { last: "P7D" } } } },
    ];
    const calls = mockApi(
      routes("admin", {
        [`GET ${WS}/consent/policy/draft`]: () => json(draftBody(rules, 2, 2)),
        [`POST ${WS}/consent/policy/evaluate`]: () =>
          json({ allowed: false, rule: { id: COOLDOWN_ID, name: "Quiet week", unevaluable: false }, policy_version: 2 }),
      }),
    );
    const user = userEvent.setup();
    mount();

    await user.type(await screen.findByLabelText("Person"), PERSON);
    await user.selectOptions(screen.getByLabelText("Topic"), "offers");
    await user.selectOptions(screen.getByLabelText("Vertical"), "sports");
    await user.click(screen.getByRole("button", { name: "Check" }));

    const result = await screen.findByText(/Would skip/);
    expect(result).toBeInTheDocument();
    expect(screen.getByText("Quiet week", { selector: "strong" })).toBeInTheDocument();
    expect(screen.getByText(/Checked against published version 2/)).toBeInTheDocument();
    const sent = calls.find((call) => call.path === `${WS}/consent/policy/evaluate`);
    expect(sent?.body).toEqual({ person_id: PERSON, channel: "email", topic_key: "offers", dimensions: { vertical: "sports" } });
    const card = screen.getByRole("heading", { name: /Quiet week/ }).closest("li")!;
    expect(within(card).getByText("Decided the last check")).toBeInTheDocument();
  });

  it("lists the published versions in the history view", async () => {
    mockApi(
      routes("admin", {
        [`GET ${WS}/consent/policy/versions`]: () =>
          json([
            { version: 2, rules: [BUILTIN], published_by: OWNER.id, published_at: "2026-10-22T09:00:00Z" },
            { version: 1, rules: [BUILTIN], published_by: OWNER.id, published_at: "2026-10-20T09:00:00Z" },
          ]),
      }),
    );
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("tab", { name: "Version history" }));
    const list = await screen.findByRole("list", { name: "Published versions, newest first" });
    expect(within(list).getByText("Version 2")).toBeInTheDocument();
    expect(within(list).getByText("Version 1")).toBeInTheDocument();
  });
});
