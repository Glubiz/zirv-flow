import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { json, MEMBER, mockApi, OWNER, problem, session } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { ProfilePage } from "./ProfilePage";

const PERSON = "aaaaaaaa-0000-4000-8000-000000000001";
const MERGED = "bbbbbbbb-0000-4000-8000-000000000002";
const LIST_ID = "cccccccc-0000-4000-8000-000000000003";
const TRACE = "0123456789abcdef0123456789abcdef";

const PROFILE_ROUTE = `GET /api/v1/workspaces/ws1/people/${PERSON}`;
const CONSENT_ROUTE = `/api/v1/workspaces/ws1/people/${PERSON}/consent`;
const LISTS_ROUTE = `GET /api/v1/workspaces/ws1/people/${PERSON}/lists`;
const TIMELINE_ROUTE = `GET /api/v1/workspaces/ws1/people/${PERSON}/timeline`;
const TOPICS_ROUTE = "GET /api/v1/workspaces/ws1/topics";
const MEMBERS_ROUTE = "GET /api/v1/workspaces/ws1/members";

function identity(kind: string, value: string | null) {
  return { kind, value, masked: value === null };
}

function profile(overrides: Record<string, unknown> = {}) {
  return {
    id: PERSON,
    merged_from: null,
    display: { kind: "email", value: "ada@example.test", masked: false },
    created_at: "2026-10-01T09:00:00Z",
    updated_at: "2026-10-02T09:00:00Z",
    identity_groups: [
      { source: "crm", identities: [identity("email", "ada@example.test")] },
      { source: "shop", identities: [identity("email", "ada@example.test"), identity("external:shop", "c-9")] },
    ],
    observed: [identity("anonymous_id", "anon-1")],
    traits: [
      {
        key: "city",
        trait_type: "string",
        value: "Aarhus",
        masked: false,
        pii: true,
        source: "crm",
        observed_at: "2026-10-01T10:00:00Z",
      },
      { key: "plan", trait_type: "string", value: "pro", masked: false, pii: false, source: "shop", observed_at: null },
    ],
    consent: [],
    lists: [],
    merges: [{ merged_person_id: MERGED, merged_at: "2026-10-03T09:00:00Z" }],
    ...overrides,
  };
}

function eventEntry(id: string, at: string, overrides: Record<string, unknown> = {}) {
  return {
    type: "event",
    id,
    at,
    name: "order_completed",
    properties: { total: 42, coupon: "SECRET" },
    masked: [],
    validation_state: "valid",
    flags: [],
    trace_id: TRACE,
    ...overrides,
  };
}

function decisionEntry(id: string, at: string, overrides: Record<string, unknown> = {}) {
  return {
    type: "decision",
    id,
    at,
    action: "person.merge",
    outcome: "merged",
    reason: null,
    cause_kind: "event",
    subject_ids: { person: PERSON },
    trace_id: TRACE,
    ...overrides,
  };
}

const NOW_PAGE = {
  entries: [
    decisionEntry("d1", "2026-10-03T09:00:00Z"),
    eventEntry("e1", "2026-10-02T09:00:00Z"),
    decisionEntry("d2", "2026-10-01T09:00:00Z", { action: "trait.change", outcome: "changed", reason: "definition_changed" }),
  ],
  next_before: null,
};

function members(role: "admin" | "member", userId: string) {
  return [{ user_id: userId, email: "someone@example.test", role, added_at: "2026-10-01T09:00:00Z" }];
}

function states(...items: Record<string, unknown>[]) {
  return json({ person_id: PERSON, states: items });
}

const SUBSCRIBED = {
  channel: "email",
  topic_key: null,
  topic_name: null,
  status: "subscribed",
  explicit: true,
  dimensions: {},
  changed_at: "2026-10-02T09:00:00Z",
};

