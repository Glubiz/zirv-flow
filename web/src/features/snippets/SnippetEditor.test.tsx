import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { json, mockApi } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { heading, previewBody, saves, SNIPPET_ID, snippetDetail, snippetRoutes, templateUse, usedBy, WS } from "../../test/snippetsApi";
import { SnippetPage } from "./SnippetPage";

const WAIT = { timeout: 4000 };
const BASE = `${WS}/snippets/${SNIPPET_ID}`;

function mount() {
  return renderRoute(`/w/ws1/snippets/${SNIPPET_ID}`, "/w/:ws/snippets/:id", <SnippetPage />);
}

function refusal(status: number, body: Record<string, unknown>) {
  return new Response(JSON.stringify({ type: "about:blank", title: "Refused", status, detail: "Refused.", ...body }), {
    status,
    headers: { "content-type": "application/problem+json" },
  });
}

async function publishButton() {
  const publish = await screen.findByRole("button", { name: "Publish…" });
  await waitFor(() => expect(publish).toBeEnabled(), WAIT);
  return publish;
}

describe("the block editor in snippet mode", () => {
  it("offers the seven blocks and no sections, snippet block, visibility, document style or message", async () => {
    mockApi(snippetRoutes({ detail: snippetDetail({}, [heading("heading-1", "Free returns")]) }));
    const user = userEvent.setup();
    mount();

    const palette = await screen.findByRole("group", { name: "Add a block" });
    const names = within(palette)
      .getAllByRole("button")
      .map((button) => button.textContent);
    expect(names).toEqual(["Heading", "Text", "Image", "Button", "Divider", "Spacer", "Raw HTML"]);
    expect(screen.queryByRole("button", { name: /section/i })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /snippet block/i })).not.toBeInTheDocument();
    expect(screen.queryByRole("tab", { name: /Theme|Document style|Message/ })).not.toBeInTheDocument();

    await user.click(await screen.findByRole("group", { name: "Heading block 1" }));
    expect(screen.queryByText(/visib/i)).not.toBeInTheDocument();
    expect(screen.queryByText(/Section 1/)).not.toBeInTheDocument();
  });

  it("saves the blocks as a flat list with the revision it read", async () => {
    const calls = mockApi(snippetRoutes());
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Add heading block" }));

    await waitFor(() => expect(saves(calls).length).toBeGreaterThan(0), WAIT);
    const sent = saves(calls)[0];
    expect(sent?.revision).toBe(1);
    expect(sent?.content).toEqual({ blocks: [expect.objectContaining({ type: "heading" })] });
  });

  it("offers a reload when someone else saved", async () => {
    mockApi(snippetRoutes({ save: () => refusal(409, { code: "snippet_revision_mismatch" }) }));
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Add heading block" }));

    const banner = await screen.findByRole("alert", undefined, WAIT);
    expect(banner).toHaveTextContent("Someone else saved this snippet");
    expect(within(banner).getByRole("button", { name: "Reload" })).toBeInTheDocument();
  });

  it("sends the revision the last save returned with the next save", async () => {
    const calls = mockApi(snippetRoutes());
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Add heading block" }));
    await waitFor(() => expect(saves(calls)).toHaveLength(1), WAIT);
    await user.click(screen.getByRole("button", { name: "Add text block" }));
    await waitFor(() => expect(saves(calls)).toHaveLength(2), WAIT);

    expect(saves(calls).map((sent) => sent.revision)).toEqual([1, 2]);
  });

  it("makes no further save after a revision conflict, even on leaving", async () => {
    const calls = mockApi(snippetRoutes({ save: () => refusal(409, { code: "snippet_revision_mismatch" }) }));
    const user = userEvent.setup();
    const { unmount } = mount();

    await user.click(await screen.findByRole("button", { name: "Add heading block" }));
    await screen.findByRole("alert", undefined, WAIT);
    await user.click(screen.getByRole("button", { name: "Add text block" }));
    await new Promise((resolve) => setTimeout(resolve, 800));
    unmount();
    await new Promise((resolve) => setTimeout(resolve, 100));

    expect(saves(calls)).toHaveLength(1);
  });

  it("shows preview warnings as issues and still renders the preview", async () => {
    mockApi(
      snippetRoutes({
        preview: () => previewBody({ issues: [{ code: "image_without_alt", severity: "warning", at: "/blocks/block/image-1/alt" }] }),
      }),
    );
    mount();

    expect(await screen.findByText(/An image has no alt text/, undefined, WAIT)).toBeInTheDocument();
    const frame = await screen.findByTitle("Preview of the snippet");
    expect(frame).toHaveAttribute("sandbox", "");
    const document = new DOMParser().parseFromString(frame.getAttribute("srcdoc") ?? "", "text/html");
    expect(document.head.firstElementChild?.localName).toBe("meta");
    expect(document.head.firstElementChild?.getAttribute("http-equiv")).toBe("Content-Security-Policy");
    expect(document.body.innerHTML).toBe("<p>Free returns</p>");
    expect(screen.getByRole("button", { name: "Mobile" })).toBeInTheDocument();
  });

  it("offers the theme logo when choosing an image", async () => {
    mockApi(snippetRoutes({ detail: snippetDetail({}, [{ type: "image", id: "image-1", src: { url: "" }, alt: "" }]) }));
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("group", { name: "Image block 1" }));
    await user.click(await screen.findByRole("button", { name: "Choose image…" }));

    expect(await screen.findByText("Theme logo")).toBeInTheDocument();
  });
});

