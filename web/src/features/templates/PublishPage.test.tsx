import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { json, mockApi, problem } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { SENDER_ID, templateDetail, templateRoutes, TEMPLATE_ID, useBody, WS, type Doc } from "../../test/templatesApi";
import { DEFAULT_STYLE, type Block } from "./blocks/document";
import { PublishPage } from "./PublishPage";

const WAIT = { timeout: 4000 };
const BASE = `${WS}/templates/${TEMPLATE_ID}`;
const PREVIEW = `POST ${BASE}/preview`;

function mount() {
  return renderRoute(`/w/ws1/templates/${TEMPLATE_ID}/publish`, "/w/:ws/templates/:id/publish", <PublishPage />);
}

function blocksDoc(...blocks: Block[]): Doc {
  return {
    schema: 2,
    channel: "email",
    mode: "blocks",
    blocks: {
      subject: "Hello",
      preheader: "",
      sender: SENDER_ID,
      category: "marketing",
      theme: "default",
      footer: "theme",
      settings: { ...DEFAULT_STYLE },
      sections: [{ id: "intro", columns: [blocks] }],
    },
  };
}

const heading = (text: string): Block => ({ type: "heading", id: "h1", level: 1, text });
const text = (content: string): Block => ({ type: "text", id: "t1", content });
const PAGE = "<html><head></head><body><p>Hi</p></body></html>";
const rendered = (extra: Doc = {}) =>
  json({ source: "draft", version: null, content_type: "html", person_id: null, rendered: true, subject: "Hello", html: PAGE, text: "Hi", headers: [], body: null, issues: [], hidden_blocks: [], ...extra });

const PEOPLE = ["p-1", "p-2", "p-3"].map((id, index) => ({
  person_id: id,
  display: { kind: "email", masked: false, value: `person${index + 1}@example.test` },
  created_at: "2026-10-26T00:00:00Z",
}));

function liveRoutes(draftDoc: Doc, liveDoc: Doc) {
  return {
    ...templateRoutes({ detail: templateDetail({ current_version: 1 }, draftDoc) }),
    [`GET ${BASE}/versions/1`]: () => json({ version: 1, document: liveDoc, warnings: [], published_by: null, published_at: "2026-10-05T09:00:00Z", note: null }),
  };
}

function publishButtons() {
  return screen.getAllByRole("button", { name: /^Publish version \d+$/ });
}

