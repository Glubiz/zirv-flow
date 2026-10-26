import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { json, mockApi, problem } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { templateBody, templateRoutes, TEMPLATE_ID, WS } from "../../test/templatesApi";
import { TemplatePage } from "./TemplatePage";
import { TemplatesPage } from "./TemplatesPage";

function mount() {
  return renderRoute("/w/ws1/templates", "/w/:ws/templates", <TemplatesPage />);
}

describe("the template list", () => {
  it("lists the templates with a status in words", async () => {
    mockApi({
      ...templateRoutes(),
      [`GET ${WS}/templates`]: () => json([templateBody(), templateBody({ id: "t2", name: "Receipt", current_version: 3 })]),
    });
    mount();

    const table = await screen.findByRole("table", { name: "Templates of this workspace" });
    expect(within(table).getByRole("link", { name: "Trial nudge" })).toHaveAttribute("href", `/w/ws1/templates/${TEMPLATE_ID}`);
    expect(within(table).getByText("Draft, never published")).toBeInTheDocument();
    expect(within(table).getByText("Published v3")).toBeInTheDocument();
  });

  it("shows an empty state", async () => {
    mockApi({ ...templateRoutes(), [`GET ${WS}/templates`]: () => json([]) });
    mount();

    expect(await screen.findByText("No templates yet")).toBeInTheDocument();
  });

  it("shows an error with a retry that loads again", async () => {
    let attempts = 0;
    mockApi({
      ...templateRoutes(),
      [`GET ${WS}/templates`]: () => {
        attempts += 1;
        return attempts === 1 ? problem(500, "Internal Server Error", "internal error") : json([templateBody()]);
      },
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Try again" }));

    expect(await screen.findByRole("link", { name: "Trial nudge" })).toBeInTheDocument();
  });

  it("offers no way to make a template to someone who is not a member", async () => {
    mockApi(templateRoutes({ role: "reader" }));
    mount();

    await screen.findByRole("table");
    expect(screen.queryByRole("button", { name: "New template" })).not.toBeInTheDocument();
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
    return {
      ...templateRoutes(),
      [`POST ${WS}/templates`]: () => json(templateBody({ id: TEMPLATE_ID }), 201),
    };
  }

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

    await user.selectOptions(within(dialog).getByLabelText("Channel"), "webhook");
    await user.click(within(dialog).getByRole("button", { name: "Create template" }));

    await waitFor(() => expect(created(calls)).toHaveLength(1));
    expect(created(calls)[0]).toMatchObject({ channel: "webhook", category: "transactional" });
    expect(created(calls)[0]?.["document"]).toBeUndefined();
  });

  it("shows the refusal when the name is taken", async () => {
    mockApi({ ...routes(), [`POST ${WS}/templates`]: () => problem(409, "Conflict", "a template with that name already exists") });
    const { user, dialog } = await openDialog();

    await user.click(within(dialog).getByRole("button", { name: "Create template" }));

    expect(await within(dialog).findByText("a template with that name already exists")).toBeInTheDocument();
  });

  it("opens the editor route for a template", async () => {
    mockApi(templateRoutes());
    renderRoute(`/w/ws1/templates/${TEMPLATE_ID}`, "/w/:ws/templates/:id", <TemplatePage />);

    expect(await screen.findByRole("heading", { name: "Trial nudge" })).toBeInTheDocument();
  });
});
