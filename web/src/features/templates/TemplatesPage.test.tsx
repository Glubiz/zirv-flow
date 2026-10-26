import { fireEvent, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { json, mockApi, problem, type Call, type Handler } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { listItem, templateDetail, templateRoutes, TEMPLATE_ID, WS } from "../../test/templatesApi";
import { themeBody, tokens } from "../../test/themesApi";
import { statusKey, type TemplateListItem } from "./library";
import { TemplatePage } from "./TemplatePage";
import { TemplatesPage } from "./TemplatesPage";

const LIVE = listItem({ id: "t-live", name: "Spring sale", current_version: 2, checks: { blocking: 0, warnings: 2 }, active_flows: 2, edited_by_email: "ana@example.test", edited_at: "2026-10-09T09:00:00Z" });
const CHANGED = listItem({ id: "t-changed", name: "Order shipped", current_version: 1, draft_changed: true, checks: { blocking: 1, warnings: 0 } });
const FRESH = listItem({ id: "t-fresh", name: "Welcome", category: "transactional" });
const HOOK = listItem({ id: "t-hook", name: "Order hook", channel: "webhook", category: "transactional" });
const GONE = listItem({ id: "t-gone", name: "Old promo", current_version: 1, archived_at: "2026-10-02T09:00:00Z" });
const ALL = [LIVE, CHANGED, FRESH, HOOK, GONE];

interface Options {
  role?: "admin" | "member" | "reader";
  items?: Record<string, unknown>[];
  extra?: Record<string, Handler>;
  themes?: unknown[];
}

/** The routes of the page: the list answers the filters it is asked for, and the unfiltered call everything. */
function libraryRoutes({ role = "admin", items = ALL, extra = {}, themes = [] }: Options = {}): Record<string, Handler> {
  return {
    ...templateRoutes({ role }),
    [`GET ${WS}/themes`]: () => json(themes),
    [`GET ${WS}/templates`]: (request) => {
      const query = new URL(request.url).searchParams;
      const status = query.get("status");
      const term = (query.get("q") ?? "").toLowerCase();
      const shown = items.filter((item) => {
        const entry = item as unknown as TemplateListItem;
        const kind = statusKey(entry);
        if (status ? kind !== status : kind === "archived" && query.get("archived") !== "true") return false;
        if (query.get("channel") && entry.channel !== query.get("channel")) return false;
        return entry.name.toLowerCase().includes(term);
      });
      return json(shown);
    },
    ...extra,
  };
}

function mount(url = "/w/ws1/templates") {
  return renderRoute(url, "/w/:ws/templates", <TemplatesPage />);
}

function listCalls(calls: Call[]): URLSearchParams[] {
  return calls.filter((call) => call.method === "GET" && call.path === `${WS}/templates`).map((call) => new URLSearchParams(call.search));
}

describe("the template library", () => {
  it("shows each template as a card with its status, checks, flows and edit time", async () => {
    mockApi(libraryRoutes());
    mount();

    const cards = await screen.findByRole("list", { name: "Templates of this workspace" });
    const spring = within(cards).getByRole("link", { name: "Spring sale" }).closest("li");
    expect(spring).not.toBeNull();
    expect(within(spring as HTMLElement).getByText("Live")).toBeInTheDocument();
    expect(within(spring as HTMLElement).getByText("2 warnings")).toBeInTheDocument();
    expect(within(spring as HTMLElement).getByText("Used in 2 flows")).toBeInTheDocument();
    expect(within(spring as HTMLElement).getByText("Email · Marketing")).toBeInTheDocument();
    const changed = within(cards).getByRole("link", { name: "Order shipped" }).closest("li") as HTMLElement;
    expect(within(changed).getByText("Draft changes")).toBeInTheDocument();
    expect(within(changed).getByText("1 blocking issue")).toBeInTheDocument();
    const fresh = within(cards).getByRole("link", { name: "Welcome" }).closest("li") as HTMLElement;
    expect(within(fresh).getByText("Draft")).toBeInTheDocument();
    expect(within(fresh).getByText("Not checked")).toBeInTheDocument();
    expect(within(cards).getByText("Webhook")).toBeInTheDocument();
    expect(within(cards).queryByText("Old promo")).not.toBeInTheDocument();
  });

  it("shows the list view with its columns, from the URL", async () => {
    mockApi(libraryRoutes({ themes: [{ ...themeBody({ id: "th1", name: "Warm", is_default: true }), tokens: tokens(), used_by_templates: 0 }] }));
    mount("/w/ws1/templates?view=list");

    const table = await screen.findByRole("table", { name: "Templates of this workspace" });
    const headers = within(table).getAllByRole("columnheader").map((cell) => cell.textContent);
    expect(headers.slice(0, 6)).toEqual(["Name", "Status", "Checks", "Used in", "Theme", "Edited"]);
    const row = within(table).getByRole("link", { name: "Spring sale" }).closest("tr") as HTMLElement;
    expect(within(row).getByText("2 flows")).toBeInTheDocument();
    expect(within(row).getByText("No theme")).toBeInTheDocument();
    expect(within(row).getByText("ana@example.test")).toBeInTheDocument();
  });

  it("counts the status chips from one unfiltered call", async () => {
    const calls = mockApi(libraryRoutes());
    mount();

    expect(await screen.findByRole("button", { name: "All 4" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Live 1" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Draft changes 1" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Draft 2" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Archived 1" })).toBeInTheDocument();
    expect(listCalls(calls).filter((query) => query.get("archived") === "true")).toHaveLength(1);
  });

  it("filters by status, channel and sort, and keeps them in the URL", async () => {
    const calls = mockApi(libraryRoutes());
    const user = userEvent.setup();
    const { router } = mount();
    await screen.findByRole("list", { name: "Templates of this workspace" });

    await user.click(screen.getByRole("button", { name: "Draft changes 1" }));
    await waitFor(() => expect(listCalls(calls).some((query) => query.get("status") === "changes")).toBe(true));
    expect(await screen.findByRole("link", { name: "Order shipped" })).toBeInTheDocument();
    expect(screen.queryByRole("link", { name: "Spring sale" })).not.toBeInTheDocument();

    await user.selectOptions(screen.getByLabelText("Channel"), "webhook");
    await user.selectOptions(screen.getByLabelText("Sort"), "most_used");
    await user.click(screen.getByRole("button", { name: "List" }));

    await waitFor(() => expect(router.state.location.search).toBe("?status=changes&channel=webhook&sort=most_used&view=list"));
    await waitFor(() => expect(listCalls(calls).some((query) => query.get("channel") === "webhook" && query.get("sort") === "most_used")).toBe(true));
  });

  it("reads the filters back from the URL after a reload", async () => {
    const calls = mockApi(libraryRoutes());
    mount("/w/ws1/templates?q=ship&status=changes&sort=name");

    expect(await screen.findByRole("link", { name: "Order shipped" })).toBeInTheDocument();
    expect(screen.getByRole("searchbox", { name: "Search templates" })).toHaveValue("ship");
    expect(screen.getByRole("button", { name: "Draft changes 1" })).toHaveAttribute("aria-pressed", "true");
    expect(screen.getByLabelText("Sort")).toHaveValue("name");
    const query = listCalls(calls).find((entry) => entry.get("q") !== null);
    expect(query?.get("status")).toBe("changes");
    expect(query?.get("sort")).toBe("name");
  });

  it("waits for a pause in typing before it searches", async () => {
    const calls = mockApi(libraryRoutes());
    const { router } = mount();
    await screen.findByRole("list", { name: "Templates of this workspace" });
    await screen.findByRole("button", { name: "All 4" });
    const before = listCalls(calls).length;
    const box = screen.getByRole("searchbox", { name: "Search templates" });

    fireEvent.change(box, { target: { value: "sa" } });
    fireEvent.change(box, { target: { value: "sale" } });

    expect(listCalls(calls)).toHaveLength(before);
    await waitFor(() => expect(router.state.location.search).toBe("?q=sale"));
    expect(listCalls(calls).filter((query) => query.get("q") === "sale")).toHaveLength(1);
    expect(await screen.findByRole("link", { name: "Spring sale" })).toBeInTheDocument();
    expect(screen.queryByRole("link", { name: "Welcome" })).not.toBeInTheDocument();
  });

  it("filters by theme", async () => {
    const calls = mockApi(libraryRoutes({ themes: [{ ...themeBody({ id: "th1", name: "Warm" }), tokens: tokens(), used_by_templates: 0 }] }));
    const user = userEvent.setup();
    mount();
    await screen.findByRole("list", { name: "Templates of this workspace" });

    await user.selectOptions(await screen.findByRole("option", { name: "Warm" }).then(() => screen.getByLabelText("Theme")), "th1");

    await waitFor(() => expect(listCalls(calls).some((query) => query.get("theme") === "th1")).toBe(true));
  });

  it("says so when the library is empty, with a way to make a template", async () => {
    mockApi(libraryRoutes({ items: [] }));
    mount();

    expect(await screen.findByText("No templates yet")).toBeInTheDocument();
    expect(screen.getAllByRole("button", { name: "New template" }).length).toBeGreaterThan(0);
  });

  it("says nothing matches and clears the search and filters", async () => {
    const calls = mockApi(libraryRoutes());
    const user = userEvent.setup();
    const { router } = mount("/w/ws1/templates?q=zzz&status=live");

    expect(await screen.findByText("No templates match")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Clear search and filters" }));

    expect(await screen.findByRole("link", { name: "Spring sale" })).toBeInTheDocument();
    expect(router.state.location.search).toBe("");
    expect(screen.getByRole("searchbox", { name: "Search templates" })).toHaveValue("");
    expect(listCalls(calls).at(-1)?.get("q")).toBeNull();
  });

  it("shows an error with a retry that loads again", async () => {
    let attempts = 0;
    const routes = libraryRoutes();
    mockApi({
      ...routes,
      [`GET ${WS}/templates`]: (request) => {
        const query = new URL(request.url).searchParams;
        if (query.get("archived") === "true") return json(ALL);
        attempts += 1;
        return attempts === 1 ? problem(500, "Internal Server Error", "internal error") : json([LIVE]);
      },
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Try again" }));

    expect(await screen.findByRole("link", { name: "Spring sale" })).toBeInTheDocument();
  });

  it("offers a reader no way to make, copy or archive a template", async () => {
    mockApi(libraryRoutes({ role: "reader" }));
    const user = userEvent.setup();
    mount();

    await screen.findByRole("list", { name: "Templates of this workspace" });
    expect(screen.queryByRole("button", { name: "New template" })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Import HTML or MJML" })).not.toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Actions for Order shipped" }));
    expect(screen.getByRole("menuitem", { name: "Open" })).toBeInTheDocument();
    expect(screen.queryByRole("menuitem", { name: "Duplicate" })).not.toBeInTheDocument();
    expect(screen.queryByRole("menuitem", { name: /Archive/ })).not.toBeInTheDocument();
  });

  it("offers a member who is not an admin Duplicate but not Archive", async () => {
    mockApi(libraryRoutes({ role: "member" }));
    const user = userEvent.setup();
    mount();

    await screen.findByRole("list", { name: "Templates of this workspace" });
    await user.click(screen.getByRole("button", { name: "Actions for Order shipped" }));
    expect(screen.getByRole("menuitem", { name: "Duplicate" })).toBeInTheDocument();
    expect(screen.queryByRole("menuitem", { name: /Archive/ })).not.toBeInTheDocument();
  });
});

describe("a template's menu", () => {
  const COPY_SOURCE = templateDetail({ id: "t-changed", name: "Order shipped", current_version: 1 });

  function copyRoutes(created: (body: Record<string, unknown>) => Response) {
    return libraryRoutes({
      extra: {
        [`GET ${WS}/templates/t-changed`]: () => json(COPY_SOURCE),
        [`POST ${WS}/templates`]: async (request) => created((await request.json()) as Record<string, unknown>),
      },
    });
  }

  function posts(calls: Call[]) {
    return calls.filter((call) => call.method === "POST" && call.path === `${WS}/templates`).map((call) => call.body as Record<string, unknown>);
  }

  it("duplicates a template as Copy of its name with its document, channel and category", async () => {
    const calls = mockApi(copyRoutes(() => json(templateDetail({ id: "t-copy" }), 201)));
    const user = userEvent.setup();
    const { router } = mount();
    await screen.findByRole("list", { name: "Templates of this workspace" });

    await user.click(screen.getByRole("button", { name: "Actions for Order shipped" }));
    await user.click(screen.getByRole("menuitem", { name: "Duplicate" }));

    await waitFor(() => expect(router.state.location.pathname).toBe("/w/ws1/templates/t-copy"));
    expect(posts(calls)).toHaveLength(1);
    expect(posts(calls)[0]).toMatchObject({
      name: "Copy of Order shipped",
      channel: "email",
      category: "marketing",
      document: COPY_SOURCE.draft.document,
    });
  });

  it("adds a number to the name when the copy's name is taken", async () => {
    const calls = mockApi(
      copyRoutes((body) => (body["name"] === "Copy of Order shipped" ? problem(409, "Conflict", "a template with that name already exists") : json(templateDetail({ id: "t-copy" }), 201))),
    );
    const user = userEvent.setup();
    const { router } = mount();
    await screen.findByRole("list", { name: "Templates of this workspace" });

    await user.click(screen.getByRole("button", { name: "Actions for Order shipped" }));
    await user.click(screen.getByRole("menuitem", { name: "Duplicate" }));

    await waitFor(() => expect(router.state.location.pathname).toBe("/w/ws1/templates/t-copy"));
    expect(posts(calls).map((body) => body["name"])).toEqual(["Copy of Order shipped", "Copy of Order shipped 2"]);
  });

  it("skips a copy name the library already shows", async () => {
    const calls = mockApi({
      ...copyRoutes(() => json(templateDetail({ id: "t-copy" }), 201)),
      ...libraryRoutes({ items: [...ALL, listItem({ id: "t-old-copy", name: "Copy of Order shipped" })], extra: {
        [`GET ${WS}/templates/t-changed`]: () => json(COPY_SOURCE),
        [`POST ${WS}/templates`]: () => json(templateDetail({ id: "t-copy" }), 201),
      } }),
    });
    const user = userEvent.setup();
    const { router } = mount();
    await screen.findByRole("list", { name: "Templates of this workspace" });

    await user.click(screen.getByRole("button", { name: "Actions for Order shipped" }));
    await user.click(screen.getByRole("menuitem", { name: "Duplicate" }));

    await waitFor(() => expect(router.state.location.pathname).toBe("/w/ws1/templates/t-copy"));
    expect(posts(calls).map((body) => body["name"])).toEqual(["Copy of Order shipped 2"]);
  });

  it("opens the editor, the test send page and the where used tab", async () => {
    mockApi(libraryRoutes());
    const user = userEvent.setup();
    const { router } = mount();
    await screen.findByRole("list", { name: "Templates of this workspace" });

    await user.click(screen.getByRole("button", { name: "Actions for Spring sale" }));
    await user.click(screen.getByRole("menuitem", { name: "Send a test…" }));
    expect(router.state.location.pathname).toBe("/w/ws1/templates/t-live/test");
  });

  it("opens the where used tab of the editor", async () => {
    mockApi(libraryRoutes());
    const user = userEvent.setup();
    const { router } = mount();
    await screen.findByRole("list", { name: "Templates of this workspace" });

    await user.click(screen.getByRole("button", { name: "Actions for Spring sale" }));
    await user.click(screen.getByRole("menuitem", { name: "Where it is used" }));

    expect(router.state.location.pathname).toBe("/w/ws1/templates/t-live");
    expect(router.state.location.search).toBe("?tab=used");
  });

  it("disables Archive while active flows send the template, and names the flows", async () => {
    const calls = mockApi(
      libraryRoutes({
        extra: {
          [`GET ${WS}/templates/t-live/where-used`]: () =>
            json([
              { flow_id: "f1", flow_name: "Welcome series", flow_state: "published", active: true, versions: [1] },
              { flow_id: "f2", flow_name: "Paused one", flow_state: "paused", active: true, versions: [1] },
            ]),
        },
      }),
    );
    const user = userEvent.setup();
    mount();
    await screen.findByRole("list", { name: "Templates of this workspace" });

    await user.click(screen.getByRole("button", { name: "Actions for Spring sale" }));

    const archive = screen.getByRole("menuitem", { name: /Archive/ });
    expect(archive).toHaveAttribute("aria-disabled", "true");
    expect(await screen.findByText("Used by Welcome series. Remove it from the flows first.")).toBeInTheDocument();
    await user.click(archive);
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(calls.filter((call) => call.method === "DELETE")).toHaveLength(0);
  });

  it("archives after a confirmation and reloads the list", async () => {
    let archived = false;
    const calls = mockApi(
      libraryRoutes({
        extra: {
          [`DELETE ${WS}/templates/t-changed`]: () => {
            archived = true;
            return json(templateDetail({ id: "t-changed", archived_at: "2026-10-11T09:00:00Z" }));
          },
          [`GET ${WS}/templates`]: () => json(archived ? [LIVE] : ALL.filter((item) => item !== GONE)),
        },
      }),
    );
    const user = userEvent.setup();
    mount();
    await screen.findByRole("list", { name: "Templates of this workspace" });

    await user.click(screen.getByRole("button", { name: "Actions for Order shipped" }));
    await user.click(screen.getByRole("menuitem", { name: "Archive" }));
    const dialog = await screen.findByRole("dialog", { name: "Archive “Order shipped”?" });
    expect(calls.filter((call) => call.method === "DELETE")).toHaveLength(0);
    await user.click(within(dialog).getByRole("button", { name: "Archive template" }));

    await waitFor(() => expect(screen.queryByRole("link", { name: "Order shipped" })).not.toBeInTheDocument());
    expect(calls.filter((call) => call.method === "DELETE")).toHaveLength(1);
  });

  it("shows the flows from the refusal when a flow started sending the template since the list was read", async () => {
    mockApi(
      libraryRoutes({
        extra: {
          [`DELETE ${WS}/templates/t-changed`]: () =>
            new Response(
              JSON.stringify({
                type: "about:blank",
                title: "Conflict",
                status: 409,
                detail: "active flows still send the template",
                code: "template_in_use",
                flows: [{ flow_id: "f9", flow_name: "Late flow" }],
              }),
              { status: 409, headers: { "content-type": "application/problem+json" } },
            ),
        },
      }),
    );
    const user = userEvent.setup();
    mount();
    await screen.findByRole("list", { name: "Templates of this workspace" });

    await user.click(screen.getByRole("button", { name: "Actions for Order shipped" }));
    await user.click(screen.getByRole("menuitem", { name: "Archive" }));
    const dialog = await screen.findByRole("dialog");
    await user.click(within(dialog).getByRole("button", { name: "Archive template" }));

    expect(await within(dialog).findByText("Used by Late flow. Remove it from the flows first.")).toBeInTheDocument();
    expect(within(dialog).getByRole("button", { name: "Archive template" })).toBeDisabled();
  });
});

describe("making a template", () => {
  async function openDialog() {
    const user = userEvent.setup();
    mount();
    await user.click(await screen.findByRole("button", { name: "New template" }));
    const dialog = await screen.findByRole("dialog", { name: "New template" });
    await user.type(within(dialog).getByLabelText("Name"), "Welcome");
    return { user, dialog };
  }

  function created(calls: { method: string; path: string; body: unknown }[]) {
    return calls.filter((call) => call.method === "POST" && call.path === `${WS}/templates`).map((call) => call.body as Record<string, unknown>);
  }

  function routes() {
    return libraryRoutes({ extra: { [`POST ${WS}/templates`]: () => json(templateDetail({ id: TEMPLATE_ID }), 201) } });
  }

  it("offers only the channels that exist, and the rest as after 1.0", async () => {
    mockApi(routes());
    const { dialog } = await openDialog();

    const channels = within(dialog).getByRole("group", { name: "Channel" });
    expect(within(channels).getByRole("button", { name: "Email" })).toBeEnabled();
    expect(within(channels).getByRole("button", { name: "Webhook" })).toBeEnabled();
    for (const name of ["SMS", "Push", "In-app"]) {
      expect(within(channels).getByRole("button", { name: new RegExp(name) })).toBeDisabled();
    }
    expect(within(channels).getAllByText("After 1.0")).toHaveLength(3);
    expect(within(dialog).queryByText(/assistant/i)).not.toBeInTheDocument();
  });

  it("explains the categories", async () => {
    mockApi(routes());
    const { user, dialog } = await openDialog();

    expect(within(dialog).getByText(/Marketing emails follow consent/)).toBeInTheDocument();
    await user.click(within(dialog).getByRole("button", { name: "Transactional" }));
    expect(within(dialog).getByText(/expects after an action/)).toBeInTheDocument();
  });

  it("makes an HTML template", async () => {
    const calls = mockApi(routes());
    const { user, dialog } = await openDialog();

    await user.click(within(dialog).getByRole("button", { name: "Create template" }));

    await waitFor(() => expect(created(calls)).toHaveLength(1));
    const email = (created(calls)[0]?.["document"] as { email: Record<string, unknown> }).email;
    expect(email["syntax"]).toBeUndefined();
    expect(created(calls)[0]).toMatchObject({ name: "Welcome", channel: "email", category: "marketing" });
  });

  it("makes an MJML template", async () => {
    const calls = mockApi(routes());
    const { user, dialog } = await openDialog();

    await user.click(within(dialog).getByRole("radio", { name: /MJML/ }));
    await user.click(within(dialog).getByRole("button", { name: "Create template" }));

    await waitFor(() => expect(created(calls)).toHaveLength(1));
    const email = (created(calls)[0]?.["document"] as { email: Record<string, unknown> }).email;
    expect(email["syntax"]).toBe("mjml");
  });

  it("makes a block template with the schema 2 block document and no blocks", async () => {
    const calls = mockApi(routes());
    const { user, dialog } = await openDialog();

    await user.click(within(dialog).getByRole("radio", { name: /Blocks/ }));
    await user.click(within(dialog).getByRole("button", { name: "Create template" }));

    await waitFor(() => expect(created(calls)).toHaveLength(1));
    expect(created(calls)[0]?.["document"]).toMatchObject({
      schema: 2,
      channel: "email",
      mode: "blocks",
      blocks: { subject: "", category: "marketing", sections: [] },
    });
  });

  it("makes a webhook template with the server's empty document", async () => {
    const calls = mockApi(routes());
    const { user, dialog } = await openDialog();

    await user.click(within(dialog).getByRole("button", { name: "Webhook" }));
    await user.click(within(dialog).getByRole("button", { name: "Create template" }));

    await waitFor(() => expect(created(calls)).toHaveLength(1));
    expect(created(calls)[0]).toMatchObject({ channel: "webhook", category: "transactional" });
    expect(created(calls)[0]?.["document"]).toBeUndefined();
  });

  it("starts from an existing template of the same kind and copies its draft document", async () => {
    const source = templateDetail({ id: "t-live", name: "Spring sale", current_version: 2 });
    const calls = mockApi({
      ...libraryRoutes({ extra: { [`GET ${WS}/templates/t-live`]: () => json(source), [`POST ${WS}/templates`]: () => json(templateDetail({ id: TEMPLATE_ID }), 201) } }),
    });
    const { user, dialog } = await openDialog();

    await user.click(within(dialog).getByRole("radio", { name: /An existing template/ }));
    expect(within(dialog).queryByRole("radio", { name: "Welcome" })).not.toBeInTheDocument();
    await user.type(within(dialog).getByLabelText("Search templates to copy"), "spring");
    await user.click(within(dialog).getByRole("radio", { name: "Spring sale" }));
    await user.click(within(dialog).getByRole("button", { name: "Create template" }));

    await waitFor(() => expect(created(calls)).toHaveLength(1));
    expect(created(calls)[0]).toMatchObject({ name: "Welcome", channel: "email", category: "marketing", document: source.draft.document });
  });

  it("needs a template picked before it copies one", async () => {
    mockApi(routes());
    const { user, dialog } = await openDialog();

    await user.click(within(dialog).getByRole("radio", { name: /An existing template/ }));

    expect(within(dialog).getByRole("button", { name: "Create template" })).toBeDisabled();
  });

  it("shows the refusal when the name is taken", async () => {
    mockApi(libraryRoutes({ extra: { [`POST ${WS}/templates`]: () => problem(409, "Conflict", "a template with that name already exists") } }));
    const { user, dialog } = await openDialog();

    await user.click(within(dialog).getByRole("button", { name: "Create template" }));

    expect(await within(dialog).findByText("a template with that name already exists")).toBeInTheDocument();
  });

  it("opens the editor route for a template", async () => {
    mockApi(templateRoutes());
    renderRoute(`/w/ws1/templates/${TEMPLATE_ID}`, "/w/:ws/templates/:id", <TemplatePage />);

    expect(await screen.findByRole("heading", { name: "Trial nudge" })).toBeInTheDocument();
  });

  it("opens the where used tab of the editor from the link", async () => {
    mockApi(templateRoutes());
    renderRoute(`/w/ws1/templates/${TEMPLATE_ID}?tab=used`, "/w/:ws/templates/:id", <TemplatePage />);

    expect(await screen.findByRole("tab", { name: "Where used", selected: true })).toBeInTheDocument();
  });
});

describe("importing HTML or MJML", () => {
  async function openDialog() {
    const user = userEvent.setup();
    mount();
    await user.click(await screen.findByRole("button", { name: "Import HTML or MJML" }));
    const dialog = await screen.findByRole("dialog", { name: "Import HTML or MJML" });
    return { user, dialog };
  }

  function created(calls: Call[]) {
    return calls.filter((call) => call.method === "POST" && call.path === `${WS}/templates`).map((call) => call.body as Record<string, unknown>);
  }

  const routes = () => libraryRoutes({ extra: { [`POST ${WS}/templates`]: () => json(templateDetail({ id: TEMPLATE_ID }), 201) } });

  it("makes an HTML code template from pasted source", async () => {
    const calls = mockApi(routes());
    const { user, dialog } = await openDialog();

    fireEvent.change(within(dialog).getByLabelText("Source"), { target: { value: "<html><body><p>Hi</p></body></html>" } });
    await user.type(within(dialog).getByLabelText("Name"), "Imported");
    expect(within(dialog).getByText("Read as HTML.")).toBeInTheDocument();
    await user.click(within(dialog).getByRole("button", { name: "Import template" }));

    await waitFor(() => expect(created(calls)).toHaveLength(1));
    const body = created(calls)[0] as { name: string; document: { mode: string; email: Record<string, unknown> } };
    expect(body.name).toBe("Imported");
    expect(body.document.mode).toBe("code");
    expect(body.document.email["html"]).toBe("<html><body><p>Hi</p></body></html>");
    expect(body.document.email["syntax"]).toBeUndefined();
  });

  it("reads a file, names the template after it and detects MJML", async () => {
    const calls = mockApi(routes());
    const { user, dialog } = await openDialog();
    const source = "<mjml><mj-body><mj-section><mj-column><mj-text>Hi</mj-text></mj-column></mj-section></mj-body></mjml>";

    await user.upload(within(dialog).getByLabelText("File"), new File([source], "monthly-news.mjml", { type: "text/plain" }));
    expect(await within(dialog).findByText("Read as MJML.")).toBeInTheDocument();
    expect(within(dialog).getByLabelText("Name")).toHaveValue("monthly-news");
    await user.click(within(dialog).getByRole("button", { name: "Import template" }));

    await waitFor(() => expect(created(calls)).toHaveLength(1));
    const email = (created(calls)[0] as { document: { email: Record<string, unknown> } }).document.email;
    expect(email["syntax"]).toBe("mjml");
    expect(email["html"]).toBe(source);
  });

  it("needs a source and a name", async () => {
    mockApi(routes());
    const { dialog } = await openDialog();

    expect(within(dialog).getByRole("button", { name: "Import template" })).toBeDisabled();
  });
});