function asOwner(extra: Parameters<typeof mockApi>[0] = {}) {
  return mockApi({
    "GET /api/v1/session": () => json(session(OWNER, "csrf-profile")),
    [MEMBERS_ROUTE]: () => json(members("admin", OWNER.id)),
    [PROFILE_ROUTE]: () => json(profile()),
    [`GET ${CONSENT_ROUTE}`]: () => states(SUBSCRIBED),
    [LISTS_ROUTE]: () =>
      json([{ list_id: LIST_ID, name: "Newsletter", added_at: "2026-10-02T09:00:00Z", source: "import" }]),
    [TIMELINE_ROUTE]: () => json(NOW_PAGE),
    [TOPICS_ROUTE]: () => json([]),
    ...extra,
  });
}

function mount(id = PERSON) {
  return renderRoute(`/w/ws1/people/${id}`, "/w/:ws/people/:id", <ProfilePage />);
}

describe("a person's profile", () => {
  it("shows identities by source, the observed ones and the traits with their provenance", async () => {
    asOwner();
    mount();

    expect(await screen.findByRole("heading", { level: 1, name: "ada@example.test" })).toBeInTheDocument();
    const crm = await screen.findByRole("region", { name: "Identities from crm" });
    expect(within(crm).getByText("ada@example.test")).toBeInTheDocument();
    const shop = screen.getByRole("region", { name: "Identities from shop" });
    expect(within(shop).getByText("External ID (shop)")).toBeInTheDocument();
    expect(within(shop).getByText("c-9")).toBeInTheDocument();
    const seen = screen.getByRole("region", { name: "Identities seen on events" });
    expect(within(seen).getByText("Anonymous ID")).toBeInTheDocument();
    const table = screen.getByRole("table", { name: "Field values with the source and time of each" });
    const city = within(table).getByRole("row", { name: /city/ });
    expect(within(city).getByText("Aarhus")).toBeInTheDocument();
    expect(within(city).getByText("crm")).toBeInTheDocument();
    expect(screen.getByRole("link", { name: "All people" })).toHaveAttribute("href", "/w/ws1/people");
    expect(screen.queryByText(/was merged into this one/)).not.toBeInTheDocument();
  });

  it("lists the people merged into this one, each linking to its own address", async () => {
    asOwner();
    mount();

    const panel = await screen.findByRole("region", { name: "Merged people" });
    expect(within(panel).getByRole("link", { name: /Person/ })).toHaveAttribute("href", `/w/ws1/people/${MERGED}`);
  });

  it("tells you when the address you opened was merged into this person", async () => {
    asOwner({ [`GET /api/v1/workspaces/ws1/people/${MERGED}`]: () => json(profile({ merged_from: MERGED })) });
    mount(MERGED);

    expect(await screen.findByText(/was merged into this one/)).toBeInTheDocument();
  });

  it("hides identifiers and PII traits from a member and says why", async () => {
    asOwner({
      "GET /api/v1/session": () => json(session(MEMBER)),
      [MEMBERS_ROUTE]: () => json(members("member", MEMBER.id)),
      [PROFILE_ROUTE]: () =>
        json(
          profile({
            display: { kind: "email", value: null, masked: true },
            identity_groups: [{ source: "crm", identities: [identity("email", null)] }],
            observed: [],
            traits: [
              { key: "city", trait_type: "string", value: null, masked: true, pii: true, source: "crm", observed_at: null },
              { key: "plan", trait_type: "string", value: "pro", masked: false, pii: false, source: "shop", observed_at: null },
            ],
          }),
        ),
    });
    mount();

    expect(await screen.findByRole("heading", { level: 1, name: /Person aaaa/ })).toBeInTheDocument();
    expect(screen.getByText(/Email address, hidden from you/)).toBeInTheDocument();
    const crm = await screen.findByRole("region", { name: "Identities from crm" });
    expect(within(crm).getByText("Hidden")).toBeInTheDocument();
    expect(within(crm).getByText(/only workspace admins can see it/)).toBeInTheDocument();
    const table = screen.getByRole("table", { name: "Field values with the source and time of each" });
    const city = within(table).getByRole("row", { name: /city/ });
    expect(within(city).getByText("Hidden")).toBeInTheDocument();
    expect(within(city).queryByText("Aarhus")).not.toBeInTheDocument();
    expect(within(table).getByText("pro")).toBeInTheDocument();
  });

  it("shows empty identity and field panels for a person with nothing yet", async () => {
    asOwner({
      [PROFILE_ROUTE]: () => json(profile({ display: null, identity_groups: [], observed: [], traits: [], merges: [] })),
    });
    mount();

    expect(await screen.findByText("No identifiers")).toBeInTheDocument();
    expect(screen.getByText("No field values yet")).toBeInTheDocument();
    expect(screen.getByRole("heading", { level: 1, name: /Person aaaa/ })).toBeInTheDocument();
  });

  it("shows a loading state, then an error with a retry when the profile fails", async () => {
    let attempts = 0;
    asOwner({
      [PROFILE_ROUTE]: () => {
        attempts += 1;
        return attempts === 1 ? problem(500, "Internal Server Error", "internal error") : json(profile());
      },
    });
    const user = userEvent.setup();
    mount();

    expect(await screen.findByRole("alert")).toHaveTextContent("internal error");
    await user.click(screen.getByRole("button", { name: "Try again" }));
    expect(await screen.findByRole("heading", { level: 1, name: "ada@example.test" })).toBeInTheDocument();
  });

  it("shows a loading state while the profile loads", async () => {
    asOwner({ [PROFILE_ROUTE]: () => new Promise<Response>(() => undefined) });
    mount();

    expect(await screen.findByText("Loading the profile")).toBeInTheDocument();
  });

  it("shows a 404 for a person who is not in the workspace", async () => {
    asOwner({ [PROFILE_ROUTE]: () => problem(404, "Not Found", "no such resource") });
    mount();

    expect(await screen.findByRole("alert")).toHaveTextContent("no such resource");
    expect(screen.queryByRole("heading", { name: "Timeline" })).not.toBeInTheDocument();
  });
});

