import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { json, MEMBER, mockApi, OWNER, problem, session } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { SuppressionsPage } from "./SuppressionsPage";

const MANUAL = {
  hash_prefix: "9f2a41c7",
  kind: "email",
  reason: "manual",
  source: "ui",
  note: "Asked to stop",
  created_at: "2026-10-01T09:00:00Z",
};
const ERASED = { ...MANUAL, hash_prefix: "00ab12cd", reason: "erased", source: "erasure", note: null };

const BASE = "/api/v1/workspaces/ws1/suppressions";
const ROUTE = "/w/:ws/suppressions";

function members(role: "admin" | "member", userId: string) {
  return [{ user_id: userId, email: "someone@example.test", role, added_at: "2026-10-01T09:00:00Z" }];
}

function adminRoutes(extra: Parameters<typeof mockApi>[0] = {}) {
  return {
    "GET /api/v1/session": () => json(session(OWNER, "csrf-sup")),
    "GET /api/v1/workspaces/ws1/members": () => json(members("admin", OWNER.id)),
    ...extra,
  };
}

describe("suppressions", () => {
  it("lists hash prefixes with reason, source and note, and no address", async () => {
    mockApi(
      adminRoutes({
        [`GET ${BASE}`]: () => json({ suppressions: [MANUAL, ERASED], next_before: null }),
      }),
    );
    renderRoute("/w/ws1/suppressions", ROUTE, <SuppressionsPage />);

    const table = await screen.findByRole("table", { name: "Suppressed addresses and numbers" });
    expect(within(table).getByText(/9f2a41c7/)).toBeInTheDocument();
    expect(within(table).getByText("Asked to stop")).toBeInTheDocument();
    expect(within(table).getByText("Added manually")).toBeInTheDocument();
    expect(within(table).getByText("Erased")).toBeInTheDocument();
    expect(within(table).queryByText(/@/)).not.toBeInTheDocument();
  });

  it("shows an empty state, and a retry when loading fails", async () => {
    mockApi(adminRoutes({ [`GET ${BASE}`]: () => json({ suppressions: [], next_before: null }) }));
    const first = renderRoute("/w/ws1/suppressions", ROUTE, <SuppressionsPage />);
    expect(await screen.findByText("Nothing is suppressed")).toBeInTheDocument();
    first.unmount();

    mockApi(adminRoutes({ [`GET ${BASE}`]: () => problem(500, "Internal Server Error", "internal error") }));
    renderRoute("/w/ws1/suppressions", ROUTE, <SuppressionsPage />);
    expect(await screen.findByRole("button", { name: /try again/i })).toBeInTheDocument();
  });

  it("filters by reason and loads more pages", async () => {
    const calls = mockApi(
      adminRoutes({
        [`GET ${BASE}`]: (request) => {
          const query = new URL(request.url).searchParams;
          if (query.get("reason") === "erased") {
            return json({ suppressions: [ERASED], next_before: null });
          }
          if (query.get("before") === "cursor-1") {
            return json({ suppressions: [ERASED], next_before: null });
          }
          return json({ suppressions: [MANUAL], next_before: "cursor-1" });
        },
      }),
    );
    const user = userEvent.setup();
    renderRoute("/w/ws1/suppressions", ROUTE, <SuppressionsPage />);

    await user.click(await screen.findByRole("button", { name: "Show more" }));
    expect(await screen.findByText(/00ab12cd/)).toBeInTheDocument();
    expect(screen.getByText(/9f2a41c7/)).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Show more" })).not.toBeInTheDocument();

    await user.selectOptions(screen.getByLabelText("Show"), "erased");

    await waitFor(() => expect(screen.queryByText(/9f2a41c7/)).not.toBeInTheDocument());
    expect(calls.some((call) => call.search.includes("reason=erased"))).toBe(true);
  });

  it("is read-only for a member, who can still check an address", async () => {
    const calls = mockApi({
      "GET /api/v1/session": () => json(session(MEMBER)),
      "GET /api/v1/workspaces/ws1/members": () => json(members("member", MEMBER.id)),
      [`GET ${BASE}`]: () => json({ suppressions: [MANUAL], next_before: null }),
      [`POST ${BASE}/check`]: () => json({ suppressed: true, reason: "manual" }),
    });
    const user = userEvent.setup();
    renderRoute("/w/ws1/suppressions", ROUTE, <SuppressionsPage />);

    expect(await screen.findByText(/9f2a41c7/)).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Suppress an address" })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Import CSV" })).not.toBeInTheDocument();
    expect(screen.queryByRole("link", { name: "Export CSV" })).not.toBeInTheDocument();
    expect(screen.getByText(/Only workspace admins and the instance owner can add, import, export or remove/)).toBeInTheDocument();

    await user.type(screen.getByLabelText("Address or number"), "ada@example.com");
    await user.click(screen.getByRole("button", { name: "Check" }));

    expect(await screen.findByText("Suppressed: Added manually")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Remove suppression" })).not.toBeInTheDocument();
    expect(calls.find((call) => call.path.endsWith("/check"))?.body).toEqual({ kind: "email", value: "ada@example.com" });
  });

  it("checks an address, then removes the suppression after confirmation", async () => {
    let suppressed = true;
    const calls = mockApi(
      adminRoutes({
        [`GET ${BASE}`]: () => json({ suppressions: suppressed ? [MANUAL] : [], next_before: null }),
        [`POST ${BASE}/check`]: () => json({ suppressed, reason: suppressed ? "manual" : null }),
        [`POST ${BASE}/remove`]: () => {
          suppressed = false;
          return new Response(null, { status: 204 });
        },
      }),
    );
    const user = userEvent.setup();
    renderRoute("/w/ws1/suppressions", ROUTE, <SuppressionsPage />);

    await user.type(await screen.findByLabelText("Address or number"), "ada@example.com");
    await user.click(screen.getByRole("button", { name: "Check" }));
    await user.click(await screen.findByRole("button", { name: "Remove suppression" }));
    const confirm = within(screen.getByRole("dialog", { name: "Remove this suppression?" }));
    await user.click(confirm.getByRole("button", { name: "Remove suppression" }));

    expect(await screen.findByText(/The suppression was removed/)).toBeInTheDocument();
    expect(screen.getByText("Nothing is suppressed")).toBeInTheDocument();
    const remove = calls.find((call) => call.path.endsWith("/remove"));
    expect(remove?.body).toEqual({ kind: "email", value: "ada@example.com" });
    expect(remove?.headers.get("x-csrf-token")).toBe("csrf-sup");
  });

  it("offers no removal for an erased suppression", async () => {
    mockApi(
      adminRoutes({
        [`GET ${BASE}`]: () => json({ suppressions: [], next_before: null }),
        [`POST ${BASE}/check`]: () => json({ suppressed: true, reason: "erased" }),
      }),
    );
    const user = userEvent.setup();
    renderRoute("/w/ws1/suppressions", ROUTE, <SuppressionsPage />);

    await user.type(await screen.findByLabelText("Address or number"), "ada@example.com");
    await user.click(screen.getByRole("button", { name: "Check" }));

    expect(await screen.findByText("Suppressed: Erased")).toBeInTheDocument();
    expect(screen.getByText(/permanent and cannot be removed/)).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Remove suppression" })).not.toBeInTheDocument();
  });

  it("says so when an address is not suppressed and drops the answer when the input changes", async () => {
    mockApi(
      adminRoutes({
        [`GET ${BASE}`]: () => json({ suppressions: [], next_before: null }),
        [`POST ${BASE}/check`]: () => json({ suppressed: false, reason: null }),
      }),
    );
    const user = userEvent.setup();
    renderRoute("/w/ws1/suppressions", ROUTE, <SuppressionsPage />);

    await user.type(await screen.findByLabelText("Address or number"), "bob@example.com");
    await user.click(screen.getByRole("button", { name: "Check" }));
    expect(await screen.findByText("Not suppressed")).toBeInTheDocument();
    await user.type(screen.getByLabelText("Address or number"), "x");

    expect(screen.queryByText("Not suppressed")).not.toBeInTheDocument();
  });

  it("suppresses an address and shows the server's reason when it is refused", async () => {
    let attempts = 0;
    const calls = mockApi(
      adminRoutes({
        [`GET ${BASE}`]: () => json({ suppressions: [], next_before: null }),
        [`POST ${BASE}`]: () => {
          attempts += 1;
          return attempts === 1 ? problem(422, "Unprocessable Entity", "value: invalid_email") : json({ added: true });
        },
      }),
    );
    const user = userEvent.setup();
    renderRoute("/w/ws1/suppressions", ROUTE, <SuppressionsPage />);

    await user.click(await screen.findByRole("button", { name: "Suppress an address" }));
    const form = within(screen.getByRole("dialog", { name: "Suppress an address" }));
    expect(form.getByRole("button", { name: "Suppress" })).toBeDisabled();
    await user.type(form.getByLabelText("Address or number"), "nope");
    await user.click(form.getByRole("button", { name: "Suppress" }));
    expect(await form.findByText(/invalid_email/)).toBeInTheDocument();
    await user.clear(form.getByLabelText("Address or number"));
    await user.type(form.getByLabelText("Address or number"), "ada@example.com");
    await user.type(form.getByLabelText("Note"), "Asked to stop");
    await user.click(form.getByRole("button", { name: "Suppress" }));

    expect(await screen.findByText(/Suppressed\. Nothing is sent/)).toBeInTheDocument();
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    const posts = calls.filter((call) => call.method === "POST").map((call) => call.body);
    expect(posts[1]).toEqual({ kind: "email", value: "ada@example.com", note: "Asked to stop" });
    expect(calls.find((call) => call.method === "POST")?.headers.get("x-csrf-token")).toBe("csrf-sup");
  });

  it("previews an import with a dry run, then commits it", async () => {
    const csv = "kind,value\nemail,a@example.com\nemail,oops\n";
    const calls = mockApi(
      adminRoutes({
        [`GET ${BASE}`]: () => json({ suppressions: [], next_before: null }),
        [`POST ${BASE}/import`]: (request) => {
          const dryRun = new URL(request.url).searchParams.get("dry_run") === "true";
          return json({ dry_run: dryRun, added: 1, existing: 2, invalid: [{ row: 3, reason: "invalid_email" }] });
        },
      }),
    );
    const user = userEvent.setup();
    renderRoute("/w/ws1/suppressions", ROUTE, <SuppressionsPage />);

    await user.click(await screen.findByRole("button", { name: "Import CSV" }));
    const form = within(screen.getByRole("dialog", { name: "Import suppressions" }));
    expect(form.getByRole("button", { name: "Preview" })).toBeDisabled();
    expect(form.getByRole("button", { name: "Import" })).toBeDisabled();
    await user.click(form.getByLabelText("Or paste the CSV"));
    await user.paste(csv);
    await user.click(form.getByRole("button", { name: "Preview" }));

    expect(await form.findByText(/1 address would be suppressed, 2 already suppressed/)).toBeInTheDocument();
    expect(form.getByText(/Row 3: Not a valid email address/)).toBeInTheDocument();
    expect(calls.filter((call) => call.path.endsWith("/import"))).toHaveLength(1);
    await user.click(form.getByRole("button", { name: "Import" }));

    expect(await screen.findByText(/Imported: 1 address suppressed, 2 already suppressed, 1 row skipped/)).toBeInTheDocument();
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    const imports = calls.filter((call) => call.path.endsWith("/import"));
    expect(imports.map((call) => call.search)).toEqual(["?dry_run=true", "?dry_run=false"]);
    expect(imports[1]?.body).toBe(csv);
    expect(imports[1]?.headers.get("content-type")).toBe("text/csv");
    expect(imports[1]?.headers.get("x-csrf-token")).toBe("csrf-sup");
  });

  it("asks for a new preview when the text changes after one", async () => {
    mockApi(
      adminRoutes({
        [`GET ${BASE}`]: () => json({ suppressions: [], next_before: null }),
        [`POST ${BASE}/import`]: () => json({ dry_run: true, added: 1, existing: 0, invalid: [] }),
      }),
    );
    const user = userEvent.setup();
    renderRoute("/w/ws1/suppressions", ROUTE, <SuppressionsPage />);

    await user.click(await screen.findByRole("button", { name: "Import CSV" }));
    const form = within(screen.getByRole("dialog", { name: "Import suppressions" }));
    await user.click(form.getByLabelText("Or paste the CSV"));
    await user.paste("kind,value\nemail,a@example.com\n");
    await user.click(form.getByRole("button", { name: "Preview" }));
    await form.findByText(/1 address would be suppressed/);
    expect(form.getByRole("button", { name: "Import" })).toBeEnabled();
    await user.type(form.getByLabelText("Or paste the CSV"), "x");

    expect(form.queryByText(/would be suppressed/)).not.toBeInTheDocument();
    expect(form.getByRole("button", { name: "Import" })).toBeDisabled();
  });

  it("shows why an import was refused", async () => {
    mockApi(
      adminRoutes({
        [`GET ${BASE}`]: () => json({ suppressions: [], next_before: null }),
        [`POST ${BASE}/import`]: () => problem(422, "Unprocessable Entity", "header: must be kind,value or kind,value,note"),
      }),
    );
    const user = userEvent.setup();
    renderRoute("/w/ws1/suppressions", ROUTE, <SuppressionsPage />);

    await user.click(await screen.findByRole("button", { name: "Import CSV" }));
    const form = within(screen.getByRole("dialog", { name: "Import suppressions" }));
    await user.click(form.getByLabelText("Or paste the CSV"));
    await user.paste("a,b\n");
    await user.click(form.getByRole("button", { name: "Preview" }));

    expect(await form.findByText(/header: must be kind,value/)).toBeInTheDocument();
    expect(form.getByRole("button", { name: "Import" })).toBeDisabled();
  });

  it("links the export to the server's CSV", async () => {
    mockApi(adminRoutes({ [`GET ${BASE}`]: () => json({ suppressions: [], next_before: null }) }));
    renderRoute("/w/ws1/suppressions", ROUTE, <SuppressionsPage />);

    const link = await screen.findByRole("link", { name: "Export CSV" });

    expect(link).toHaveAttribute("href", `${BASE}/export`);
    expect(link).toHaveAttribute("download", "suppressions.csv");
  });
});
