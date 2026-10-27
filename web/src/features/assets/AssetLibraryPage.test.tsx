import { act, fireEvent, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { assetBody, FakeRequest, listBody, members } from "../../test/assetsApi";
import { json, MEMBER, mockApi, OWNER, problem, session } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { AssetLibraryPage } from "./AssetLibraryPage";

const WS = "/api/v1/workspaces/ws1";
const A = assetBody(1);
const B = assetBody(2, { name: "banner.jpg", content_type: "image/jpeg" });

function mount() {
  return renderRoute("/w/ws1/images", "/w/:ws/images", <AssetLibraryPage />);
}

function asRole(role: "admin" | "editor" | null, list = listBody([A, B]), more: Record<string, (r: Request) => Response> = {}) {
  const user = role === "admin" ? OWNER : MEMBER;
  return mockApi({
    "GET /api/v1/session": () => json(session(user, "csrf-img")),
    "GET /api/v1/workspaces/ws1/members": () => json(role === null ? [] : members(role, user.id)),
    [`GET ${WS}/assets`]: () => json(list),
    [`GET ${WS}/assets/${A.id}`]: () => json({ ...A, used_by: [] }),
    [`GET ${WS}/assets/${B.id}`]: () => json({ ...B, references: 2, used_by: [{ kind: "template", id: "t-1", version: 6, name: "Order shipped", archived: false }, { kind: "theme", id: "th-1", version: 3, name: null, archived: false }, { kind: "snippet", id: "sn-1", version: 1, name: "Footer", archived: true }] }),
    ...more,
  });
}

beforeEach(() => {
  FakeRequest.all = [];
  vi.stubGlobal("XMLHttpRequest", FakeRequest);
});

afterEach(() => {
  vi.unstubAllGlobals();
});

const PNG = () => new File(["x"], "new-logo.png", { type: "image/png" });

async function sendFile(file: File) {
  const input = await screen.findByLabelText("Choose an image file");
  fireEvent.change(input, { target: { files: [file] } });
}

describe("the image library", () => {
  it("shows the storage bar from the list response, the tiles and the library tabs", async () => {
    asRole("admin");
    mount();

    expect(await screen.findByText("212 MB of 1 GB used")).toBeInTheDocument();
    expect(screen.getByRole("meter", { name: "Storage used" })).toHaveAttribute("aria-valuenow", String(212 * 1024 * 1024));
    expect(screen.getByRole("button", { name: /photo-1\.png/ })).toHaveTextContent("640 × 160 · 38 KB");
    expect(screen.getByRole("link", { name: "Images" })).toHaveAttribute("href", "/w/ws1/images");
    expect(screen.getByRole("link", { name: "Snippets" })).toHaveAttribute("href", "/w/ws1/snippets");
  });

  it("searches by name", async () => {
    asRole("admin");
    const user = userEvent.setup();
    mount();

    await user.type(await screen.findByLabelText("Search images"), "banner");

    expect(screen.queryByRole("button", { name: /photo-1\.png/ })).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: /banner\.jpg/ })).toBeInTheDocument();
  });

  it("says so when there are no images, and when the list cannot be read", async () => {
    asRole("admin", listBody([]));
    mount();
    expect(await screen.findByText("No images yet")).toBeInTheDocument();
  });

  it("shows an error state when the list fails", async () => {
    asRole("admin", listBody([]), { [`GET ${WS}/assets`]: () => problem(500, "Broken", "Try later") });
    mount();
    expect(await screen.findByRole("alert")).toHaveTextContent("Broken");
  });

  it("pages with before when there is more", async () => {
    const calls = asRole("admin", listBody([A], { next_before: A.id }), {
      [`GET ${WS}/assets`]: (request) =>
        json(new URL(request.url).searchParams.has("before") ? listBody([B]) : listBody([A], { next_before: A.id })),
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Show more" }));

    expect(await screen.findByRole("button", { name: /banner\.jpg/ })).toBeInTheDocument();
    expect(calls.some((call) => call.search.includes(`before=${A.id}`))).toBe(true);
    expect(screen.queryByRole("button", { name: "Show more" })).not.toBeInTheDocument();
  });

  it("offers a reader neither upload nor delete", async () => {
    asRole(null);
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: /photo-1\.png/ }));

    expect(await screen.findByText("Used in")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Upload" })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Delete" })).not.toBeInTheDocument();
  });

  it("lets a member upload but not delete", async () => {
    asRole("editor");
    const user = userEvent.setup();
    mount();

    expect(await screen.findByRole("button", { name: "Upload" })).toBeEnabled();
    await user.click(screen.getByRole("button", { name: /photo-1\.png/ }));
    await screen.findByText("Used in");
    expect(screen.queryByRole("button", { name: "Delete" })).not.toBeInTheDocument();
  });
});