describe("a person's lists and consent", () => {
  it("shows the lists with links", async () => {
    asOwner();
    mount();

    const panel = await screen.findByRole("region", { name: "Lists" });
    expect(await within(panel).findByRole("link", { name: "Newsletter" })).toHaveAttribute("href", `/w/ws1/lists/${LIST_ID}`);
    expect(within(panel).getByText(/CSV import/)).toBeInTheDocument();
  });

  it("shows the consent states", async () => {
    asOwner({
      [`GET ${CONSENT_ROUTE}`]: () =>
        states(SUBSCRIBED, { ...SUBSCRIBED, topic_key: "news", topic_name: "News", status: "unsubscribed" }),
    });
    mount();

    const panel = await screen.findByRole("region", { name: "Consent" });
    expect(await within(panel).findByText("All email", { selector: ".strong" })).toBeInTheDocument();
    expect(within(panel).getByText("Subscribed", { selector: ".pill" })).toBeInTheDocument();
    expect(within(panel).getByText("News")).toBeInTheDocument();
    expect(within(panel).getByText("Unsubscribed", { selector: ".pill" })).toBeInTheDocument();
  });

  it("sets consent for the whole channel and shows the new state", async () => {
    let current = states(SUBSCRIBED);
    const calls = asOwner({
      [`GET ${CONSENT_ROUTE}`]: () => current.clone(),
      [`PUT ${CONSENT_ROUTE}`]: () => {
        current = states({ ...SUBSCRIBED, status: "unsubscribed" });
        return current.clone();
      },
    });
    const user = userEvent.setup();
    mount();

    const panel = await screen.findByRole("region", { name: "Consent" });
    await within(panel).findByText("All email", { selector: ".strong" });
    await user.selectOptions(within(panel).getByLabelText("Choice"), "unsubscribed");
    await user.click(within(panel).getByRole("button", { name: "Record choice" }));

    expect(await within(panel).findByText("Recorded: unsubscribed.")).toBeInTheDocument();
    await waitFor(() => {
      expect(within(panel).getByText("Unsubscribed", { selector: ".pill" })).toBeInTheDocument();
    });
    const put = calls.find((call) => call.method === "PUT");
    expect(put?.body).toEqual({ channel: "email", status: "unsubscribed", topic_key: null });
    expect(put?.headers.get("x-csrf-token")).toBe("csrf-profile");
  });

  it("sets consent for one topic and shows a refusal", async () => {
    const calls = asOwner({
      [TOPICS_ROUTE]: () =>
        json([
          { id: "t1", key: "news", name: "News", description: "", channel: "email", policy: "opt_out", archived: false, created_at: "2026-10-01T09:00:00Z", updated_at: "2026-10-01T09:00:00Z" },
          { id: "t2", key: "old", name: "Old", description: "", channel: "email", policy: "opt_in", archived: true, created_at: "2026-10-01T09:00:00Z", updated_at: "2026-10-01T09:00:00Z" },
        ]),
      [`PUT ${CONSENT_ROUTE}`]: () => problem(422, "Unprocessable Entity", "topic_key: is not a topic of this workspace"),
    });
    const user = userEvent.setup();
    mount();

    const panel = await screen.findByRole("region", { name: "Consent" });
    const applies = await within(panel).findByLabelText("Applies to");
    await waitFor(() => {
      expect(within(applies).getAllByRole("option").map((option) => option.textContent)).toEqual(["All email", "News"]);
    });
    await user.selectOptions(applies, "news");
    await user.click(within(panel).getByRole("button", { name: "Record choice" }));

    expect(await within(panel).findByRole("alert")).toHaveTextContent("is not a topic of this workspace");
    expect(calls.find((call) => call.method === "PUT")?.body).toEqual({
      channel: "email",
      status: "unsubscribed",
      topic_key: "news",
    });
  });

  it("does not offer to record a choice to someone who cannot change anything", async () => {
    asOwner({
      "GET /api/v1/session": () => json(session(MEMBER)),
      [MEMBERS_ROUTE]: () => json(members("admin", "someone-else")),
    });
    mount();

    const panel = await screen.findByRole("region", { name: "Consent" });
    await within(panel).findByText("All email");
    expect(within(panel).queryByRole("button", { name: "Record choice" })).not.toBeInTheDocument();
  });

  it("shows empty states for no consent and no lists", async () => {
    asOwner({ [`GET ${CONSENT_ROUTE}`]: () => states(), [LISTS_ROUTE]: () => json([]) });
    mount();

    expect(await screen.findByText("No choice on record")).toBeInTheDocument();
    expect(await screen.findByText("On no list")).toBeInTheDocument();
  });
});

