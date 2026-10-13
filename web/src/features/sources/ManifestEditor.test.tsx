import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useState } from "react";
import { describe, expect, it, vi } from "vitest";

import { SessionProvider, useSession } from "../session";
import { json, mockApi, OWNER, problem, session } from "../../test/mockApi";
import { ManifestEditor } from "./ManifestEditor";
import { issueText } from "./labels";
import { starterManifest } from "./manifest";
import { aiSettings } from "./testData";

function Editor({ initial, sourceId }: { initial: string; sourceId?: string }) {
  const [value, setValue] = useState(initial);
  const { state } = useSession();
  if (state.status !== "signed-in") return <p>Waiting for the session</p>;
  return <ManifestEditor label="Manifest" value={value} onChange={setValue} workspaceId="ws1" sourceId={sourceId} />;
}

function mount(initial: string, sourceId?: string) {
  return render(
    <SessionProvider>
      <Editor initial={initial} sourceId={sourceId} />
    </SessionProvider>,
  );
}

const VALID = {
  valid: true,
  issues: [],
  streams: [
    {
      name: "contacts",
      fields: [
        { name: "id", type: "string" },
        { name: "email", type: "string" },
      ],
    },
  ],
};
const BASE = {
  "GET /api/v1/session": () => json(session(OWNER, "csrf-m")),
  "GET /api/v1/workspaces/ws1/ai/settings": () => json(aiSettings(false)),
};
const VALIDATE = "POST /api/v1/workspaces/ws1/source-manifests/validate";
const SAMPLE = "POST /api/v1/workspaces/ws1/sources/src-1/manifest/sample";

