import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { json, MEMBER, mockApi, OWNER, problem, session } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { ConflictsPage } from "./ConflictsPage";
import { kindLabel } from "../../ui";
import { reasonExplanation, reasonLabel, resolutionLabel } from "./conflictLabels";

const A = "aaaaaaaa-0000-4000-8000-000000000001";
const B = "bbbbbbbb-0000-4000-8000-000000000002";

const CONFLICT = {
  id: "c0000000-0000-4000-8000-000000000001",
  kind: "email",
  person_id: A,
  other_person_id: B,
  reason: "unique_kind",
  created_at: "2026-10-01T09:00:00Z",
  resolved_at: null,
  resolution: null,
};

const DETAIL = {
  conflict: CONFLICT,
  people: [
    {
      person_id: A,
      identities: [{ kind: "external:crm", value: "2", masked: false, shared: false }],
    },
    {
      person_id: B,
      identities: [
        { kind: "email", value: "ada@example.test", masked: false, shared: true },
        { kind: "external:crm", value: "1", masked: false, shared: false },
      ],
    },
  ],
};

const MASKED_DETAIL = {
  conflict: CONFLICT,
  people: DETAIL.people.map((person) => ({
    ...person,
    identities: person.identities.map((identity) => ({ ...identity, value: null, masked: true })),
  })),
};

const LIST = "GET /api/v1/workspaces/ws1/identity-conflicts";
const DETAIL_ROUTE = `GET /api/v1/workspaces/ws1/identity-conflicts/${CONFLICT.id}`;
const RESOLVE_ROUTE = `POST /api/v1/workspaces/ws1/identity-conflicts/${CONFLICT.id}/resolve`;

function members(role: "admin" | "editor", userId: string) {
  return [{ user_id: userId, email: "someone@example.test", role, added_at: "2026-10-01T09:00:00Z" }];
}

function mountAsAdmin(extra: Parameters<typeof mockApi>[0] = {}) {
  return mockApi({
    "GET /api/v1/session": () => json(session(OWNER, "csrf-conflicts")),
    "GET /api/v1/workspaces/ws1/members": () => json(members("admin", OWNER.id)),
    [LIST]: () => json({ conflicts: [CONFLICT], next_cursor: null }),
    [DETAIL_ROUTE]: () => json(DETAIL),
    ...extra,
  });
}

function mount() {
  return renderRoute("/w/ws1/people/conflicts", "/w/:ws/people/conflicts", <ConflictsPage />);
}

describe("identity labels", () => {
  it("name every kind, reason and outcome in plain words", () => {
    expect(kindLabel("email")).toBe("Email address");
    expect(kindLabel("phone")).toBe("Phone number");
    expect(kindLabel("anonymous_id")).toBe("Anonymous ID");
    expect(kindLabel("external:crm")).toBe("External ID (crm)");
    expect(kindLabel("fax")).toBe("fax");
    expect(reasonLabel("unique_kind")).toBe("One per person");
    expect(reasonLabel("max_per_person")).toBe("Too many for one person");
    expect(reasonLabel("merge_deferred")).toBe("Not merged");
    expect(reasonLabel("other")).toBe("other");
    expect(reasonExplanation("unique_kind", "email")).toMatch(/two different values/);
    expect(reasonExplanation("max_per_person", "phone")).toMatch(/more identifiers/);
    expect(reasonExplanation("merge_deferred", "email")).toMatch(/not merged/);
    expect(resolutionLabel("merged")).toBe("Merged");
    expect(resolutionLabel("kept_separate")).toBe("Kept separate");
    expect(resolutionLabel("identifier_moved")).toBe("Identifier moved");
    expect(resolutionLabel(null)).toBe("Open");
  });
});

