import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { json, mockApi, problem } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { previewBody, previews, templateRoutes, TEMPLATE_ID, WS } from "../../test/templatesApi";
import { TemplatePage } from "./TemplatePage";

function mount() {
  return renderRoute(`/w/ws1/templates/${TEMPLATE_ID}`, "/w/:ws/templates/:id", <TemplatePage />);
}

const WAIT = { timeout: 4000 };

describe("the preview", () => {
  it("shows the server-rendered subject and HTML", async () => {
    mockApi(templateRoutes());
    mount();

    const frame = await screen.findByTitle("Preview of the message");
    expect(frame).toHaveAttribute("srcdoc", "<p>Hi Aarhus</p>");
    expect(screen.getByText("Hello")).toBeInTheDocument();
  });

  it("puts the HTML in an iframe with exactly an empty sandbox, so no script runs", async () => {
    mockApi(templateRoutes({ preview: () => previewBody({ html: "<script>window.__ran = true</script><p>x</p>" }) }));
    mount();

    const frame = await screen.findByTitle("Preview of the message");
    expect(frame.getAttribute("sandbox")).toBe("");
    expect(frame.tagName).toBe("IFRAME");
    expect((window as unknown as Record<string, unknown>)["__ran"]).toBeUndefined();
    expect(document.querySelector("body > script, #root script")).toBeNull();
  });

  it("renders again after an edit", async () => {
    const calls = mockApi(
      templateRoutes({
        preview: (_document, asked) => previewBody({ html: `<p>render ${asked}</p>` }),
      }),
    );
    const user = userEvent.setup();
    mount();

    expect(await screen.findByTitle("Preview of the message")).toHaveAttribute("srcdoc", "<p>render 1</p>");
    await user.click(await screen.findByRole("textbox", { name: /source/ }));
    await user.paste("x");

    await waitFor(() => expect(screen.getByTitle("Preview of the message")).toHaveAttribute("srcdoc", "<p>render 2</p>"), WAIT);
    expect(previews(calls)).toHaveLength(2);
  });

  it("shows a render failure as an error in the pane, with a retry", async () => {
    let fail = true;
    mockApi(
      templateRoutes({
        preview: () =>
          fail
            ? problem(422, "Unprocessable", "the template used too much memory to render", undefined, "render_memory_limit")
            : previewBody(),
      }),
    );
    const user = userEvent.setup();
    mount();

    const pane = await screen.findByRole("region", { name: "Preview" });
    expect(await within(pane).findByText("The preview could not be rendered")).toBeInTheDocument();
    expect(within(pane).getByText("the template used too much memory to render")).toBeInTheDocument();
    expect(screen.queryByTitle("Preview of the message")).not.toBeInTheDocument();
    fail = false;
    await user.click(within(pane).getByRole("button", { name: "Try again" }));

    expect(await screen.findByTitle("Preview of the message")).toBeInTheDocument();
  });

  it("says nothing was rendered when the draft has blocking issues", async () => {
    mockApi(
      templateRoutes({
        preview: () =>
          previewBody({
            rendered: false,
            html: null,
            subject: null,
            text: null,
            issues: [{ code: "missing_unsubscribe", severity: "blocking", at: "/email/html" }],
          }),
      }),
    );
    mount();

    expect(await screen.findByText(/Nothing was rendered/)).toBeInTheDocument();
  });

  it("switches between the desktop and mobile widths and to the text part", async () => {
    mockApi(templateRoutes());
    const user = userEvent.setup();
    mount();

    const frame = await screen.findByTitle("Preview of the message");
    expect(frame.parentElement).not.toHaveClass("tp-frame-mobile");
    await user.click(screen.getByRole("button", { name: "Mobile" }));
    expect(screen.getByTitle("Preview of the message").parentElement).toHaveClass("tp-frame-mobile");
    expect(screen.getByRole("button", { name: "Mobile" })).toHaveAttribute("aria-pressed", "true");

    await user.click(screen.getByRole("button", { name: "Text" }));
    expect(screen.getByLabelText("Text part", { selector: "pre" })).toHaveTextContent("Hi Aarhus");
  });
});

