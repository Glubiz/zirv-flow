import { act, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { mockApi, type Handler } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { drafts, templateDetail, templateRoutes, TEMPLATE_ID, WS, type Doc } from "../../test/templatesApi";
import { TemplatePage } from "./TemplatePage";

const WAIT = { timeout: 4000 };
const PUT = `PUT ${WS}/templates/${TEMPLATE_ID}/draft`;

function mount() {
  return renderRoute(`/w/ws1/templates/${TEMPLATE_ID}`, "/w/:ws/templates/:id", <TemplatePage />);
}

function htmlOf(document: Doc): string {
  return String((document["email"] as Doc)["html"]);
}

describe("saving the draft", () => {
  it("saves an edit made inside the debounce when the page is left", async () => {
    const calls = mockApi(templateRoutes());
    const user = userEvent.setup();
    const view = mount();

    await user.click(await screen.findByRole("textbox", { name: /source/ }));
    await user.paste("<b>late</b>");
    expect(drafts(calls)).toHaveLength(0);
    view.unmount();

    await waitFor(() => expect(drafts(calls)).toHaveLength(1), WAIT);
    expect(htmlOf(drafts(calls)[0] as Doc)).toContain("<b>late</b>");
  });

  it("asks before leaving the page with an unsaved draft", async () => {
    mockApi(templateRoutes());
    const user = userEvent.setup();
    const view = mount();

    await user.click(await screen.findByRole("textbox", { name: /source/ }));
    await user.paste("x");
    await act(async () => {
      await view.router.navigate("/w/ws1/templates");
    });

    const dialog = await screen.findByRole("dialog", { name: "Leave without saving?" });
    await user.click(within(dialog).getByRole("button", { name: "Keep editing" }));
    expect(screen.queryByRole("dialog", { name: "Leave without saving?" })).not.toBeInTheDocument();
    expect(view.router.state.location.pathname).toBe(`/w/ws1/templates/${TEMPLATE_ID}`);
  });

  it("does not ask when the draft is saved", async () => {
    mockApi(templateRoutes());
    const view = mount();

    await screen.findByText("Draft saved");
    await act(async () => {
      await view.router.navigate("/w/ws1/templates");
    });

    expect(screen.queryByRole("dialog", { name: "Leave without saving?" })).not.toBeInTheDocument();
    expect(view.router.state.location.pathname).toBe("/w/ws1/templates");
  });

  it("runs one save at a time and sends the newest content after the one in flight", async () => {
    const base = templateRoutes();
    const started: string[] = [];
    let release: () => void = () => undefined;
    const gate = new Promise<void>((resolve) => {
      release = resolve;
    });
    let inFlight = 0;
    let overlapped = false;
    const put: Handler = async (request) => {
      inFlight += 1;
      if (inFlight > 1) overlapped = true;
      const document = ((await request.clone().json()) as { document: Doc }).document;
      started.push(htmlOf(document));
      if (started.length === 1) await gate;
      const answer = await (base[PUT] as Handler)(request);
      inFlight -= 1;
      return answer;
    };
    mockApi({ ...base, [PUT]: put });
    const user = userEvent.setup();
    mount();

    const field = await screen.findByRole("textbox", { name: /source/ });
    await user.click(field);
    await user.paste("A");
    await waitFor(() => expect(started).toHaveLength(1), WAIT);
    await user.paste("B");
    await new Promise((resolve) => setTimeout(resolve, 900));

    expect(started).toHaveLength(1);
    expect(overlapped).toBe(false);
    release();
    await waitFor(() => expect(started).toHaveLength(2), WAIT);
    expect(started[1]).toContain("AB");
    expect(overlapped).toBe(false);
    expect(await screen.findByText("Draft saved", undefined, WAIT)).toBeInTheDocument();
  });
});

describe("the other template fixes", () => {
  it("offers no test send for a webhook template", async () => {
    const document: Doc = { schema: 1, channel: "webhook", mode: "code", webhook: { headers: {}, body: "{}" } };
    mockApi(templateRoutes({ detail: templateDetail({ channel: "webhook" }, document) }));
    mount();

    await screen.findByRole("button", { name: "Publish…" });
    expect(screen.queryByRole("button", { name: "Send a test" })).not.toBeInTheDocument();
  });

  it("saves the draft, then opens the test page", async () => {
    const calls = mockApi(templateRoutes());
    const user = userEvent.setup();
    const view = mount();

    await user.click(await screen.findByRole("textbox", { name: /source/ }));
    await user.paste("<b>fresh</b>");
    await user.click(await screen.findByRole("button", { name: "Send a test" }));

    await waitFor(() => expect(view.router.state.location.pathname).toBe(`/w/ws1/templates/${TEMPLATE_ID}/test`), WAIT);
    expect(drafts(calls).map(htmlOf).join()).toContain("fresh");
  });

  it("saves the draft, then opens the history from the Versions link", async () => {
    const calls = mockApi(templateRoutes());
    const user = userEvent.setup();
    const view = mount();

    await user.click(await screen.findByRole("textbox", { name: /source/ }));
    await user.paste("<b>fresh</b>");
    expect(screen.queryByRole("tab", { name: "Versions" })).not.toBeInTheDocument();
    await user.click(await screen.findByRole("link", { name: "Versions" }));

    await waitFor(() => expect(view.router.state.location.pathname).toBe(`/w/ws1/templates/${TEMPLATE_ID}/history`), WAIT);
    expect(drafts(calls).map(htmlOf).join()).toContain("fresh");
  });

  it("moves between the view tabs with the arrow keys", async () => {
    mockApi(templateRoutes());
    const user = userEvent.setup();
    mount();

    const editor = await screen.findByRole("tab", { name: "Editor" });
    editor.focus();
    await user.keyboard("{ArrowRight}");

    const used = screen.getByRole("tab", { name: "Where used" });
    expect(used).toHaveAttribute("aria-selected", "true");
    expect(used).toHaveFocus();
    await user.keyboard("{ArrowLeft}");
    expect(screen.getByRole("tab", { name: "Editor" })).toHaveFocus();
  });
});