describe("uploading an image", () => {
  it("sends the raw file with its type and the CSRF token, shows progress, and selects the new image", async () => {
    asRole("admin");
    mount();

    await sendFile(PNG());

    const request = FakeRequest.all[0]!;
    expect(request.method).toBe("POST");
    expect(request.url).toBe(`${WS}/assets?name=new-logo.png`);
    expect(request.headers["content-type"]).toBe("image/png");
    expect(request.headers["x-csrf-token"]).toBe("csrf-img");
    act(() => request.progress(1, 2));
    expect(await screen.findByText("Uploading 50%")).toBeInTheDocument();
    const created = assetBody(3, { name: "new-logo.png" });
    act(() => request.respond(201, created));

    await waitFor(() => expect(screen.getByRole("button", { name: /new-logo\.png/ })).toHaveAttribute("aria-pressed", "true"));
    expect(screen.getByRole("dialog", { name: "new-logo.png" })).toBeInTheDocument();
  });

  it("selects the existing image when the same bytes were uploaded before", async () => {
    asRole("admin");
    mount();

    await sendFile(PNG());
    act(() => FakeRequest.all[0]!.respond(200, A));

    await waitFor(() => expect(screen.getByRole("button", { name: /photo-1\.png/ })).toHaveAttribute("aria-pressed", "true"));
    expect(screen.getAllByRole("button", { name: /photo-1\.png/ })).toHaveLength(1);
  });

  it("keeps the newest-first order when the same bytes of an older image are uploaded again", async () => {
    asRole("admin");
    mount();

    await sendFile(PNG());
    act(() => FakeRequest.all[0]!.respond(200, B));

    await waitFor(() => expect(screen.getByRole("button", { name: /banner\.jpg/ })).toHaveAttribute("aria-pressed", "true"));
    const names = Array.from(document.querySelectorAll(".image-grid .image-name")).map((node) => node.textContent);
    expect(names).toEqual(["photo-1.png", "banner.jpg"]);
  });

  it("sends the type from the extension when the browser gives none, and the browser's word otherwise", async () => {
    asRole("admin");
    mount();

    await sendFile(new File(["x"], "Pic.JPG", { type: "" }));
    expect(FakeRequest.all[0]!.headers["content-type"]).toBe("image/jpeg");
    act(() => FakeRequest.all[0]!.respond(200, A));
    await waitFor(() => expect(screen.getByRole("button", { name: "Upload" })).toBeEnabled());

    await sendFile(new File(["x"], "notes.txt", { type: "" }));
    expect(FakeRequest.all[1]!.headers["content-type"]).toBe("application/octet-stream");
  });

  it("cancels an upload in flight when the page goes away", async () => {
    asRole("admin");
    const view = mount();

    await sendFile(PNG());
    view.unmount();

    expect(FakeRequest.all[0]!.aborted).toBe(true);
  });

  it("refuses a file over 5 MiB without a request", async () => {
    asRole("admin");
    mount();
    const big = new File(["x"], "big.png", { type: "image/png" });
    Object.defineProperty(big, "size", { value: 5 * 1024 * 1024 + 1 });

    await sendFile(big);

    expect(await screen.findByRole("alert")).toHaveTextContent("That image is over 5 MB. Use a smaller one.");
    expect(FakeRequest.all).toHaveLength(0);
  });

  it.each([
    [415, "asset_type_unsupported", "Only PNG, JPEG, GIF and WebP images can be uploaded."],
    [415, "asset_type_mismatch", "The file is not the type it says it is."],
    [413, "asset_too_large", "That image is over 5 MB."],
    [413, "asset_quota_exceeded", "Your image storage is full."],
    [422, "asset_dimensions_exceeded", "That image is too large in pixels."],
  ])("explains a %i %s refusal in its own words", async (status, code, message) => {
    asRole("admin");
    mount();

    await sendFile(PNG());
    act(() => FakeRequest.all[0]!.respond(status, { type: "about:blank", title: "Refused", status, detail: "x", code }));

    expect(await screen.findByRole("alert")).toHaveTextContent(message);
  });
});