describe("the snippet page", () => {
  it("shows the title bar with the status and the used-in count", async () => {
    mockApi(snippetRoutes({ detail: snippetDetail({ current_version: 3 }), uses: usedBy({ templates: [templateUse(1), templateUse(2)] }) }));
    mount();

    expect(await screen.findByRole("link", { name: "← Snippets" })).toHaveAttribute("href", "/w/ws1/snippets");
    expect(screen.getByText("Draft · v3 live")).toBeInTheDocument();
    expect(await screen.findByRole("button", { name: "Used in 2 templates" })).toBeInTheDocument();
    expect(screen.getByRole("tab", { name: "Where used" })).toBeInTheDocument();
  });

  it("hides publishing from a writer", async () => {
    mockApi(snippetRoutes({ role: "editor" }));
    mount();

    await screen.findByTitle("Preview of the snippet");
    expect(screen.queryByRole("button", { name: "Publish…" })).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Save draft" })).toBeInTheDocument();
  });

  it("lists the versions and shows one read only", async () => {
    mockApi({
      ...snippetRoutes({ versions: [{ version: 2, published_at: "2026-10-05T09:00:00Z", warnings: 1, published_by: null }] }),
      [`GET ${BASE}/versions/2`]: () =>
        json({
          version: 2,
          published_at: "2026-10-05T09:00:00Z",
          content: { blocks: [heading("heading-1", "Old promise")] },
          warnings: [{ code: "image_without_alt", severity: "warning", at: "/blocks/block/x/alt" }],
        }),
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("tab", { name: "Versions" }));
    await user.click(await screen.findByRole("button", { name: "View version 2" }));

    const view = await screen.findByRole("region", { name: "Version 2" });
    expect(await within(view).findByText("Old promise")).toBeInTheDocument();
    expect(within(view).getByText(/An image has no alt text/)).toBeInTheDocument();
    expect(within(view).queryByRole("button", { name: /Add heading/ })).not.toBeInTheDocument();
  });

  it("shows where it is used, each linked and typed", async () => {
    mockApi(
      snippetRoutes({
        uses: usedBy({
          templates: [templateUse(1)],
          themes: [{ id: "th1", name: "Footer theme" }],
          active_flows: [{ flow_id: "f1", flow_name: "Welcome series", flow_state: "published", template_ids: ["t1"] }],
        }),
      }),
    );
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("tab", { name: "Where used" }));

    const list = await screen.findByRole("list", { name: "What uses this snippet" });
    expect(within(list).getByRole("link", { name: "Template 1" })).toHaveAttribute("href", "/w/ws1/templates/t1");
    expect(within(list).getByRole("link", { name: "Footer theme" })).toHaveAttribute("href", "/w/ws1/themes/th1");
    expect(within(list).getByRole("link", { name: "Welcome series" })).toHaveAttribute("href", "/w/ws1/flows/f1");
    expect(within(list).getByText("Active flow")).toBeInTheDocument();
  });
});

