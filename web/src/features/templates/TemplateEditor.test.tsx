import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { mockApi } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import {
  drafts,
  emailDocument,
  previewBody,
  previews,
  templateDetail,
  SENDER_ID,
  templateRoutes,
  TEMPLATE_ID,
  type Doc,
  type Issue,
} from "../../test/templatesApi";
import { TemplatePage } from "./TemplatePage";

function mount() {
  return renderRoute(`/w/ws1/templates/${TEMPLATE_ID}`, "/w/:ws/templates/:id", <TemplatePage />);
}

const WAIT = { timeout: 4000 };

function blocking(code: string, at: string, extra: Partial<Issue> = {}): Issue {
  return { code, severity: "blocking", at, ...extra };
}

async function sourceField() {
  return await screen.findByRole<HTMLTextAreaElement>("textbox", { name: /source/ });
}

describe("editing a code template", () => {
  it("saves an HTML edit as the draft and asks for the preview again", async () => {
    const calls = mockApi(templateRoutes());
    const user = userEvent.setup();
    mount();

    const field = await sourceField();
    await user.click(field);
    await user.paste("<b>more</b>");

    await waitFor(() => expect(drafts(calls)).toHaveLength(1), WAIT);
    const email = (drafts(calls)[0] as { email: Record<string, unknown> }).email;
    expect(String(email["html"])).toContain("<b>more</b>");
    expect(email["syntax"]).toBeUndefined();
    await waitFor(() => expect(previews(calls).length).toBeGreaterThanOrEqual(2), WAIT);
    expect(await screen.findByText("Draft saved")).toBeInTheDocument();
  });

  it("edits MJML and keeps the mode in the saved document", async () => {
    const calls = mockApi(
      templateRoutes({ detail: templateDetail({}, emailDocument({}, { syntax: "mjml", html: "<mjml></mjml>" })) }),
    );
    const user = userEvent.setup();
    mount();

    expect(await screen.findByText("MJML template")).toBeInTheDocument();
    const field = await screen.findByRole<HTMLTextAreaElement>("textbox", { name: "MJML source" });
    await user.click(field);
    await user.paste("<mj-body/>");

    await waitFor(() => expect(drafts(calls)).toHaveLength(1), WAIT);
    const email = (drafts(calls)[0] as { email: Record<string, unknown> }).email;
    expect(email["syntax"]).toBe("mjml");
    expect(String(email["html"])).toContain("<mj-body/>");
  });

  it("shows the validation the server returns for the saved draft, inline and in the list", async () => {
    mockApi(
      templateRoutes({
        preview: () => previewBody({ issues: [blocking("unknown_merge_field", "/email/html", { field: "person.nickname" })] }),
      }),
    );
    mount();

    const list = await screen.findByRole("region", { name: "Validation results" });
    expect(await within(list).findByText("1 issue, 1 blocking publishing")).toBeInTheDocument();
    expect(within(list).getByText("person.nickname is not a field in this workspace.")).toBeInTheDocument();
    const inline = screen.getByRole("list", { name: "Problems in this field" });
    expect(within(inline).getByText(/person\.nickname is not a field/)).toBeInTheDocument();
  });

  it.each([
    ["an unknown merge field", blocking("unknown_merge_field", "/email/html", { field: "person.nickname" })],
    ["a missing unsubscribe link", blocking("missing_unsubscribe", "/email/html")],
    ["an mrml parse error", blocking("mjml_parse_error", "/email/html", { message: "unexpected tag" })],
  ])("blocks publishing for %s", async (_name, issue) => {
    mockApi(templateRoutes({ preview: () => previewBody({ issues: [issue] }) }));
    mount();

    const publish = await screen.findByRole("button", { name: "Publish…" });
    await waitFor(() => expect(screen.getByText("1 issue, 1 blocking publishing")).toBeInTheDocument());
    expect(publish).toBeDisabled();
    expect(screen.getAllByText("Blocks publishing").length).toBeGreaterThan(0);
  });

  it("allows publishing when only warnings remain", async () => {
    mockApi(
      templateRoutes({ preview: () => previewBody({ issues: [{ code: "insecure_link", severity: "warning", at: "/email/html" }] }) }),
    );
    mount();

    const publish = await screen.findByRole("button", { name: "Publish…" });
    await waitFor(() => expect(publish).toBeEnabled());
    expect(screen.getByText("1 issue, 0 blocking publishing")).toBeInTheDocument();
  });

  it("inserts a merge tag from the picker at the caret", async () => {
    const calls = mockApi(templateRoutes({ detail: templateDetail({}, emailDocument({}, { html: "<p></p>" })) }));
    const user = userEvent.setup();
    mount();

    const field = await sourceField();
    field.focus();
    field.setSelectionRange(3, 3);
    await user.click(await screen.findByRole("button", { name: "Insert person.city" }));

    await waitFor(() => expect(field.value).toBe('<p>{{ person.city | default("there") }}</p>'));
    await waitFor(() => expect(drafts(calls)).toHaveLength(1), WAIT);
  });

  it("offers the system values and the properties of a chosen event", async () => {
    mockApi(templateRoutes());
    const user = userEvent.setup();
    mount();

    expect(await screen.findByRole("button", { name: "Insert system.unsubscribe_url" })).toBeInTheDocument();
    await user.selectOptions(await screen.findByLabelText("Event"), "purchase");
    expect(await screen.findByRole("button", { name: "Insert event.amount" })).toBeInTheDocument();
  });

  it("edits the subject and the text part on their own tabs", async () => {
    const calls = mockApi(templateRoutes());
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("tab", { name: "Subject" }));
    const subject = screen.getByRole("textbox", { name: "Subject" });
    await user.click(subject);
    await user.paste("!");
    await waitFor(() => expect(drafts(calls)).toHaveLength(1), WAIT);
    expect(((drafts(calls)[0] as Doc)["email"] as Doc)["subject"]).toBe("Hello!");
  });

  it("shows a read-only editor to someone who cannot write", async () => {
    mockApi(templateRoutes({ role: "reader" }));
    mount();

    const field = await sourceField();
    expect(field).toHaveAttribute("readonly");
    expect(screen.getByText(/You can look at this template but not change it/)).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Save draft" })).not.toBeInTheDocument();
  });

  it("says so, with a retry, when the draft cannot be saved", async () => {
    const routes = templateRoutes();
    let fail = true;
    const put = routes[`PUT /api/v1/workspaces/ws1/templates/${TEMPLATE_ID}/draft`];
    routes[`PUT /api/v1/workspaces/ws1/templates/${TEMPLATE_ID}/draft`] = (request) =>
      fail ? new Response(JSON.stringify({ title: "Server error", detail: "boom", status: 500 }), { status: 500, headers: { "content-type": "application/problem+json" } }) : put!(request);
    mockApi(routes);
    const user = userEvent.setup();
    mount();

    const field = await sourceField();
    await user.click(field);
    await user.paste("x");

    expect(await screen.findByText("Could not save", undefined, WAIT)).toBeInTheDocument();
    fail = false;
    await user.click(screen.getByRole("button", { name: "Try saving again" }));
    expect(await screen.findByText("Draft saved", undefined, WAIT)).toBeInTheDocument();
  });

  it("edits the headers and body of a webhook template", async () => {
    const document: Doc = { schema: 1, channel: "webhook", mode: "code", webhook: { headers: { "X-Key": "{{ person.city }}" }, body: "{}" } };
    const calls = mockApi(templateRoutes({ detail: templateDetail({ channel: "webhook" }, document) }));
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("tab", { name: "Headers" }));
    expect(screen.getByLabelText("Header 1 name")).toHaveValue("X-Key");
    await user.click(screen.getByRole("tab", { name: "Body" }));
    await user.click(screen.getByRole("textbox", { name: "JSON body" }));
    await user.paste(" ");

    await waitFor(() => expect(drafts(calls)).toHaveLength(1), WAIT);
    expect(((drafts(calls)[0] as Doc)["webhook"] as Doc)["headers"]).toEqual({ "X-Key": "{{ person.city }}" });
  });
  it("lets a writer who is not an admin pick the sender by name on the Settings tab", async () => {
    mockApi(templateRoutes({ role: "editor" }));
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("tab", { name: "Settings" }));

    expect(await screen.findByRole("option", { name: "Acme <hello@mail.acme.example>" })).toBeInTheDocument();
    expect(screen.getByLabelText("Sender identity")).toHaveValue(SENDER_ID);
  });
});