describe("the Review and publish page", () => {
  it("checks three saved people and the sample person, three at a time, and a blocking result disables Publish", async () => {
    let inFlight = 0;
    let peak = 0;
    const asked: string[] = [];
    mockApi({
      ...templateRoutes({ detail: templateDetail({}, blocksDoc(heading("Hi"))), saved: PEOPLE }),
      [PREVIEW]: async (request) => {
        const body = (await request.json()) as { person_id?: string };
        asked.push(body.person_id ?? "sample");
        inFlight += 1;
        peak = Math.max(peak, inFlight);
        await new Promise((resolve) => setTimeout(resolve, 20));
        inFlight -= 1;
        const issues = body.person_id === "p-2" ? [{ code: "invalid_url", severity: "blocking", at: "/blocks/block/h1/href" }] : [];
        return rendered({ issues });
      },
    });
    mount();

    expect(await screen.findByText("1 blocking issue must be fixed first.", undefined, WAIT)).toBeInTheDocument();
    expect([...asked].sort()).toEqual(["p-1", "p-2", "p-3", "sample"]);
    expect(peak).toBeLessThanOrEqual(3);
    expect(publishButtons().every((button) => button.hasAttribute("disabled"))).toBe(true);
    expect(screen.getByRole("link", { name: "Fix" })).toHaveAttribute("href", `/w/ws1/templates/${TEMPLATE_ID}?block=h1`);
  });

  it("says nothing blocks publishing when the checks pass, and keeps a warning with its Fix link", async () => {
    mockApi({
      ...templateRoutes({ detail: templateDetail({}, blocksDoc(heading("Hi"))) }),
      [PREVIEW]: () => rendered({ issues: [{ code: "image_without_alt", severity: "warning", at: "/blocks/block/h1/alt" }] }),
    });
    mount();

    expect(await screen.findByText("Nothing blocks publishing. 1 check passed.", undefined, WAIT)).toBeInTheDocument();
    expect(screen.getByText("An image has no alt text. Screen readers skip it.")).toBeInTheDocument();
    await waitFor(() => expect(publishButtons()[0]).toBeEnabled());
  });

  it("lists what changed since the live version and marks the selected change in the render", async () => {
    mockApi({ ...liveRoutes(blocksDoc(heading("Hi"), text("<p>New</p>")), blocksDoc(heading("Hi"))), [PREVIEW]: () => rendered() });
    const user = userEvent.setup();
    mount();

    expect(await screen.findByRole("heading", { name: "Changes since v1" })).toBeInTheDocument();
    const change = await screen.findByRole("button", { name: /Added/ });
    expect(change).toHaveTextContent("Section 1 › Text");
    expect(screen.getByRole("heading", { name: "Review and publish version 2" })).toBeInTheDocument();

    await user.click(change);

    await waitFor(() => expect(screen.getByTitle("Version 2 draft").getAttribute("srcdoc")).toContain(".zf-block-t1{outline:2px solid #2e9d5b"));
    expect(screen.getByTitle("Version 2 draft").getAttribute("srcdoc")).not.toContain("zf-block-h1");
    const document = new DOMParser().parseFromString(screen.getByTitle("Version 2 draft").getAttribute("srcdoc") ?? "", "text/html");
    expect(document.head.firstElementChild?.localName).toBe("meta");
    expect(document.head.firstElementChild?.getAttribute("http-equiv")).toBe("Content-Security-Policy");
    expect(await screen.findByTitle("Version 1")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "v2 only" }));
    expect(screen.queryByTitle("Version 1")).not.toBeInTheDocument();
  });

  it("shows the draft alone and First version when nothing is live, and publishes with a note", async () => {
    const calls = mockApi({
      ...templateRoutes({ detail: templateDetail({}, blocksDoc(heading("Hi"))), uses: [useBody()] }),
      [PREVIEW]: () => rendered(),
      [`GET ${BASE}/send-volume`]: () => json([{ flow_id: useBody().flow_id, flow_name: "Welcome series", per_day: 1140.4 }]),
      [`POST ${BASE}/publish`]: async (request) => {
        const body = (await request.json()) as { note?: string };
        return json(
          { template: templateDetail({ current_version: 1 }), version: { version: 1, document: {}, warnings: [], published_at: "2026-10-11T09:00:00Z", note: body.note ?? null }, used_by: [useBody()] },
          201,
        );
      },
    });
    const user = userEvent.setup();
    mount();

    expect(await screen.findByText("First version")).toBeInTheDocument();
    expect(screen.queryByRole("group", { name: "Compare" })).not.toBeInTheDocument();
    expect(await screen.findByText("about 1,140 a day")).toBeInTheDocument();
    await user.type(screen.getByLabelText("Note for the version history"), "  First cut ");
    await waitFor(() => expect(publishButtons()[0]).toBeEnabled(), WAIT);
    await user.click(publishButtons()[0] as HTMLElement);

    const done = await screen.findByRole("status", { name: "Published" });
    expect(within(done).getByRole("heading", { name: "Version 1 is live" })).toBeInTheDocument();
    expect(within(done).getByRole("link", { name: "Welcome series" })).toBeInTheDocument();
    expect(within(done).getByRole("link", { name: "View history" })).toHaveAttribute("href", `/w/ws1/templates/${TEMPLATE_ID}/history`);
    expect(within(done).getByRole("link", { name: "Back to the library" })).toBeInTheDocument();
    expect(calls.find((call) => call.method === "POST" && call.path.endsWith("/publish"))?.body).toEqual({ revision: 1, note: "First cut" });
  });

  it("shows the refusal of a publish and the newest test of this draft", async () => {
    const row = {
      id: "ts-1",
      template_id: TEMPLATE_ID,
      created_at: "2026-10-11T09:00:00Z",
      outcome: "sent",
      render_context: "sample",
      recipient_display: "q***@example.com",
      template_version: null,
      draft_revision: 1,
    };
    mockApi({
      ...templateRoutes({ detail: templateDetail({}, blocksDoc(heading("Hi"))) }),
      [PREVIEW]: () => rendered(),
      [`GET ${WS}/test-sends`]: () => json({ test_sends: [row], next_before: null }),
      [`POST ${BASE}/publish`]: () => problem(422, "Unprocessable", "The draft has blocking issues."),
    });
    const user = userEvent.setup();
    mount();

    expect(await screen.findByText(/To q\*\*\*@example.com as the sample person · sent/)).toBeInTheDocument();
    await waitFor(() => expect(publishButtons()[0]).toBeEnabled(), WAIT);
    await user.click(publishButtons()[0] as HTMLElement);

    expect(await screen.findByText("The draft has blocking issues.")).toBeInTheDocument();
  });

  it("refuses to call a changed draft live, says so, and Reload reviews the draft again", async () => {
    let reads = 0;
    const routes = templateRoutes({ detail: templateDetail({}, blocksDoc(heading("Hi"))) });
    const calls = mockApi({
      ...routes,
      [`GET ${BASE}`]: (request) => {
        reads += 1;
        return (routes[`GET ${BASE}`] as (request: Request) => Response | Promise<Response>)(request);
      },
      [PREVIEW]: () => rendered(),
      [`POST ${BASE}/publish`]: () => problem(409, "Conflict", "The draft was saved while it was being validated.", undefined, "template_draft_changed"),
    });
    const user = userEvent.setup();
    mount();

    await waitFor(() => expect(publishButtons()[0]).toBeEnabled(), WAIT);
    await user.click(publishButtons()[0] as HTMLElement);

    expect(await screen.findByText("The draft changed while you were reviewing it.")).toBeInTheDocument();
    expect(screen.queryByRole("heading", { name: /is live/ })).not.toBeInTheDocument();
    const before = reads;
    await user.click(screen.getByRole("button", { name: "Reload" }));
    await waitFor(() => expect(reads).toBeGreaterThan(before));
    await waitFor(() => expect(calls.filter((call) => call.path.endsWith("/preview")).length).toBeGreaterThan(1));
  });

  it("does not pass the checks when the saved people fail to load, and Retry loads them", async () => {
    let failing = true;
    const routes = templateRoutes({ detail: templateDetail({}, blocksDoc(heading("Hi"))), saved: PEOPLE });
    mockApi({
      ...routes,
      [PREVIEW]: () => rendered(),
      [`GET ${BASE}/preview-people`]: (request) => (failing ? problem(500, "Broken", "The people could not be read.") : (routes[`GET ${BASE}/preview-people`] as (request: Request) => Response)(request)),
    });
    const user = userEvent.setup();
    mount();

    expect(await screen.findByText("The saved people could not be loaded")).toBeInTheDocument();
    await screen.findByTitle("Version 1 draft");
    expect(screen.queryByText(/Nothing blocks publishing/)).not.toBeInTheDocument();
    expect(publishButtons().every((button) => button.hasAttribute("disabled"))).toBe(true);

    failing = false;
    await user.click(screen.getByRole("button", { name: "Retry" }));

    expect(await screen.findByText("Nothing blocks publishing. 4 checks passed.", undefined, WAIT)).toBeInTheDocument();
    await waitFor(() => expect(publishButtons()[0]).toBeEnabled());
  });

  it("shows the send volume failing in its own section, and Retry brings the counts", async () => {
    let failing = true;
    mockApi({
      ...templateRoutes({ detail: templateDetail({}, blocksDoc(heading("Hi"))), uses: [useBody()] }),
      [PREVIEW]: () => rendered(),
      [`GET ${BASE}/send-volume`]: () => (failing ? problem(500, "Broken", "The counts could not be read.") : json([{ flow_id: useBody().flow_id, flow_name: "Welcome series", per_day: 12 }])),
    });
    const user = userEvent.setup();
    mount();

    expect(await screen.findByText("Sends a day could not be loaded")).toBeInTheDocument();
    expect(screen.queryByText(/a day$/)).not.toBeInTheDocument();
    failing = false;
    await user.click(screen.getByRole("button", { name: "Retry" }));

    expect(await screen.findByText("about 12 a day")).toBeInTheDocument();
    expect(screen.queryByText("Sends a day could not be loaded")).not.toBeInTheDocument();
  });

  it("offers a member who is not an admin the sample person only, and no way to publish", async () => {
    const calls = mockApi({ ...templateRoutes({ role: "member", detail: templateDetail({}, blocksDoc(heading("Hi"))), saved: PEOPLE }), [PREVIEW]: () => rendered() });
    mount();

    expect(await screen.findByText("Only workspace admins can publish.")).toBeInTheDocument();
    await screen.findByTitle("Version 1 draft");
    expect(publishButtons().every((button) => button.hasAttribute("disabled"))).toBe(true);
    expect(calls.some((call) => call.path.endsWith("/preview-people"))).toBe(false);
  });
});
