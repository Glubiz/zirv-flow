import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { json, mockApi, problem, type Call } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { emailDocument, previewBody, templateDetail, templateRoutes, TEMPLATE_ID, useBody, WS, type Doc } from "../../test/templatesApi";
import { HistoryPage } from "./HistoryPage";

const BASE = `${WS}/templates/${TEMPLATE_ID}`;
const DRAFT_DOC = emailDocument({}, { subject: "Draft subject" });
const LIVE_DOC = emailDocument({}, { subject: "Live subject" });
const OLD_DOC = emailDocument({}, { subject: "Old subject" });

function mount() {
  return renderRoute(`/w/ws1/templates/${TEMPLATE_ID}/history`, "/w/:ws/templates/:id/history", <HistoryPage />);
}

const SUMMARIES = [
  { version: 2, blocking: 0, warnings: 0, published_by: "u-member", note: "Shorter intro", published_at: "2026-10-14T09:12:00Z" },
  { version: 1, blocking: 0, warnings: 0, published_by: null, note: null, published_at: "2026-10-02T09:12:00Z" },
];

const PEOPLE = [{ person_id: "p-1", display: { kind: "email", masked: false, value: "mia@example.test" }, created_at: "2026-10-26T00:00:00Z" }];

interface Options {
  role?: "admin" | "member";
  versions?: unknown[];
  put?: (body: { document: Doc; revision?: number }, asked: number) => Response;
}

function routes({ role = "admin", versions = SUMMARIES, put }: Options = {}) {
  let asked = 0;
  return {
    ...templateRoutes({ role, saved: PEOPLE, detail: templateDetail({ current_version: 2 }, DRAFT_DOC), versions, preview: () => previewBody() }),
    [`GET ${BASE}/versions/2`]: () => json({ version: 2, document: LIVE_DOC, warnings: [], published_by: "u-member", published_at: "2026-10-14T09:12:00Z", note: "Shorter intro" }),
    [`GET ${BASE}/versions/1`]: () => json({ version: 1, document: OLD_DOC, warnings: [], published_by: null, published_at: "2026-10-02T09:12:00Z", note: null }),
    [`GET ${BASE}/versions/2/stats`]: () => json({ messages: 18240, renders: [{ theme_id: null, theme_version: null, snippet_versions: {}, count: 18240 }], more: false }),
    [`GET ${BASE}/versions/1/stats`]: () => json({ messages: 0, renders: [], more: false }),
    [`GET ${BASE}/where-used`]: () => json([useBody()]),
    [`GET ${WS}/snippets`]: () => json([]),
    ...(put
      ? {
          [`PUT ${BASE}/draft`]: async (request: Request) => {
            asked += 1;
            return put((await request.json()) as { document: Doc; revision?: number }, asked);
          },
        }
      : {}),
  };
}

const saved = (document: Doc, revision: number) => json({ ...templateDetail({ current_version: 2 }, document), draft: { ...templateDetail({}, document).draft, revision } });

const puts = (calls: Call[]) => calls.filter((call) => call.method === "PUT" && call.path.endsWith("/draft")).map((call) => call.body as { document: Doc; revision: number });
const previewsOf = (calls: Call[]) => calls.filter((call) => call.method === "POST" && call.path.endsWith("/preview")).map((call) => call.body as Record<string, unknown>);

