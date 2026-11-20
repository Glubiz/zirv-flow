import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { json, MEMBER, mockApi, OWNER, problem, session } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { reasonText } from "./ImportDialog";
import { ListDetailPage } from "./ListDetailPage";

const LIST_ID = "11111111-0000-4000-8000-000000000001";
const LIST = {
  id: LIST_ID,
  name: "Newsletter",
  description: "Monthly digest",
  member_count: 2,
  created_at: "2026-10-01T09:00:00Z",
  updated_at: "2026-10-01T09:00:00Z",
  created_by: null,
};
const ADA = "aaaaaaaa-0000-4000-8000-000000000001";
const BOB = "bbbbbbbb-0000-4000-8000-000000000002";

function member(person: string, kind: string, value: string | null, source = "ui") {
  return {
    person_id: person,
    added_at: "2026-10-02T09:00:00Z",
    source,
    identifier: { kind, value, masked: value === null },
  };
}

const PAGE = { members: [member(ADA, "email", "ada@example.test"), member(BOB, "external:crm", "42", "import")], next_cursor: null };

const LIST_ROUTE = `GET /api/v1/workspaces/ws1/lists/${LIST_ID}`;
const MEMBERS_ROUTE = `GET /api/v1/workspaces/ws1/lists/${LIST_ID}/members`;
const ADD_ROUTE = `POST /api/v1/workspaces/ws1/lists/${LIST_ID}/members`;
const REMOVE_ROUTE = `POST /api/v1/workspaces/ws1/lists/${LIST_ID}/members/remove`;
const IMPORT_ROUTE = `POST /api/v1/workspaces/ws1/lists/${LIST_ID}/members/import`;

function roles(role: "admin" | "editor", userId: string) {
  return [{ user_id: userId, email: "someone@example.test", role, added_at: "2026-10-01T09:00:00Z" }];
}

function asAdmin(extra: Parameters<typeof mockApi>[0] = {}) {
  return mockApi({
    "GET /api/v1/session": () => json(session(OWNER, "csrf-detail")),
    "GET /api/v1/workspaces/ws1/members": () => json(roles("admin", OWNER.id)),
    [LIST_ROUTE]: () => json(LIST),
    [MEMBERS_ROUTE]: () => json(PAGE),
    ...extra,
  });
}

function mount() {
  return renderRoute(`/w/ws1/lists/${LIST_ID}`, "/w/:ws/lists/:listId", <ListDetailPage />);
}