describe("previewing as a person", () => {
  it("hides the real person choice from someone who is not an admin", async () => {
    mockApi(templateRoutes({ role: "member" }));
    mount();

    await screen.findByTitle("Preview of the message");
    const select = screen.getByLabelText("Preview as");
    expect(within(select).queryByRole("option", { name: "A real person" })).not.toBeInTheDocument();
    expect(screen.getByText("Only admins can preview as a real person.")).toBeInTheDocument();
  });

  it("lets an admin find a person and previews with their id", async () => {
    const calls = mockApi({
      ...templateRoutes(),
      [`POST ${WS}/people/search`]: () =>
        json({ people: [{ id: "p-1", display: { kind: "email", masked: false, value: "ada@example.test" } }], next_cursor: null }),
    });
    const user = userEvent.setup();
    mount();

    await screen.findByTitle("Preview of the message");
    await user.selectOptions(screen.getByLabelText("Preview as"), "other");
    await user.type(await screen.findByLabelText("Person's email address"), "ada@example.test");
    await user.click(screen.getByRole("button", { name: "Search" }));
    await user.click(await screen.findByRole("button", { name: "Preview as ada@example.test" }));

    await waitFor(() => expect(previews(calls).some((call) => (call.body as { person_id?: string }).person_id === "p-1")).toBe(true), WAIT);
    expect(screen.getByLabelText("Preview as")).toHaveValue("p-1");
  });
});

describe("the saved preview people", () => {
  const SAVED = [{ person_id: "p-1", display: { kind: "email", masked: false, value: "ada@example.test" }, created_at: "2026-10-26T00:00:00Z" }];

  it("lists the sample person, the saved people and the picker for an admin", async () => {
    mockApi(templateRoutes({ saved: SAVED }));
    mount();

    await screen.findByTitle("Preview of the message");
    const select = screen.getByLabelText("Preview as");
    await within(select).findByRole("option", { name: "ada@example.test" });
    expect(within(select).getAllByRole("option").map((option) => option.textContent)).toEqual(["Sample person", "ada@example.test", "Pick another person…"]);
  });

  it("shows only the sample person to a writer, and never asks for the saved people", async () => {
    const calls = mockApi(templateRoutes({ role: "member", saved: SAVED }));
    mount();

    await screen.findByTitle("Preview of the message");
    const select = screen.getByLabelText("Preview as");
    expect(within(select).getAllByRole("option").map((option) => option.textContent)).toEqual(["Sample person"]);
    expect(screen.queryByRole("button", { name: "Save" })).not.toBeInTheDocument();
    expect(calls.some((call) => call.path.endsWith("/preview-people"))).toBe(false);
  });

  it("previews as a saved person and removes them with the button", async () => {
    const calls = mockApi(templateRoutes({ saved: SAVED }));
    const user = userEvent.setup();
    mount();

    await screen.findByTitle("Preview of the message");
    await user.selectOptions(screen.getByLabelText("Preview as"), await screen.findByRole("option", { name: "ada@example.test" }));

    await waitFor(() => expect(previews(calls).some((call) => (call.body as { person_id?: string }).person_id === "p-1")).toBe(true), WAIT);
    await user.click(await screen.findByRole("button", { name: "Remove" }));
    await waitFor(() => expect(calls.some((call) => call.method === "DELETE" && call.path.endsWith("/preview-people/p-1"))).toBe(true), WAIT);
  });

  it("saves a person picked by search", async () => {
    const calls = mockApi({
      ...templateRoutes(),
      [`POST ${WS}/people/search`]: () => json({ people: [{ id: "p-2", display: { kind: "email", masked: false, value: "bo@example.test" } }], next_cursor: null }),
    });
    const user = userEvent.setup();
    mount();

    await screen.findByTitle("Preview of the message");
    await user.selectOptions(screen.getByLabelText("Preview as"), "other");
    await user.type(await screen.findByLabelText("Person's email address"), "bo@example.test");
    await user.click(screen.getByRole("button", { name: "Search" }));
    await user.click(await screen.findByRole("button", { name: "Preview as bo@example.test" }));
    await user.click(await screen.findByRole("button", { name: "Save" }));

    await waitFor(() => expect(calls.some((call) => call.method === "POST" && call.path.endsWith("/preview-people") && (call.body as { person_id?: string }).person_id === "p-2")).toBe(true), WAIT);
  });
});