describe("the manifest editor", () => {
  it("checks the manifest after a pause and says it is valid", async () => {
    const calls = mockApi({ ...BASE, [VALIDATE]: () => json(VALID) });
    mount(starterManifest("none"));

    expect(await screen.findByText("The manifest is valid")).toBeInTheDocument();

    const sent = calls.find((call) => call.path.endsWith("/validate"));
    expect((sent?.body as { manifest: { version: string } }).manifest.version).toBe("v1");
    expect(screen.queryByRole("list", { name: "Problems in the manifest" })).not.toBeInTheDocument();
  });

  it("shows each problem as a sentence with its path, and a click puts the caret on its line", async () => {
    const text = starterManifest("none");
    mockApi({
      ...BASE,
      [VALIDATE]: () =>
        json({
          valid: false,
          issues: [
            { code: "path_invalid", path: "/streams/0/path" },
            { code: "unknown_field", path: "/streams/0/extra" },
          ],
          streams: [],
        }),
    });
    const user = userEvent.setup();
    mount(text);

    const list = await screen.findByRole("list", { name: "Problems in the manifest" });
    expect(within(list).getByText(issueText("path_invalid"))).toBeInTheDocument();
    expect(within(list).getByText("/streams/0/path")).toBeInTheDocument();
    await user.click(within(list).getByRole("button", { name: issueText("path_invalid") }));

    const area = screen.getByLabelText<HTMLTextAreaElement>("Manifest");
    const line = text.slice(0, area.selectionStart).split("\n").length;
    const lines = text.split("\n");
    expect(lines[line - 1]).toContain('"path": "/contacts"');
    expect(area).toHaveFocus();
  });

  it("reports a syntax problem by line without asking the server, and never shows the text", async () => {
    const calls = mockApi({ ...BASE, [VALIDATE]: () => json(VALID) });
    mount('{\n  "token": "SECRET-value",\n  "x": ]\n}');

    const list = await screen.findByRole("list", { name: "Problems in the manifest" });
    expect(within(list).getByText(issueText("syntax_error"))).toBeInTheDocument();
    expect(within(list).getByText(/Line 3, column 8/)).toBeInTheDocument();
    expect(list.textContent).not.toContain("SECRET");
    await new Promise((resolve) => setTimeout(resolve, 600));
    expect(calls.filter((call) => call.path.endsWith("/validate"))).toHaveLength(0);
  });

  it("offers starters on an empty editor and fills one in", async () => {
    mockApi({ ...BASE, [VALIDATE]: () => json(VALID) });
    const user = userEvent.setup();
    mount("");

    expect(await screen.findByText("Nothing yet")).toBeInTheDocument();
    await user.selectOptions(await screen.findByLabelText("Start from"), "bearer");

    const area = screen.getByLabelText<HTMLTextAreaElement>("Manifest");
    expect(area.value).toContain('"type": "bearer"');
    expect(screen.queryByLabelText("Start from")).not.toBeInTheDocument();
  });

  it("formats the text", async () => {
    mockApi({ ...BASE, [VALIDATE]: () => json(VALID) });
    const user = userEvent.setup();
    mount('{"version":"v1"}');

    await user.click(await screen.findByRole("button", { name: "Format" }));

    expect((screen.getByLabelText<HTMLTextAreaElement>("Manifest")).value).toBe('{\n  "version": "v1"\n}');
  });

  it("says when the manifest could not be checked", async () => {
    mockApi({ ...BASE, [VALIDATE]: () => problem(503, "Unavailable", "try later") });
    mount(starterManifest("none"));

    expect(await screen.findByText("The manifest could not be checked")).toBeInTheDocument();
  });

  it("asks to save the source before it can read a sample", async () => {
    mockApi({ ...BASE, [VALIDATE]: () => json(VALID) });
    mount(starterManifest("none"));

    expect(await screen.findByText("Save the source to read a sample with its credential.")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Test with sample" })).not.toBeInTheDocument();
  });
});

describe("the sample", () => {
  const ready = { ...BASE, [VALIDATE]: () => json(VALID) };

  it("is idle until asked, then shows the raw records of the declared fields", async () => {
    const calls = mockApi({
      ...ready,
      [SAMPLE]: () =>
        json({
          records: [
            { id: "c1", email: "a@example.com", hidden: "not declared" },
            { id: "c2", email: null },
          ],
          read: 2,
          truncated: true,
        }),
    });
    const user = userEvent.setup();
    mount(starterManifest("none"), "src-1");

    expect(await screen.findByText(/Reads a few records with the saved credential/)).toBeInTheDocument();
    const test = await screen.findByRole("button", { name: "Test with sample" });
    await vi.waitFor(() => expect(test).toBeEnabled());
    await user.selectOptions(screen.getByLabelText("Records"), "5");
    await user.click(test);

    const table = await screen.findByRole("table", { name: "Sample records" });
    expect(within(table).getAllByRole("columnheader").map((header) => header.textContent)).toEqual(["id", "email"]);
    expect(within(table).getByText("a@example.com")).toBeInTheDocument();
    expect(within(table).queryByText("not declared")).not.toBeInTheDocument();
    expect(screen.getByText(/Raw values, as the API returned them/)).toBeInTheDocument();
    expect(screen.getByText(/more records than shown/)).toBeInTheDocument();
    const sent = calls.find((call) => call.path.endsWith("/manifest/sample"));
    expect(sent?.body).toMatchObject({ stream: "contacts", limit: 5 });
  });

  it("says the API returned nothing", async () => {
    mockApi({ ...ready, [SAMPLE]: () => json({ records: [], read: 0, truncated: false }) });
    const user = userEvent.setup();
    mount(starterManifest("none"), "src-1");

    const test = await screen.findByRole("button", { name: "Test with sample" });
    await vi.waitFor(() => expect(test).toBeEnabled());
    await user.click(test);

    expect(await screen.findByText("The API returned no records")).toBeInTheDocument();
  });

  it("shows the failure when the API refuses", async () => {
    mockApi({ ...ready, [SAMPLE]: () => problem(502, "Bad Gateway", "the source refused the credential") });
    const user = userEvent.setup();
    mount(starterManifest("none"), "src-1");

    const test = await screen.findByRole("button", { name: "Test with sample" });
    await vi.waitFor(() => expect(test).toBeEnabled());
    await user.click(test);

    expect(await screen.findByText("the source refused the credential")).toBeInTheDocument();
  });

  it("cannot be asked for while the manifest has problems", async () => {
    mockApi({
      ...BASE,
      [VALIDATE]: () => json({ valid: false, issues: [{ code: "required", path: "/version" }], streams: [] }),
    });
    mount(starterManifest("none"), "src-1");

    await screen.findByRole("list", { name: "Problems in the manifest" });
    expect(screen.getByRole("button", { name: "Test with sample" })).toBeDisabled();
    expect(screen.getByLabelText("Stream to read")).toBeDisabled();
  });

  it("replaces the text with an accepted draft", async () => {
    const manifest = JSON.parse(starterManifest("none")) as Record<string, unknown>;
    manifest["base_url"] = "https://drafted.example.test";
    mockApi({
      ...BASE,
      "GET /api/v1/workspaces/ws1/ai/settings": () => json(aiSettings(true)),
      [VALIDATE]: () => json(VALID),
      "POST /api/v1/workspaces/ws1/ai/drafts": () =>
        json({ id: "d1", kind: "manifest", status: "valid", attempts: 1, content: manifest, issues: [], changes: [], dry_run: null }),
      "POST /api/v1/workspaces/ws1/ai/drafts/d1/accept": () =>
        json({ id: "d1", kind: "manifest", source_id: null, stream: null, content: manifest }),
    });
    const user = userEvent.setup();
    mount(starterManifest("none"));

    await user.type(await screen.findByLabelText("What do you want?"), "Read my contacts");
    await user.click(screen.getByRole("button", { name: "Draft" }));
    await user.click(await screen.findByRole("button", { name: "Accept" }));

    expect(await screen.findByText("Loaded into the editor. Save to apply it.")).toBeInTheDocument();
    expect(screen.getByLabelText<HTMLTextAreaElement>("Manifest").value).toContain("https://drafted.example.test");
  });
});