describe("a person's timeline", () => {
  it("interleaves events and decisions newest first and links each to its trace", async () => {
    asOwner();
    mount();

    const list = await screen.findByRole("list", { name: "Timeline, newest first" });
    const items = within(list).getAllByRole("listitem");
    expect(items).toHaveLength(3);
    expect(within(items[0]!).getByText("Merged with another person")).toBeInTheDocument();
    expect(within(items[0]!).getByText("Merged")).toBeInTheDocument();
    expect(within(items[1]!).getByText("order_completed")).toBeInTheDocument();
    expect(within(items[1]!).getByText("Event")).toBeInTheDocument();
    expect(within(items[2]!).getByText("Fields changed")).toBeInTheDocument();
    expect(within(items[2]!).getByText(/definition changed/)).toBeInTheDocument();
    for (const item of items) {
      expect(within(item).getByRole("link", { name: /View trace/ })).toHaveAttribute("href", `/w/ws1/traces/${TRACE}`);
    }
  });

  it("discloses an event's properties and shows masked ones as Hidden", async () => {
    asOwner({
      [TIMELINE_ROUTE]: () =>
        json({
          entries: [eventEntry("e1", "2026-10-02T09:00:00Z", { properties: { total: 42, coupon: null }, masked: ["coupon"], flags: ["late"] })],
          next_before: null,
        }),
    });
    const user = userEvent.setup();
    mount();

    const summary = await screen.findByText("Properties");
    expect(screen.getByText("Flags: late")).toBeInTheDocument();
    await user.click(summary);
    const details = summary.closest("details")!;
    expect(within(details).getByText("total")).toBeInTheDocument();
    expect(within(details).getByText("42")).toBeInTheDocument();
    expect(within(details).getByText("Hidden")).toBeInTheDocument();
    expect(within(details).queryByText("SECRET")).not.toBeInTheDocument();
  });

  it("marks an event that is not valid", async () => {
    asOwner({
      [TIMELINE_ROUTE]: () =>
        json({ entries: [eventEntry("e1", "2026-10-02T09:00:00Z", { validation_state: "flagged", properties: {} })], next_before: null }),
    });
    mount();

    expect(await screen.findByText("Flagged")).toBeInTheDocument();
    expect(screen.queryByText("Properties")).not.toBeInTheDocument();
  });

  it("loads older entries when asked", async () => {
    const calls = asOwner({
      [TIMELINE_ROUTE]: (request) =>
        new URL(request.url).searchParams.get("before") === "cursor-1"
          ? json({ entries: [decisionEntry("d9", "2026-09-01T09:00:00Z", { action: "consent.change", outcome: "changed" })], next_before: null })
          : json({ entries: [eventEntry("e1", "2026-10-02T09:00:00Z")], next_before: "cursor-1" }),
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Load older" }));

    expect(await screen.findByText("Consent changed")).toBeInTheDocument();
    expect(screen.getByText("order_completed")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Load older" })).not.toBeInTheDocument();
    const queries = calls.filter((call) => call.path.endsWith("/timeline")).map((call) => call.search);
    expect(queries).toEqual(["?limit=25&kinds=event%2Cdecision", "?limit=25&kinds=event%2Cdecision&before=cursor-1"]);
  });

  it("asks for one kind only when the other is switched off, and never both off", async () => {
    const calls = asOwner();
    const user = userEvent.setup();
    mount();

    await screen.findByRole("list", { name: "Timeline, newest first" });
    await user.click(screen.getByRole("checkbox", { name: "Decisions" }));

    await waitFor(() => {
      expect(calls.some((call) => call.search.includes("kinds=event&") || call.search.endsWith("kinds=event"))).toBe(true);
    });
    expect(screen.getByRole("checkbox", { name: "Events" })).toBeDisabled();
    expect(screen.getByRole("checkbox", { name: "Decisions" })).not.toBeDisabled();
  });

  it("says nothing has happened for a person with an empty timeline", async () => {
    asOwner({ [TIMELINE_ROUTE]: () => json({ entries: [], next_before: null }) });
    mount();

    expect(await screen.findByText("Nothing has happened yet")).toBeInTheDocument();
  });

  it("shows a loading state and an error with a retry for the timeline", async () => {
    let attempts = 0;
    asOwner({
      [TIMELINE_ROUTE]: () => {
        attempts += 1;
        return attempts === 1 ? problem(503, "Service Unavailable", "the query took too long") : json(NOW_PAGE);
      },
    });
    const user = userEvent.setup();
    mount();

    const alert = await screen.findByText("Service Unavailable");
    expect(alert).toBeInTheDocument();
    const retry = screen.getAllByRole("button", { name: "Try again" });
    await user.click(retry[retry.length - 1]!);
    expect(await screen.findByText("Merged with another person")).toBeInTheDocument();
  });

  it("has a Profile and a Messages tab, and the Messages tab loads the message log", async () => {
    asOwner({
      [`GET /api/v1/workspaces/ws1/people/${PERSON}/messages`]: () => json({ messages: [], next_before: null }),
      "GET /api/v1/workspaces/ws1/templates": () => json([]),
    });
    const user = userEvent.setup();
    mount();

    const profileTab = await screen.findByRole("tab", { name: "Profile" });
    expect(profileTab).toHaveAttribute("aria-selected", "true");
    expect(screen.queryByText("No messages to this person yet.")).not.toBeInTheDocument();
    await user.click(screen.getByRole("tab", { name: "Messages" }));

    expect(screen.getByRole("tab", { name: "Messages" })).toHaveAttribute("aria-selected", "true");
    expect(await screen.findByText("No messages to this person yet.")).toBeInTheDocument();
    expect(screen.queryByRole("region", { name: "Identities from crm" })).not.toBeInTheDocument();
    await user.click(screen.getByRole("tab", { name: "Profile" }));
    expect(await screen.findByRole("region", { name: "Identities from crm" })).toBeInTheDocument();
  });
});