describe("a list", () => {
  it("shows the list, its count and its members with their identifiers", async () => {
    asAdmin();
    mount();

    expect(await screen.findByRole("heading", { name: "Newsletter" })).toBeInTheDocument();
    expect(screen.getByText("2 people in this list.")).toBeInTheDocument();
    const table = await screen.findByRole("table", { name: "People in this list, newest first" });
    expect(within(table).getByText("ada@example.test")).toBeInTheDocument();
    expect(within(table).getByText("Email address")).toBeInTheDocument();
    expect(within(table).getByText("External ID (crm)")).toBeInTheDocument();
    expect(within(table).getByText("CSV import")).toBeInTheDocument();
    expect(within(screen.getByRole("navigation", { name: "Breadcrumb" })).getByRole("link", { name: "Lists" })).toHaveAttribute("href", "/w/ws1/lists");
    expect(screen.queryByText(/hidden from you/)).not.toBeInTheDocument();
  });

  it("hides identifier values from a member and says so", async () => {
    mockApi({
      "GET /api/v1/session": () => json(session(MEMBER)),
      "GET /api/v1/workspaces/ws1/members": () => json(roles("editor", MEMBER.id)),
      [LIST_ROUTE]: () => json(LIST),
      [MEMBERS_ROUTE]: () => json({ members: [member(ADA, "email", null)], next_cursor: null }),
    });
    mount();

    const table = await screen.findByRole("table", { name: "People in this list, newest first" });
    expect(within(table).getByText("Hidden")).toBeInTheDocument();
    expect(within(table).queryByText("ada@example.test")).not.toBeInTheDocument();
    expect(screen.getByText(/hidden without the personal data permission/)).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Import CSV" })).toBeInTheDocument();
  });

  it("is read-only for someone who cannot change anything", async () => {
    mockApi({
      "GET /api/v1/session": () => json(session(MEMBER)),
      "GET /api/v1/workspaces/ws1/members": () => json(roles("admin", "someone-else")),
      [LIST_ROUTE]: () => json(LIST),
      [MEMBERS_ROUTE]: () => json(PAGE),
    });
    mount();

    expect(await screen.findByText("You can look at this list but not change it.")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Import CSV" })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Add to list" })).not.toBeInTheDocument();
    expect(screen.queryByRole("checkbox")).not.toBeInTheDocument();
  });

  it("shows an empty state for a list with no one in it", async () => {
    asAdmin({ [MEMBERS_ROUTE]: () => json({ members: [], next_cursor: null }) });
    mount();

    expect(await screen.findByText("No one is in this list yet")).toBeInTheDocument();
  });

  it("shows a loading state while the members load", async () => {
    asAdmin({ [MEMBERS_ROUTE]: () => new Promise<Response>(() => undefined) });
    mount();

    expect(await screen.findByText("Loading members")).toBeInTheDocument();
  });

  it("shows an error with a retry when the members fail to load", async () => {
    let attempts = 0;
    asAdmin({
      [MEMBERS_ROUTE]: () => {
        attempts += 1;
        return attempts === 1 ? problem(500, "Internal Server Error", "internal error") : json(PAGE);
      },
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Try again" }));
    expect(await screen.findByText("ada@example.test")).toBeInTheDocument();
  });

  it("says the list does not exist, with no retry", async () => {
    asAdmin({ [LIST_ROUTE]: () => problem(404, "Not Found", "no such resource") });
    mount();

    expect(await screen.findByText("This list does not exist")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Try again" })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Import CSV" })).not.toBeInTheDocument();
  });

  it("loads the next page of members when asked", async () => {
    const calls = asAdmin({
      [MEMBERS_ROUTE]: (request) =>
        new URL(request.url).searchParams.get("cursor") === "cursor-1"
          ? json({ members: [member(BOB, "phone", "+4512345678")], next_cursor: null })
          : json({ members: [member(ADA, "email", "ada@example.test")], next_cursor: "cursor-1" }),
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Show more" }));

    expect(await screen.findByText("+4512345678")).toBeInTheDocument();
    expect(screen.getByText("ada@example.test")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Show more" })).not.toBeInTheDocument();
    const queries = calls.filter((call) => call.path.endsWith(`/lists/${LIST_ID}/members`)).map((call) => call.search);
    expect(queries).toEqual(["?limit=50", "?limit=50&cursor=cursor-1"]);
  });

  it("adds a person by identifier and says when no one has it", async () => {
    let known = false;
    const calls = asAdmin({
      [ADD_ROUTE]: () =>
        known
          ? json({ added: 1, already_member: 0, not_found: [] })
          : json({ added: 0, already_member: 0, not_found: [{ field: "identifiers", index: 0, reason: "not_found" }] }),
    });
    const user = userEvent.setup();
    mount();

    await screen.findByText("ada@example.test");
    await user.type(screen.getByLabelText("Identifier"), "nobody@example.test");
    await user.click(screen.getByRole("button", { name: "Add to list" }));
    expect(await screen.findByText(/No one has that identifier/)).toBeInTheDocument();
    known = true;
    await user.click(screen.getByRole("button", { name: "Add to list" }));

    expect(await screen.findByText("Added the person.")).toBeInTheDocument();
    expect(screen.getByLabelText("Identifier")).toHaveValue("");
    const posts = calls.filter((call) => call.method === "POST" && call.path.endsWith("/members"));
    expect(posts[0]?.body).toEqual({ identifiers: [{ kind: "email", value: "nobody@example.test" }] });
    expect(posts[0]?.headers.get("x-csrf-token")).toBe("csrf-detail");
  });

  it("adds by an external id with its system name", async () => {
    const calls = asAdmin({ [ADD_ROUTE]: () => json({ added: 0, already_member: 1, not_found: [] }) });
    const user = userEvent.setup();
    mount();

    await screen.findByText("ada@example.test");
    const add = screen.getByRole("button", { name: "Add to list" });
    await user.selectOptions(screen.getByLabelText("Identifier kind"), "external");
    await user.type(screen.getByLabelText("Identifier"), "42");
    expect(add).toBeDisabled();
    await user.type(screen.getByLabelText("System name"), "crm");
    await user.click(add);

    expect(await screen.findByText("That person is already in the list.")).toBeInTheDocument();
    const post = calls.find((call) => call.method === "POST");
    expect(post?.body).toEqual({ identifiers: [{ kind: "external:crm", value: "42" }] });
  });

  it("shows the server's reason when an identifier is refused", async () => {
    asAdmin({ [ADD_ROUTE]: () => problem(422, "Unprocessable Entity", "identifiers[0]: invalid_email") });
    const user = userEvent.setup();
    mount();

    await screen.findByText("ada@example.test");
    await user.type(screen.getByLabelText("Identifier"), "nope");
    await user.click(screen.getByRole("button", { name: "Add to list" }));

    expect(await screen.findByText(/invalid_email/)).toBeInTheDocument();
  });

  it("removes the people that are ticked", async () => {
    const calls = asAdmin({ [REMOVE_ROUTE]: () => json({ removed: 2, not_member: 0, not_found: [] }) });
    const user = userEvent.setup();
    mount();

    await screen.findByText("ada@example.test");
    expect(screen.queryByRole("button", { name: /^Remove/ })).not.toBeInTheDocument();
    await user.click(screen.getByRole("checkbox", { name: "Select ada@example.test" }));
    await user.click(screen.getByRole("checkbox", { name: "Select 42" }));
    await user.click(screen.getByRole("button", { name: "Remove 2 selected people" }));

    expect(await screen.findByText("Removed 2 people.")).toBeInTheDocument();
    const post = calls.find((call) => call.path.endsWith("/remove"));
    expect(post?.body).toEqual({ person_ids: [ADA, BOB] });
    expect(screen.queryByRole("button", { name: /^Remove/ })).not.toBeInTheDocument();
  });

  describe("removing more than 1000 loaded people", () => {
    const person = (n: number) => `p-${String(n).padStart(5, "0")}`;
    /** 21 pages of 50: the cursor of page k is `c<k>`. */
    function paged(): Parameters<typeof mockApi>[0] {
      return {
        [MEMBERS_ROUTE]: (request) => {
          const cursor = new URL(request.url).searchParams.get("cursor");
          const page = cursor === null ? 0 : Number(cursor.slice(1));
          const members = Array.from({ length: 50 }, (_, index) => member(person(page * 50 + index), "email", `m${page * 50 + index}@example.test`));
          return json({ members, next_cursor: page < 20 ? `c${page + 1}` : null });
        },
      };
    }

    async function selectEverything(user: ReturnType<typeof userEvent.setup>) {
      await screen.findByText("m0@example.test");
      for (let page = 1; page <= 20; page += 1) {
        await user.click(await screen.findByRole("button", { name: "Show more" }));
        await screen.findByText(`m${page * 50}@example.test`);
      }
      await user.click(screen.getByRole("checkbox", { name: "Select everyone on this page" }));
      await user.click(await screen.findByRole("button", { name: "Remove 1050 selected people" }));
    }

    it("sends the removals in chunks the server accepts and sums what was removed", async () => {
      const calls = asAdmin({
        ...paged(),
        [REMOVE_ROUTE]: async (request) => {
          const ids = ((await request.clone().json()) as { person_ids: string[] }).person_ids;
          return ids.length > 1000
            ? problem(422, "Unprocessable", "More than 1000 people")
            : json({ removed: ids.length, not_member: 0, not_found: [] });
        },
      });
      const user = userEvent.setup();
      mount();

      await selectEverything(user);

      expect(await screen.findByText("Removed 1050 people.", undefined, { timeout: 4000 })).toBeInTheDocument();
      const sent = calls.filter((call) => call.path.endsWith("/remove")).map((call) => (call.body as { person_ids: string[] }).person_ids);
      expect(sent.every((ids) => ids.length <= 1000)).toBe(true);
      expect(sent.flat().sort()).toEqual(Array.from({ length: 1050 }, (_, n) => person(n)));
    }, 30000);

    describe("at the chunk boundary", () => {
      /** Loads `total` people (pages of 50, the last one shorter), selects them all and removes them. */
      async function removeTotal(total: number) {
        const pages = Math.ceil(total / 50);
        const calls = asAdmin({
          [MEMBERS_ROUTE]: (request) => {
            const cursor = new URL(request.url).searchParams.get("cursor");
            const page = cursor === null ? 0 : Number(cursor.slice(1));
            const count = Math.min(50, total - page * 50);
            const members = Array.from({ length: count }, (_, index) => member(person(page * 50 + index), "email", `m${page * 50 + index}@example.test`));
            return json({ members, next_cursor: page < pages - 1 ? `c${page + 1}` : null });
          },
          [REMOVE_ROUTE]: async (request) => {
            const ids = ((await request.clone().json()) as { person_ids: string[] }).person_ids;
            return json({ removed: ids.length, not_member: 0, not_found: [] });
          },
        });
        const user = userEvent.setup();
        mount();
        await screen.findByText("m0@example.test");
        for (let page = 1; page < pages; page += 1) {
          await user.click(await screen.findByRole("button", { name: "Show more" }));
          await screen.findByText(`m${page * 50}@example.test`);
        }
        await user.click(screen.getByRole("checkbox", { name: "Select everyone on this page" }));
        await user.click(await screen.findByRole("button", { name: `Remove ${total} selected people` }));
        expect(await screen.findByText(`Removed ${total} people.`, undefined, { timeout: 4000 })).toBeInTheDocument();
        return calls.filter((call) => call.path.endsWith("/remove")).map((call) => (call.body as { person_ids: string[] }).person_ids.length);
      }

      it("makes exactly one call for exactly 1000 people", async () => {
        expect(await removeTotal(1000)).toEqual([1000]);
      }, 30000);

      it("makes two calls of 1000 and 1 for 1001 people", async () => {
        expect(await removeTotal(1001)).toEqual([1000, 1]);
      }, 30000);
    });

    it("shows what was removed and the error when a later chunk fails, then reloads", async () => {
      let removals = 0;
      const calls = asAdmin({
        ...paged(),
        [REMOVE_ROUTE]: async (request) => {
          removals += 1;
          const ids = ((await request.clone().json()) as { person_ids: string[] }).person_ids;
          return removals === 1 ? json({ removed: ids.length, not_member: 0, not_found: [] }) : problem(500, "Server error", "the store is down");
        },
      });
      const user = userEvent.setup();
      mount();

      await selectEverything(user);

      expect(await screen.findByText("Removed 1000 people.", undefined, { timeout: 4000 })).toBeInTheDocument();
      expect(screen.getByText(/the store is down/)).toBeInTheDocument();
      await waitFor(() => expect(calls.filter((call) => call.method === "GET" && call.path.endsWith(`/lists/${LIST_ID}/members`)).length).toBeGreaterThan(21));
    }, 30000);
    it("shows Removed 0 people and the error when a later chunk fails after a first chunk that removed none", async () => {
      let removals = 0;
      const calls = asAdmin({
        ...paged(),
        [REMOVE_ROUTE]: () => {
          removals += 1;
          return removals === 1 ? json({ removed: 0, not_member: 1000, not_found: [] }) : problem(500, "Server error", "the store is down");
        },
      });
      const user = userEvent.setup();
      mount();

      await selectEverything(user);

      expect(await screen.findByText("Removed 0 people.", undefined, { timeout: 4000 })).toBeInTheDocument();
      expect(screen.getByText(/the store is down/)).toBeInTheDocument();
      await waitFor(() => expect(calls.filter((call) => call.method === "GET" && call.path.endsWith(`/lists/${LIST_ID}/members`)).length).toBeGreaterThan(21));
    }, 30000);
  });

  it("drops a failed Show more error when the list reloads", async () => {
    asAdmin({
      [MEMBERS_ROUTE]: (request) =>
        new URL(request.url).searchParams.get("cursor") === "cursor-1"
          ? problem(500, "Internal Server Error", "STALEBOOM")
          : json({ members: [member(ADA, "email", "ada@example.test")], next_cursor: "cursor-1" }),
      [REMOVE_ROUTE]: () => json({ removed: 1, not_member: 0, not_found: [] }),
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Show more" }));
    expect(await screen.findByText(/STALEBOOM/)).toBeInTheDocument();
    await user.click(screen.getByRole("checkbox", { name: "Select ada@example.test" }));
    await user.click(screen.getByRole("button", { name: "Remove 1 selected person" }));

    expect(await screen.findByText("Removed 1 person.")).toBeInTheDocument();
    await waitFor(() => expect(screen.queryByText(/STALEBOOM/)).not.toBeInTheDocument());
  });

  it("selects everyone on the page at once, and clears again", async () => {
    asAdmin();
    const user = userEvent.setup();
    mount();

    await screen.findByText("ada@example.test");
    await user.click(screen.getByRole("checkbox", { name: "Select everyone on this page" }));
    expect(screen.getByRole("button", { name: "Remove 2 selected people" })).toBeInTheDocument();
    await user.click(screen.getByRole("checkbox", { name: "Select everyone on this page" }));
    expect(screen.queryByRole("button", { name: /^Remove/ })).not.toBeInTheDocument();
  });

  it("previews a CSV with a dry run, then adds on confirmation", async () => {
    const result = {
      matched: 3,
      added: 2,
      already_member: 1,
      unmatched: [
        { row: 5, reason: "not_found" },
        { row: 6, reason: "invalid_email" },
      ],
      unmatched_count: 2,
    };
    const bodies: string[] = [];
    const queries: string[] = [];
    const calls = asAdmin({
      [IMPORT_ROUTE]: async (request) => {
        bodies.push(await request.clone().text());
        queries.push(new URL(request.url).search);
        return json(result);
      },
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Import CSV" }));
    const dialog = within(screen.getByRole("dialog", { name: "Import people from a CSV" }));
    expect(dialog.getByRole("button", { name: "Check file" })).toBeDisabled();
    const csv = "kind,value\nemail,ada@example.test\n";
    await user.upload(dialog.getByLabelText("CSV file"), new File([csv], "people.csv", { type: "text/csv" }));
    await user.click(dialog.getByRole("button", { name: "Check file" }));

    expect(await dialog.findByText(/people.csv: 3 rows matched a person/)).toBeInTheDocument();
    expect(dialog.getByText(/2 people would be added/)).toBeInTheDocument();
    expect(dialog.getByText("Row 5: no person has this identifier")).toBeInTheDocument();
    expect(dialog.getByText("Row 6: not a valid email address")).toBeInTheDocument();
    expect(queries).toEqual(["?dry_run=true"]);
    await user.click(dialog.getByRole("button", { name: "Add 2 people" }));

    expect(await screen.findByText(/Added 2 people\. 1 already in the list, 2 skipped\./)).toBeInTheDocument();
    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
    expect(queries).toEqual(["?dry_run=true", "?dry_run=false"]);
    expect(bodies).toEqual([csv, csv]);
    const imports = calls.filter((call) => call.path.endsWith("/import"));
    expect(imports[0]?.headers.get("content-type")).toBe("text/csv");
    expect(imports[0]?.headers.get("x-csrf-token")).toBe("csrf-detail");
  });

  it("shows why a file was refused and keeps the dialog open", async () => {
    asAdmin({ [IMPORT_ROUTE]: () => problem(422, "Unprocessable Entity", "body: the first row must be the header kind,value") });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Import CSV" }));
    const dialog = within(screen.getByRole("dialog", { name: "Import people from a CSV" }));
    await user.upload(dialog.getByLabelText("CSV file"), new File(["email,x\n"], "bad.csv", { type: "text/csv" }));
    await user.click(dialog.getByRole("button", { name: "Check file" }));

    expect(await dialog.findByText(/the first row must be the header/)).toBeInTheDocument();
    expect(dialog.queryByRole("button", { name: /^Add \d/ })).not.toBeInTheDocument();
  });

  it("cuts a long list of skipped rows short", async () => {
    const unmatched = Array.from({ length: 1000 }, (_, index) => ({ row: index + 2, reason: "not_found" }));
    asAdmin({
      [IMPORT_ROUTE]: () => json({ matched: 0, added: 0, already_member: 0, unmatched, unmatched_count: 1025 }),
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Import CSV" }));
    const dialog = within(screen.getByRole("dialog"));
    await user.upload(dialog.getByLabelText("CSV file"), new File(["kind,value\n"], "x.csv", { type: "text/csv" }));
    await user.click(dialog.getByRole("button", { name: "Check file" }));

    expect(await dialog.findByText("and 1005 more.")).toBeInTheDocument();
    expect(dialog.getAllByText(/^Row \d+:/)).toHaveLength(20);
    expect(dialog.getByText("1025 rows will be skipped:")).toBeInTheDocument();
    expect(dialog.getByRole("button", { name: "Add 0 people" })).toBeDisabled();
  });
});

function slowFile(name: string) {
  const file = new File(["placeholder"], name, { type: "text/csv" });
  let resolve: (text: string) => void = () => undefined;
  const text = new Promise<string>((done) => {
    resolve = done;
  });
  Object.defineProperty(file, "text", { value: () => text });
  return { file, resolve };
}

describe("choosing another CSV file", () => {
  async function openDialog(user: ReturnType<typeof userEvent.setup>) {
    await user.click(await screen.findByRole("button", { name: "Import CSV" }));
    return within(screen.getByRole("dialog", { name: "Import people from a CSV" }));
  }
  const A_CSV = "kind,value\nemail,a@example.test\n";
  const B_CSV = "kind,value\nemail,b@example.test\n";
  const tick = () => new Promise((done) => setTimeout(done, 30));

  it("cannot check while the new file is still being read", async () => {
    asAdmin({ [IMPORT_ROUTE]: () => json({ matched: 1, added: 1, already_member: 0, unmatched: [], unmatched_count: 0 }) });
    const user = userEvent.setup();
    mount();
    const dialog = await openDialog(user);

    await user.upload(dialog.getByLabelText("CSV file"), new File([A_CSV], "A.csv", { type: "text/csv" }));
    await waitFor(() => expect(dialog.getByRole("button", { name: "Check file" })).toBeEnabled());
    const b = slowFile("B.csv");
    await user.upload(dialog.getByLabelText("CSV file"), b.file);

    expect(dialog.getByRole("button", { name: "Check file" })).toBeDisabled();
    b.resolve(B_CSV);
    await waitFor(() => expect(dialog.getByRole("button", { name: "Check file" })).toBeEnabled());
  });

  it("sends the last file chosen even when an earlier read finishes later", async () => {
    const bodies: string[] = [];
    asAdmin({
      [IMPORT_ROUTE]: async (request) => {
        bodies.push(await request.clone().text());
        return json({ matched: 1, added: 1, already_member: 0, unmatched: [], unmatched_count: 0 });
      },
    });
    const user = userEvent.setup();
    mount();
    const dialog = await openDialog(user);

    const a = slowFile("A.csv");
    await user.upload(dialog.getByLabelText("CSV file"), a.file);
    await user.upload(dialog.getByLabelText("CSV file"), new File([B_CSV], "B.csv", { type: "text/csv" }));
    await tick();
    a.resolve(A_CSV);
    await tick();
    await user.click(dialog.getByRole("button", { name: "Check file" }));

    expect(await dialog.findByText(/B.csv: 1 row matched a person/)).toBeInTheDocument();
    expect(bodies).toHaveLength(1);
    expect(bodies[0]).toContain("b@example.test");
    expect(bodies[0]).not.toContain("a@example.test");
  });

  it("still applies a committed import when another file is chosen while it runs", async () => {
    let answer: () => void = () => undefined;
    const held = new Promise<void>((done) => {
      answer = done;
    });
    asAdmin({
      [IMPORT_ROUTE]: async (request) => {
        const dry = new URL(request.url).searchParams.get("dry_run") === "true";
        if (dry) return json({ matched: 1, added: 1, already_member: 0, unmatched: [], unmatched_count: 0 });
        await held;
        return json({ matched: 1, added: 1, already_member: 0, unmatched: [], unmatched_count: 0 });
      },
    });
    const user = userEvent.setup();
    mount();
    const dialog = await openDialog(user);

    await user.upload(dialog.getByLabelText("CSV file"), new File([A_CSV], "A.csv", { type: "text/csv" }));
    await user.click(dialog.getByRole("button", { name: "Check file" }));
    await user.click(await dialog.findByRole("button", { name: "Add 1 person" }));
    await user.upload(dialog.getByLabelText("CSV file"), new File([B_CSV], "B.csv", { type: "text/csv" }));
    answer();

    expect(await screen.findByText(/Added 1 person\./)).toBeInTheDocument();
    await waitFor(() => expect(screen.queryByRole("dialog", { name: "Import people from a CSV" })).not.toBeInTheDocument());
  });

  it("shows the error of a committed import that fails after another file is chosen", async () => {
    let answer: () => void = () => undefined;
    const held = new Promise<void>((done) => {
      answer = done;
    });
    asAdmin({
      [IMPORT_ROUTE]: async (request) => {
        const dry = new URL(request.url).searchParams.get("dry_run") === "true";
        if (dry) return json({ matched: 1, added: 1, already_member: 0, unmatched: [], unmatched_count: 0 });
        await held;
        return problem(500, "Server error", "COMMITBOOM");
      },
    });
    const user = userEvent.setup();
    mount();
    const dialog = await openDialog(user);

    await user.upload(dialog.getByLabelText("CSV file"), new File([A_CSV], "A.csv", { type: "text/csv" }));
    await user.click(dialog.getByRole("button", { name: "Check file" }));
    await user.click(await dialog.findByRole("button", { name: "Add 1 person" }));
    await user.upload(dialog.getByLabelText("CSV file"), new File([B_CSV], "B.csv", { type: "text/csv" }));
    answer();

    expect(await dialog.findByText(/COMMITBOOM/)).toBeInTheDocument();
  });

  it("drops the preview of a file that was replaced while it was being checked", async () => {
    let answer: () => void = () => undefined;
    const held = new Promise<void>((done) => {
      answer = done;
    });
    asAdmin({
      [IMPORT_ROUTE]: async () => {
        await held;
        return json({ matched: 1, added: 1, already_member: 0, unmatched: [], unmatched_count: 0 });
      },
    });
    const user = userEvent.setup();
    mount();
    const dialog = await openDialog(user);

    await user.upload(dialog.getByLabelText("CSV file"), new File([A_CSV], "A.csv", { type: "text/csv" }));
    await user.click(dialog.getByRole("button", { name: "Check file" }));
    await user.upload(dialog.getByLabelText("CSV file"), new File([B_CSV], "B.csv", { type: "text/csv" }));
    answer();
    await tick();

    expect(dialog.queryByText(/matched a person/)).not.toBeInTheDocument();
    expect(dialog.queryByText(/A\.csv/)).not.toBeInTheDocument();
  });
});

describe("import reasons", () => {
  it("name every code the server sends in plain words", () => {
    for (const code of [
      "not_found",
      "malformed_row",
      "unknown_identifier_kind",
      "invalid_identifier_namespace",
      "invalid_email",
      "invalid_phone",
      "invalid_identifier_value",
    ]) {
      expect(reasonText(code)).not.toBe(code);
    }
    expect(reasonText("something_new")).toBe("something_new");
  });
});
