import { fireEvent, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeAll, describe, expect, it, vi } from "vitest";

import { json, mockApi } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { contrastIssue, previewBody, previews, saves, templateUse, themeDetail, themeRoutes, THEME_ID, tokens, usedBy, WS } from "../../test/themesApi";
import { ThemePage } from "./ThemePage";

const WAIT = { timeout: 4000 };
const BASE = `${WS}/themes/${THEME_ID}`;

beforeAll(() => {
  Element.prototype.scrollIntoView = vi.fn();
});

function mount() {
  return renderRoute(`/w/ws1/themes/${THEME_ID}`, "/w/:ws/themes/:id", <ThemePage />);
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

describe("the theme editor", () => {
  it("shows the title bar, the settings in order and the sandboxed preview", async () => {
    mockApi(themeRoutes({ detail: themeDetail({ current_version: 3, is_default: true }) }));
    mount();

    expect(await screen.findByRole("link", { name: "← Themes" })).toHaveAttribute("href", "/w/ws1/themes");
    expect(screen.getByRole("heading", { level: 1, name: "Warm" })).toBeInTheDocument();
    expect(screen.getByText("Default")).toBeInTheDocument();
    expect(screen.getByText("Draft · v3 live")).toBeInTheDocument();
    for (const name of ["Editor", "Versions", "Where used"]) expect(screen.getByRole("tab", { name })).toBeInTheDocument();
    const sections = screen.getAllByRole("heading", { level: 2 }).map((heading) => heading.textContent);
    expect(sections.slice(0, 5)).toEqual(["Logo", "Colours", "Text and layout", "Buttons and links", "Footer"]);

    const frame = await screen.findByTitle("Preview of the theme");
    expect(frame).toHaveAttribute("sandbox", "");
    const document = new DOMParser().parseFromString(frame.getAttribute("srcdoc") ?? "", "text/html");
    expect(document.head.firstElementChild?.localName).toBe("meta");
    expect(document.head.firstElementChild?.getAttribute("http-equiv")).toBe("Content-Security-Policy");
    expect(document.body.innerHTML).toBe("<p>A heading</p>");
    expect(screen.getByRole("button", { name: "Mobile" })).toBeInTheDocument();
  });

  it("saves a colour change after the debounce with the revision it read, then asks for the preview again", async () => {
    const calls = mockApi(themeRoutes());
    mount();
    await screen.findByTitle("Preview of the theme");
    const before = previews(calls).length;

    fireEvent.change(screen.getByLabelText("Primary"), { target: { value: "#112233" } });

    await waitFor(() => expect(saves(calls)).toHaveLength(1), WAIT);
    const sent = saves(calls)[0];
    expect(sent?.revision).toBe(1);
    expect(sent?.tokens).toMatchObject({ primary: "#112233", web_font_url: null });
    await waitFor(() => expect(previews(calls).length).toBeGreaterThan(before), WAIT);
  });

  it("holds an invalid hex locally and never saves it", async () => {
    const calls = mockApi(themeRoutes());
    mount();
    await screen.findByTitle("Preview of the theme");
    const field = screen.getByLabelText("Text");

    fireEvent.change(field, { target: { value: "#12" } });

    expect(field).toHaveValue("#12");
    expect(field).toHaveAttribute("aria-invalid", "true");
    await new Promise((resolve) => setTimeout(resolve, 700));
    expect(saves(calls)).toHaveLength(0);
  });

  it("writes the corner style as 0, 6 or 40 and selects none for another stored radius", async () => {
    const calls = mockApi(themeRoutes({ detail: themeDetail({}, tokens({ button_radius: 12 })) }));
    const user = userEvent.setup();
    mount();
    const group = await screen.findByRole("group", { name: "Button corners" });

    for (const name of ["Square", "Rounded", "Pill"]) expect(within(group).getByRole("button", { name })).toHaveAttribute("aria-pressed", "false");
    await user.click(within(group).getByRole("button", { name: "Pill" }));
    await waitFor(() => expect(saves(calls)).toHaveLength(1), WAIT);
    await user.click(within(group).getByRole("button", { name: "Square" }));
    await waitFor(() => expect(saves(calls)).toHaveLength(2), WAIT);

    expect(saves(calls).map((sent) => sent.tokens["button_radius"])).toEqual([40, 0]);
    expect(within(group).getByRole("button", { name: "Square" })).toHaveAttribute("aria-pressed", "true");
  });

  it("puts a contrast warning under its pair, keeps Publish enabled and counts it in the title bar", async () => {
    mockApi(themeRoutes({ preview: () => previewBody({ issues: [contrastIssue("/theme/button_text")] }) }));
    const user = userEvent.setup();
    mount();

    const note = await screen.findByText("Button text on Button is 4.2:1. Aim for 4.5:1.", undefined, WAIT);
    const buttonText = screen.getByLabelText("Button text");
    expect(buttonText.compareDocumentPosition(note) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
    expect(screen.getByText("Mail apps in dark mode can still change these colours.")).toBeInTheDocument();
    expect(await publishButton()).toBeEnabled();

    await user.click(screen.getByRole("button", { name: "1 warning" }));
    expect(note).toHaveFocus();
  });

  it("disables Publish for a blocking issue and says why", async () => {
    mockApi(themeRoutes({ preview: () => previewBody({ rendered: false, html: null, issues: [{ code: "invalid_setting", severity: "blocking", at: "/theme/content_width" }] }) }));
    mount();

    const publish = await screen.findByRole("button", { name: "Publish…" });
    expect(await screen.findByText("1 blocking issue must be fixed first.", undefined, WAIT)).toBeInTheDocument();
    expect(publish).toBeDisabled();
  });

  it("stops autosaving and offers a reload when someone else saved", async () => {
    const calls = mockApi(themeRoutes({ save: () => refusal(409, { code: "theme_revision_mismatch" }) }));
    mount();
    await screen.findByTitle("Preview of the theme");

    fireEvent.change(screen.getByLabelText("Primary"), { target: { value: "#112233" } });
    const banner = await screen.findByRole("alert", undefined, WAIT);
    expect(banner).toHaveTextContent("Someone else saved this theme");
    expect(within(banner).getByRole("button", { name: "Reload" })).toBeInTheDocument();
    fireEvent.change(screen.getByLabelText("Link"), { target: { value: "#445566" } });
    await new Promise((resolve) => setTimeout(resolve, 800));

    expect(saves(calls)).toHaveLength(1);
  });

  it("shows an error state, not an old render, when the preview fails", async () => {
    let asked = 0;
    mockApi(themeRoutes({ preview: () => (++asked === 1 ? previewBody() : refusal(500, { detail: "The renderer is down." })) }));
    mount();
    await screen.findByTitle("Preview of the theme");

    fireEvent.change(screen.getByLabelText("Primary"), { target: { value: "#112233" } });

    expect(await screen.findByText("The preview could not be rendered", undefined, WAIT)).toBeInTheDocument();
    expect(screen.queryByTitle("Preview of the theme")).not.toBeInTheDocument();
  });

  it("offers footer snippets that are published, with a link to the chosen one", async () => {
    const footer = "88888888-0000-4000-8000-000000000001";
    mockApi({
      ...themeRoutes({ detail: themeDetail({}, tokens({ footer_snippet: footer })) }),
      [`GET ${WS}/snippets`]: () => json([{ id: footer, name: "Sign-off", current_version: 2 }, { id: "x", name: "Unpublished", current_version: null }]),
    });
    mount();

    const select = await screen.findByLabelText("Footer snippet");
    await waitFor(() => expect(within(select).getByRole("option", { name: "Sign-off" })).toBeInTheDocument());
    expect(within(select).getByRole("option", { name: "No footer" })).toBeInTheDocument();
    expect(within(select).queryByRole("option", { name: "Unpublished" })).not.toBeInTheDocument();
    expect(screen.getByRole("link", { name: "Open snippet" })).toHaveAttribute("href", `/w/ws1/snippets/${footer}`);
  });
});

describe("roles", () => {
  it("hides publish, set as default and archive from a writer but keeps Save draft", async () => {
    mockApi(themeRoutes({ role: "editor" }));
    const user = userEvent.setup();
    mount();

    await screen.findByTitle("Preview of the theme");
    expect(screen.queryByRole("button", { name: "Publish…" })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /More actions/ })).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Save draft" })).toBeInTheDocument();
    await user.click(screen.getByRole("tab", { name: "Where used" }));
  });

  it("shows a viewer no write control and no preview", async () => {
    const calls = mockApi(themeRoutes({ role: "reader" }));
    mount();

    expect(await screen.findByRole("heading", { level: 1, name: "Warm" })).toBeInTheDocument();
    expect(screen.getByText("Previewing a theme needs the editor role.")).toBeInTheDocument();
    for (const name of ["Publish…", "Save draft"]) expect(screen.queryByRole("button", { name })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /More actions/ })).not.toBeInTheDocument();
    expect(previews(calls)).toHaveLength(0);
  });

  it("shows a viewer a version's tokens without asking for a preview", async () => {
    const calls = mockApi({
      ...themeRoutes({ role: "reader", versions: [{ version: 1, published_at: "2026-10-05T09:00:00Z", warnings: 0, published_by: null }] }),
      [`GET ${BASE}/versions/1`]: () => json({ version: 1, published_at: "2026-10-05T09:00:00Z", tokens: tokens(), warnings: [] }),
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("tab", { name: "Versions" }));
    await user.click(await screen.findByRole("button", { name: "View version 1" }));

    const view = await screen.findByRole("region", { name: "Version 1" });
    expect(await within(view).findByLabelText("Tokens of this version")).toBeInTheDocument();
    expect(within(view).queryByTitle("Preview of version 1")).not.toBeInTheDocument();
    expect(previews(calls)).toHaveLength(0);
  });

  it("gives an admin the menu, with Archive disabled on the default theme", async () => {
    mockApi(themeRoutes({ detail: themeDetail({ is_default: true, current_version: 1 }) }));
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "More actions for Warm" }));

    expect(screen.getByRole("menuitem", { name: /Archive/ })).toHaveAttribute("aria-disabled", "true");
  });
});