describe("publishing a snippet", () => {
  it("lists the first three users and keeps the rest behind a disclosure, then shows the new version", async () => {
    const calls = mockApi(
      snippetRoutes({ uses: usedBy({ templates: [1, 2, 3, 4, 5].map(templateUse) }) }),
    );
    const user = userEvent.setup();
    mount();

    await user.click(await publishButton());
    const dialog = await screen.findByRole("dialog", { name: "Publish Shipping promise?" });
    expect(within(dialog).getByText("Version 1 goes into every email that uses it, from its next send.")).toBeInTheDocument();
    const first = await within(dialog).findByRole("list", { name: "First users" });
    expect(within(first).getAllByRole("link")).toHaveLength(3);
    expect(within(dialog).queryByText("Template 4")).not.toBeInTheDocument();

    await user.click(within(dialog).getByRole("button", { name: "2 more" }));
    expect(within(dialog).getByText("Template 4")).toBeInTheDocument();
    expect(within(dialog).getByText("Template 5")).toBeInTheDocument();

    await user.click(within(dialog).getByRole("button", { name: "Publish" }));

    expect(await screen.findByText(/Published version 1/)).toBeInTheDocument();
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(screen.getByText("Draft · v1 live")).toBeInTheDocument();
    expect(calls.some((call) => call.method === "POST" && call.path.endsWith("/publish"))).toBe(true);
  });

  it("shows the issue list in the dialog when the server refuses", async () => {
    mockApi({
      ...snippetRoutes(),
      [`POST ${BASE}/publish`]: () =>
        refusal(422, {
          code: "snippet_invalid",
          template_issues: [{ code: "missing_content", severity: "blocking", at: "/blocks" }],
        }),
    });
    const user = userEvent.setup();
    mount();

    await user.click(await publishButton());
    const dialog = await screen.findByRole("dialog");
    await user.click(await within(dialog).findByRole("button", { name: "Publish" }));

    expect(await within(dialog).findByText("The template has no content yet.")).toBeInTheDocument();
    expect(within(dialog).getByText("Blocks publishing")).toBeInTheDocument();
  });

  it("lists the live templates the version would break, with links, and stays open", async () => {
    mockApi({
      ...snippetRoutes(),
      [`POST ${BASE}/publish`]: () =>
        refusal(422, {
          code: "snippet_breaks_templates",
          detail: "This snippet version would stop 25 live templates from sending; fix them or the version first.",
          total: 25,
          templates: [{ id: "t9", name: "Weekly news", codes: ["missing_unsubscribe"] }],
        }),
    });
    const user = userEvent.setup();
    mount();

    await user.click(await publishButton());
    const dialog = await screen.findByRole("dialog");
    await user.click(await within(dialog).findByRole("button", { name: "Publish" }));

    const link = await within(dialog).findByRole("link", { name: "Weekly news" });
    expect(link.getAttribute("href")).toMatch(/\/templates\/t9$/);
    expect(within(dialog).getByText("missing_unsubscribe")).toBeInTheDocument();
    expect(within(dialog).getByText(/would stop 25 live templates from sending/)).toBeInTheDocument();
    expect(within(dialog).getByText("and 24 more")).toBeInTheDocument();
    expect(within(dialog).getByRole("button", { name: "Cancel" })).toBeEnabled();
  });
});