describe("the Versions and use page", () => {
  it("lists the draft first, then each version with its note and author", async () => {
    mockApi(routes());
    mount();

    const rows = await screen.findAllByRole("button", { name: /^(Draft|Version \d)/ });
    expect(rows.map((row) => row.querySelector("b")?.textContent)).toEqual(["Draft", "Version 2", "Version 1"]);
    expect(within(rows[0] as HTMLElement).getByText("Editing")).toBeInTheDocument();
    expect(within(rows[0] as HTMLElement).getByText("1 change since v2")).toBeInTheDocument();
    expect(within(rows[1] as HTMLElement).getByText("Live")).toBeInTheDocument();
    expect(within(rows[1] as HTMLElement).getByText("Shorter intro")).toBeInTheDocument();
    expect(within(rows[1] as HTMLElement).getByText(/member@example\.test/)).toBeInTheDocument();
    expect(within(rows[2] as HTMLElement).getByText(/An API key/)).toBeInTheDocument();
  });

  it("renders a chosen version as the chosen person and says how many messages it sent", async () => {
    const calls = mockApi(routes());
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: /^Version 2/ }));
    await user.selectOptions(await screen.findByRole("combobox", { name: "Show the render for" }), "p-1");

    expect(await screen.findByRole("heading", { name: "Version 2 as mia@example.test gets it" })).toBeInTheDocument();
    expect(screen.getByText("Rendered with today's theme and snippets, as a send would be now.")).toBeInTheDocument();
    await waitFor(() => expect(previewsOf(calls)).toContainEqual(expect.objectContaining({ version: 2, person_id: "p-1" })));
    expect(await screen.findByText("18,240 messages")).toBeInTheDocument();
    expect(screen.getByRole("link", { name: /Welcome series/ })).toBeInTheDocument();
    expect(screen.getByText("Archiving is blocked while an active flow sends this template.")).toBeInTheDocument();
  });

  it("copies a version into the draft and puts the earlier draft back on Undo", async () => {
    const calls = mockApi(routes({ put: (body, asked) => saved(body.document, asked === 1 ? 2 : 3) }));
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: /^Version 1/ }));
    await user.click(await screen.findByRole("button", { name: "Copy into the draft" }));
    const banner = await screen.findByRole("status");
    expect(banner).toHaveTextContent("The draft now matches version 1");
    expect(banner).toHaveTextContent("Nothing was published");
    await user.click(within(banner).getByRole("button", { name: "Undo" }));

    await waitFor(() => expect(puts(calls)).toHaveLength(2));
    expect(puts(calls)).toEqual([
      { document: OLD_DOC, revision: 1 },
      { document: DRAFT_DOC, revision: 2 },
    ]);
    await waitFor(() => expect(screen.queryByText(/The draft now matches/)).not.toBeInTheDocument());
  });

  it("says someone else saved when the revision is stale, and offers a reload", async () => {
    const calls = mockApi(routes({ put: () => problem(409, "Conflict", "the draft was saved by someone else", undefined, "template_draft_revision_mismatch") }));
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: /^Version 1/ }));
    await user.click(await screen.findByRole("button", { name: "Copy into the draft" }));

    expect(await screen.findByText("Someone else saved the draft")).toBeInTheDocument();
    expect(screen.queryByText(/The draft now matches/)).not.toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Reload the draft" }));
    await waitFor(() => expect(screen.queryByText("Someone else saved the draft")).not.toBeInTheDocument());
    expect(calls.filter((call) => call.method === "GET" && call.path === BASE).length).toBeGreaterThan(1);
  });

  it("gives a writer who is not an admin the sample person only", async () => {
    const calls = mockApi(routes({ role: "member" }));
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: /^Version 2/ }));

    expect(await screen.findByRole("heading", { name: "Version 2 as the sample person gets it" })).toBeInTheDocument();
    expect(screen.queryByRole("combobox", { name: "Show the render for" })).not.toBeInTheDocument();
    expect(calls.some((call) => call.path.endsWith("/preview-people"))).toBe(false);
    await waitFor(() => expect(previewsOf(calls).length).toBeGreaterThan(0));
    expect(previewsOf(calls).every((body) => body["person_id"] === undefined)).toBe(true);
  });

  it("says no versions are published yet and still shows the draft, its facts and its flows", async () => {
    const calls = mockApi(routes({ versions: [] }));
    mount();

    expect(await screen.findByText(/No published versions yet/)).toBeInTheDocument();
    expect(await screen.findByRole("heading", { name: "Draft as the sample person gets it" })).toBeInTheDocument();
    expect(await screen.findByRole("heading", { name: "Draft" })).toBeInTheDocument();
    expect(screen.getByRole("link", { name: /Welcome series/ })).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Copy into the draft" })).not.toBeInTheDocument();
    await waitFor(() => expect(previewsOf(calls).length).toBeGreaterThan(0));
    expect(calls.some((call) => call.path.endsWith("/stats"))).toBe(false);
  });
});