describe("publishing a theme", () => {
  it("names the templates that pick it up, then shows the new version", async () => {
    const calls = mockApi(
      themeRoutes({ uses: usedBy({ named_templates: [templateUse(1)], default_templates: [templateUse(2), templateUse(3), templateUse(4), templateUse(5)] }) }),
    );
    const user = userEvent.setup();
    mount();

    await user.click(await publishButton());
    const dialog = await screen.findByRole("dialog", { name: "Publish Warm?" });
    expect(within(dialog).getByText(/Version 1 is used by every email that follows this theme/)).toBeInTheDocument();
    expect(await within(dialog).findByText("5 templates pick it up")).toBeInTheDocument();
    expect(within(await within(dialog).findByRole("list", { name: "First templates" })).getAllByRole("link")).toHaveLength(3);
    await user.click(within(dialog).getByRole("button", { name: "2 more" }));
    expect(within(dialog).getByText("Template 5")).toBeInTheDocument();

    await user.click(within(dialog).getByRole("button", { name: "Publish" }));

    expect(await screen.findByText(/Published version 1/)).toBeInTheDocument();
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(screen.getByText("Draft · v1 live")).toBeInTheDocument();
    expect(calls.some((call) => call.method === "POST" && call.path === `${BASE}/publish`)).toBe(true);
  });

  it("shows the issues in the dialog when the server refuses", async () => {
    mockApi({
      ...themeRoutes(),
      [`POST ${BASE}/publish`]: () => refusal(422, { code: "theme_invalid", template_issues: [{ code: "invalid_setting", severity: "blocking", at: "/theme/primary" }] }),
    });
    const user = userEvent.setup();
    mount();

    await user.click(await publishButton());
    const dialog = await screen.findByRole("dialog");
    await user.click(await within(dialog).findByRole("button", { name: "Publish" }));

    expect(await within(dialog).findByText("Blocks publishing")).toBeInTheDocument();
  });

  it("lists the live templates the version would break, with links, and stays open", async () => {
    mockApi({
      ...themeRoutes(),
      [`POST ${BASE}/publish`]: () =>
        refusal(422, {
          code: "theme_breaks_templates",
          detail: "This theme version would stop 25 live templates from sending; fix them or the version first.",
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

describe("versions and where used", () => {
  it("lists the versions and shows one read only with its preview", async () => {
    const calls = mockApi({
      ...themeRoutes({ versions: [{ version: 2, published_at: "2026-10-05T09:00:00Z", warnings: 1, published_by: null }] }),
      [`GET ${BASE}/versions/2`]: () =>
        json({ version: 2, published_at: "2026-10-05T09:00:00Z", tokens: tokens({ primary: "#abcdef" }), warnings: [contrastIssue("/theme/text")] }),
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("tab", { name: "Versions" }));
    await user.click(await screen.findByRole("button", { name: "View version 2" }));

    const view = await screen.findByRole("region", { name: "Version 2" });
    expect(await within(view).findByText("#abcdef")).toBeInTheDocument();
    const frame = await within(view).findByTitle("Preview of version 2");
    expect(frame).toHaveAttribute("sandbox", "");
    expect(within(view).queryByRole("textbox")).not.toBeInTheDocument();
    expect(previews(calls).some((call) => (call.body as { version?: number }).version === 2)).toBe(true);
  });

  it("lists the templates that name it, those that follow the default and the active flows, each linked", async () => {
    mockApi(
      themeRoutes({
        uses: usedBy({
          named_templates: [templateUse(1)],
          default_templates: [templateUse(2)],
          active_flows: [{ flow_id: "f1", flow_name: "Welcome series", flow_state: "published", template_ids: ["t2"] }],
        }),
      }),
    );
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("tab", { name: "Where used" }));

    const named = await screen.findByRole("list", { name: "Templates that name this theme" });
    expect(within(named).getByRole("link", { name: "Template 1" })).toHaveAttribute("href", "/w/ws1/templates/t1");
    const follows = screen.getByRole("list", { name: "Templates that follow the default" });
    expect(within(follows).getByRole("link", { name: "Template 2" })).toHaveAttribute("href", "/w/ws1/templates/t2");
    const flows = screen.getByRole("list", { name: "Active flows" });
    expect(within(flows).getByRole("link", { name: "Welcome series" })).toHaveAttribute("href", "/w/ws1/flows/f1");
  });
});
