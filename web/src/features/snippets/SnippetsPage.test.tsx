import { screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { json, mockApi } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { SNIPPET_ID, snippetBody, snippetRoutes, WS } from "../../test/snippetsApi";
import { SnippetsPage } from "./SnippetsPage";

function mount() {
  return renderRoute("/w/ws1/snippets", "/w/:ws/snippets", <SnippetsPage />);
}

function refusal(status: number, body: Record<string, unknown>) {
  return new Response(JSON.stringify({ type: "about:blank", title: "Refused", status, detail: "Refused.", ...body }), {
    status,
    headers: { "content-type": "application/problem+json" },
  });
}

describe("the snippet list", () => {
  it("lists name, version and how many things use each snippet", async () => {
    mockApi({
      ...snippetRoutes(),
      [`GET ${WS}/snippets`]: () =>
        json([
          { ...snippetBody({ current_version: 3 }), used_by_templates: 2 },
          { ...snippetBody({ id: "s2", name: "Other note" }), used_by_templates: 1 },
        ]),
    });
    mount();

    const table = await screen.findByRole("table", { name: "Snippets of this workspace" });
    expect(within(table).getByRole("link", { name: "Shipping promise" })).toHaveAttribute("href", `/w/ws1/snippets/${SNIPPET_ID}`);
    expect(within(table).getByText("v3")).toBeInTheDocument();
    expect(within(table).getByText("Draft")).toBeInTheDocument();
    expect(within(table).getByText("2 templates")).toBeInTheDocument();
    expect(within(table).getByText("1 template")).toBeInTheDocument();
  });

  it("shows an empty state, and an error with a retry", async () => {
    let attempts = 0;
    mockApi({
      ...snippetRoutes(),
      [`GET ${WS}/snippets`]: () => {
        attempts += 1;
        return attempts === 1 ? new Response("{}", { status: 500, headers: { "content-type": "application/json" } }) : json([]);
      },
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Try again" }));
    expect(await screen.findByText("No snippets yet")).toBeInTheDocument();
  });

  it("refuses an archive and lists the templates and themes that hold the snippet", async () => {
    mockApi({
      ...snippetRoutes(),
      [`DELETE ${WS}/snippets/${SNIPPET_ID}`]: () =>
        refusal(409, {
          code: "snippet_in_use",
          snippet_templates: [{ id: "t1", name: "Welcome email" }],
          snippet_themes: [{ id: "th1", name: "Footer theme" }],
        }),
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Archive Shipping promise" }));
    const dialog = await screen.findByRole("dialog", { name: "Archive Shipping promise?" });
    await user.click(within(dialog).getByRole("button", { name: "Archive" }));

    const list = await within(dialog).findByRole("list", { name: "Still holding this snippet" });
    expect(within(list).getByRole("link", { name: "Welcome email" })).toHaveAttribute("href", "/w/ws1/templates/t1");
    expect(within(list).getByRole("link", { name: "Footer theme" })).toHaveAttribute("href", "/w/ws1/themes/th1");
    expect(within(list).getByText("Template")).toBeInTheDocument();
    expect(within(list).getByText("Theme")).toBeInTheDocument();
  });

  it("offers archive only to an admin", async () => {
    mockApi(snippetRoutes({ role: "editor" }));
    mount();

    await screen.findByRole("link", { name: "Shipping promise" });
    expect(screen.queryByRole("button", { name: "Archive Shipping promise" })).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "New snippet" })).toBeInTheDocument();
  });

  it("says when a name is taken", async () => {
    mockApi({
      ...snippetRoutes(),
      [`POST ${WS}/snippets`]: () => refusal(409, {}),
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "New snippet" }));
    await user.type(screen.getByRole("textbox", { name: /Name/ }), "Shipping promise");
    await user.click(screen.getByRole("button", { name: "Create snippet" }));

    expect(await screen.findByText(/already exists/)).toBeInTheDocument();
  });
});