describe("the details sheet", () => {
  it("shows the facts, a shortened hash, the link to copy and where the image is used", async () => {
    asRole("admin");
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: /banner\.jpg/ }));

    const sheet = await screen.findByRole("dialog", { name: "banner.jpg" });
    expect(within(sheet).getByText("JPEG · 640 × 160")).toBeInTheDocument();
    expect(within(sheet).getByText(new Date("2026-10-12T09:00:00Z").toLocaleDateString())).toBeInTheDocument();
    expect(within(sheet).getByText(/^0202020…|^02020202…0202$/)).toBeInTheDocument();
    expect(within(sheet).getByRole("button", { name: "Copy link" })).toBeInTheDocument();
    expect(await within(sheet).findByRole("link", { name: "Order shipped" })).toHaveAttribute("href", "/w/ws1/templates/t-1");
    expect(within(sheet).getByText("template · v6")).toBeInTheDocument();
  });

  it("names each owner, marks an archived one, and falls back to the kind when the name is null", async () => {
    asRole("admin");
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: /banner\.jpg/ }));

    const sheet = await screen.findByRole("dialog", { name: "banner.jpg" });
    expect(await within(sheet).findByRole("link", { name: "Footer" })).toHaveAttribute("href", "/w/ws1/snippets/sn-1");
    expect(within(sheet).getByText("snippet · v1 · archived")).toBeInTheDocument();
    expect(within(sheet).getByRole("link", { name: "Theme" })).toHaveAttribute("href", "/w/ws1/themes/th-1");
    expect(within(sheet).getByText("theme · v3")).toBeInTheDocument();
    expect(within(sheet).queryByText("template · v6 · archived")).not.toBeInTheDocument();
  });

  it("reads where an image is used again after a delete race, so the sheet does not claim it is unused", async () => {
    let reads = 0;
    asRole("admin", listBody([A, B]), {
      [`GET ${WS}/assets/${A.id}`]: () => {
        reads += 1;
        return json({ ...A, used_by: reads > 1 ? [{ kind: "template", id: "t-9", version: 2, name: null, archived: false }] : [] });
      },
      [`DELETE ${WS}/assets/${A.id}`]: () => problem(409, "In use", "A published version uses it", undefined, "asset_in_use"),
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: /photo-1\.png/ }));
    expect(await screen.findByText("Nothing uses this image yet.")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Delete" }));
    await user.click(screen.getByRole("button", { name: "Delete image" }));

    expect(await screen.findByRole("link", { name: "Template" })).toHaveAttribute("href", "/w/ws1/templates/t-9");
    expect(screen.queryByText("Nothing uses this image yet.")).not.toBeInTheDocument();
  });

  it("disables Delete for an image in use and says why", async () => {
    asRole("admin");
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: /banner\.jpg/ }));

    const button = await screen.findByRole("button", { name: "Delete" });
    await waitFor(() => expect(button).toBeDisabled());
    expect(screen.getByText("Published versions use this image, so it can't be deleted.")).toBeInTheDocument();
  });

  it("deletes an unused image after a confirmation", async () => {
    const calls = asRole("admin", listBody([A, B]), { [`DELETE ${WS}/assets/${A.id}`]: () => new Response(null, { status: 204 }) });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: /photo-1\.png/ }));
    await user.click(await screen.findByRole("button", { name: "Delete" }));
    expect(calls.some((call) => call.method === "DELETE")).toBe(false);
    await user.click(screen.getByRole("button", { name: "Delete image" }));

    await waitFor(() => expect(calls.some((call) => call.method === "DELETE" && call.headers.get("x-csrf-token") === "csrf-img")).toBe(true));
    await waitFor(() => expect(screen.queryByRole("dialog", { name: "photo-1.png" })).not.toBeInTheDocument());
  });

  it("shows the same line when a delete loses the race to a publish", async () => {
    asRole("admin", listBody([A, B]), {
      [`DELETE ${WS}/assets/${A.id}`]: () => problem(409, "In use", "A published version uses it", undefined, "asset_in_use"),
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: /photo-1\.png/ }));
    await user.click(await screen.findByRole("button", { name: "Delete" }));
    await user.click(screen.getByRole("button", { name: "Delete image" }));

    expect(await screen.findByText("Published versions use this image, so it can't be deleted.")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Delete" })).toBeDisabled();
  });
});