describe("identity conflicts", () => {
  it("lists open conflicts with the shared kind and why they were not merged", async () => {
    mountAsAdmin();
    mount();

    const table = await screen.findByRole("table", { name: "Open identity conflicts" });
    expect(within(table).getByText("Email address")).toBeInTheDocument();
    expect(within(table).getByText("One per person")).toBeInTheDocument();
    expect(within(table).getByRole("button", { name: /^Review Email address conflict/ })).toBeInTheDocument();
  });

  it("asks for the resolved list when the filter changes", async () => {
    const resolved = { ...CONFLICT, resolved_at: "2026-10-02T09:00:00Z", resolution: "kept_separate" };
    const calls = mountAsAdmin({
      [LIST]: (request) =>
        new URL(request.url).searchParams.get("status") === "resolved"
          ? json({ conflicts: [resolved], next_cursor: null })
          : json({ conflicts: [CONFLICT], next_cursor: null }),
    });
    const user = userEvent.setup();
    mount();

    await screen.findByRole("table", { name: "Open identity conflicts" });
    await user.selectOptions(screen.getByLabelText("Show"), "resolved");

    const table = await screen.findByRole("table", { name: "Resolved identity conflicts" });
    expect(within(table).getByText("Kept separate")).toBeInTheDocument();
    expect(within(table).getByRole("button", { name: /^View Email address conflict/ })).toBeInTheDocument();
    const statuses = calls.filter((call) => call.path.endsWith("/identity-conflicts")).map((call) => call.search);
    expect(statuses[0]).toContain("status=open");
    expect(statuses.at(-1)).toContain("status=resolved");
  });

  it("shows an empty state for each filter, and a retry when loading fails", async () => {
    mountAsAdmin({ [LIST]: () => json({ conflicts: [], next_cursor: null }) });
    const user = userEvent.setup();
    const first = mount();
    expect(await screen.findByText("No open conflicts")).toBeInTheDocument();
    await user.selectOptions(screen.getByLabelText("Show"), "resolved");
    expect(await screen.findByText("No resolved conflicts yet")).toBeInTheDocument();
    first.unmount();

    mountAsAdmin({ [LIST]: () => problem(500, "Internal Server Error", "internal error") });
    mount();
    expect(await screen.findByRole("button", { name: /try again/i })).toBeInTheDocument();
  });

  it("shows a loading state while the first page loads", async () => {
    mountAsAdmin({ [LIST]: () => new Promise<Response>(() => undefined) });
    mount();

    expect(await screen.findByText("Loading conflicts")).toBeInTheDocument();
  });

  it("loads the next page with the cursor", async () => {
    const second = { ...CONFLICT, id: "c0000000-0000-4000-8000-000000000002", kind: "phone" };
    const calls = mountAsAdmin({
      [LIST]: (request) =>
        new URL(request.url).searchParams.get("cursor") === "cursor-1"
          ? json({ conflicts: [second], next_cursor: null })
          : json({ conflicts: [CONFLICT], next_cursor: "cursor-1" }),
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Show more" }));

    expect(await screen.findByText("Phone number")).toBeInTheDocument();
    expect(screen.getByText("Email address")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Show more" })).not.toBeInTheDocument();
    expect(calls.some((call) => call.search.includes("cursor=cursor-1"))).toBe(true);
  });

  it("shows the two people side by side with the shared identifier marked", async () => {
    mountAsAdmin();
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: /^Review Email address conflict/ }));

    const dialog = within(await screen.findByRole("dialog", { name: "Review conflict" }));
    const personA = within(await dialog.findByRole("region", { name: "Person A" }));
    const personB = within(dialog.getByRole("region", { name: "Person B" }));
    expect(personA.getByText("2")).toBeInTheDocument();
    expect(personB.getByText("ada@example.test")).toBeInTheDocument();
    expect(personB.getByText("Shared")).toBeInTheDocument();
    expect(personA.queryByText("Shared")).not.toBeInTheDocument();
    expect(dialog.getByText(/two different values of a kind that a person holds only once/)).toBeInTheDocument();
  });

  it("keeps the two people separate with the CSRF token and reloads the list", async () => {
    let open = true;
    const calls = mountAsAdmin({
      [LIST]: () => json({ conflicts: open ? [CONFLICT] : [], next_cursor: null }),
      [RESOLVE_ROUTE]: () => {
        open = false;
        return json({ ...CONFLICT, resolved_at: "2026-10-02T09:00:00Z", resolution: "kept_separate" });
      },
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: /^Review Email address conflict/ }));
    await user.click(await screen.findByRole("button", { name: "Keep separate" }));

    expect(await screen.findByText("No open conflicts")).toBeInTheDocument();
    expect(screen.getByRole("status")).toHaveTextContent("Kept the two people separate.");
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    const post = calls.find((call) => call.method === "POST");
    expect(post?.body).toEqual({ outcome: "keep_separate" });
    expect(post?.headers.get("x-csrf-token")).toBe("csrf-conflicts");
  });

  it("moves the identifier only after a confirmation that names its kind", async () => {
    const calls = mountAsAdmin({
      [RESOLVE_ROUTE]: () => json({ ...CONFLICT, resolved_at: "2026-10-02T09:00:00Z", resolution: "identifier_moved" }),
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: /^Review Email address conflict/ }));
    await user.click(await screen.findByRole("button", { name: "Move identifier to person A" }));

    expect(screen.getByRole("button", { name: "Move identifier to person A" })).toBeInTheDocument();
    expect(calls.some((call) => call.method === "POST")).toBe(false);
    const confirm = within(screen.getByRole("dialog", { name: "Move the email address to person A?" }));
    expect(confirm.getByText(/will identify person A instead of person B/)).toBeInTheDocument();
    await user.click(confirm.getByRole("button", { name: "Move email address" }));

    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
    expect(screen.getByRole("status")).toHaveTextContent("Moved the email address to person A.");
    expect(calls.find((call) => call.method === "POST")?.body).toEqual({
      outcome: "move_identifier",
      to_person_id: A,
    });
  });

  it("offers a move only to the person who does not hold the identifier", async () => {
    mountAsAdmin();
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: /^Review Email address conflict/ }));

    expect(await screen.findByRole("button", { name: "Move identifier to person A" })).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Move identifier to person B" })).not.toBeInTheDocument();
  });

  it("cancels the confirmation with Escape and puts focus back on the move button", async () => {
    const calls = mountAsAdmin();
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: /^Review Email address conflict/ }));
    const move = await screen.findByRole("button", { name: "Move identifier to person A" });
    await user.click(move);
    expect(screen.getByRole("dialog", { name: "Move the email address to person A?" })).toBeInTheDocument();
    await user.keyboard("{Escape}");

    expect(screen.queryByRole("dialog", { name: "Move the email address to person A?" })).not.toBeInTheDocument();
    expect(screen.getByRole("dialog", { name: "Review conflict" })).toBeInTheDocument();
    expect(move).toHaveFocus();
    expect(calls.some((call) => call.method === "POST")).toBe(false);
  });

  it("closes the review with Escape and returns focus to the row's button", async () => {
    mountAsAdmin();
    const user = userEvent.setup();
    mount();

    const review = await screen.findByRole("button", { name: /^Review Email address conflict/ });
    await user.click(review);
    await screen.findByRole("region", { name: "Person A" });
    await user.keyboard("{Escape}");

    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(review).toHaveFocus();
  });

  it("shows the server's reason when a move is refused and keeps the dialog open", async () => {
    mountAsAdmin({
      [RESOLVE_ROUTE]: () => problem(409, "Conflict", "the move would give that person two different values of a kind it holds once"),
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: /^Review Email address conflict/ }));
    await user.click(await screen.findByRole("button", { name: "Move identifier to person A" }));
    const confirm = within(screen.getByRole("dialog", { name: "Move the email address to person A?" }));
    await user.click(confirm.getByRole("button", { name: "Move email address" }));

    expect(await confirm.findByText(/two different values of a kind it holds once/)).toBeInTheDocument();
    expect(screen.getByRole("dialog", { name: "Move the email address to person A?" })).toBeInTheDocument();
    expect(confirm.getByRole("button", { name: "Move email address" })).toBeEnabled();
  });

  it("tells the user to reload when the people changed under the conflict", async () => {
    mountAsAdmin({
      [RESOLVE_ROUTE]: () => problem(409, "Conflict", "conflict_changed: the people of this conflict changed, reload it"),
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: /^Review Email address conflict/ }));
    await user.click(await screen.findByRole("button", { name: "Move identifier to person A" }));
    const confirm = within(screen.getByRole("dialog", { name: "Move the email address to person A?" }));
    await user.click(confirm.getByRole("button", { name: "Move email address" }));

    expect(await confirm.findByText("This conflict changed; reload")).toBeInTheDocument();
  });

  it("shows a loading state and an error with a retry while the people load", async () => {
    mountAsAdmin({ [DETAIL_ROUTE]: () => problem(500, "Internal Server Error", "internal error") });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: /^Review Email address conflict/ }));

    const dialog = within(await screen.findByRole("dialog", { name: "Review conflict" }));
    expect(await dialog.findByRole("button", { name: /try again/i })).toBeInTheDocument();
    expect(dialog.queryByRole("button", { name: "Keep separate" })).not.toBeInTheDocument();
  });

  it("hides values and actions from a member who is not an admin", async () => {
    mockApi({
      "GET /api/v1/session": () => json(session(MEMBER)),
      "GET /api/v1/workspaces/ws1/members": () => json(members("editor", MEMBER.id)),
      [LIST]: () => json({ conflicts: [CONFLICT], next_cursor: null }),
      [DETAIL_ROUTE]: () => json(MASKED_DETAIL),
    });
    const user = userEvent.setup();
    mount();

    expect(await screen.findByText(/Identifier values are hidden from you/)).toBeInTheDocument();
    await user.click(await screen.findByRole("button", { name: /^Review Email address conflict/ }));

    const dialog = within(await screen.findByRole("dialog", { name: "Review conflict" }));
    expect((await dialog.findAllByText("Hidden")).length).toBe(3);
    expect(dialog.queryByText("ada@example.test")).not.toBeInTheDocument();
    expect(dialog.getByText(/Only workspace admins and the instance owner can resolve conflicts/)).toBeInTheDocument();
    expect(dialog.queryByRole("button", { name: "Keep separate" })).not.toBeInTheDocument();
    expect(dialog.queryByRole("button", { name: /^Move identifier/ })).not.toBeInTheDocument();
  });

  it("shows a closed conflict's outcome without actions", async () => {
    const resolved = { ...CONFLICT, resolved_at: "2026-10-02T09:00:00Z", resolution: "kept_separate" };
    mountAsAdmin({
      [LIST]: () => json({ conflicts: [resolved], next_cursor: null }),
      [DETAIL_ROUTE]: () => json({ ...DETAIL, conflict: resolved }),
    });
    const user = userEvent.setup();
    mount();

    await user.selectOptions(await screen.findByLabelText("Show"), "resolved");
    await user.click(await screen.findByRole("button", { name: /^View Email address conflict/ }));

    const dialog = within(await screen.findByRole("dialog", { name: "Review conflict" }));
    expect(await dialog.findByRole("region", { name: "Person A" })).toBeInTheDocument();
    expect(dialog.getAllByText("Kept separate").length).toBeGreaterThan(0);
    expect(dialog.queryByRole("button", { name: "Keep separate" })).not.toBeInTheDocument();
  });
});
