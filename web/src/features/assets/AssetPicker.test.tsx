import { act, fireEvent, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter, Route, Routes } from "react-router";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { assetBody, FakeRequest, listBody, members } from "../../test/assetsApi";
import { json, MEMBER, mockApi, OWNER, problem, session } from "../../test/mockApi";
import { SessionProvider, useSession } from "../session";
import { AssetPicker, httpsAddress } from "./AssetPicker";

const WS = "/api/v1/workspaces/ws1";
const A = assetBody(1);
const B = assetBody(2, { name: "banner.jpg" });

function Gate({ children }: { children: React.ReactNode }) {
  const { state } = useSession();
  return state.status === "signed-in" ? <>{children}</> : <p>Waiting</p>;
}

function mount(props: { themeLogo?: boolean; role?: "admin" | "member" | null; list?: unknown } = {}) {
  const role = props.role === undefined ? "member" : props.role;
  const user = role === "admin" ? OWNER : MEMBER;
  mockApi({
    "GET /api/v1/session": () => json(session(user, "csrf-pick")),
    "GET /api/v1/workspaces/ws1/members": () => json(role === null ? [] : members(role, user.id)),
    [`GET ${WS}/assets`]: (request) =>
      new URL(request.url).searchParams.has("before") ? problem(500, "Broken", "Try later") : json(props.list ?? listBody([A, B])),
  });
  const onPick = vi.fn();
  const onClose = vi.fn();
  const view = render(
    <MemoryRouter initialEntries={["/w/ws1/x"]}>
      <SessionProvider>
        <Gate>
          <Routes>
            <Route path="/w/:ws/x" element={<AssetPicker open onPick={onPick} onClose={onClose} themeLogo={props.themeLogo} />} />
          </Routes>
        </Gate>
      </SessionProvider>
    </MemoryRouter>,
  );
  return { onPick, onClose, unmount: view.unmount };
}

beforeEach(() => {
  FakeRequest.all = [];
  vi.stubGlobal("XMLHttpRequest", FakeRequest);
});

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("the asset picker", () => {
  it("returns the chosen image's id and address", async () => {
    const { onPick } = mount();
    const user = userEvent.setup();

    await user.click(await screen.findByRole("radio", { name: /banner\.jpg/ }));
    await user.click(screen.getByRole("button", { name: "Use image" }));

    expect(onPick).toHaveBeenCalledWith({ asset: { id: B.id, url: B.url } });
  });

  it("will not use an image before one is chosen, and Cancel closes", async () => {
    const { onPick, onClose } = mount();
    const user = userEvent.setup();

    expect(await screen.findByRole("button", { name: "Use image" })).toBeDisabled();
    await user.click(screen.getByRole("button", { name: "Cancel" }));

    expect(onClose).toHaveBeenCalled();
    expect(onPick).not.toHaveBeenCalled();
  });

  it("chooses with the arrow keys", async () => {
    const { onPick } = mount();
    const user = userEvent.setup();

    const first = await screen.findByRole("radio", { name: /photo-1\.png/ });
    first.focus();
    await user.keyboard("{ArrowRight}");
    expect(screen.getByRole("radio", { name: /banner\.jpg/ })).toBeChecked();
    await user.click(screen.getByRole("button", { name: "Use image" }));

    expect(onPick).toHaveBeenCalledWith({ asset: { id: B.id, url: B.url } });
  });

  it("filters by name", async () => {
    mount();
    const user = userEvent.setup();

    await user.type(await screen.findByLabelText("Search images"), "photo");

    expect(screen.queryByRole("radio", { name: /banner\.jpg/ })).not.toBeInTheDocument();
    expect(screen.getByRole("radio", { name: /photo-1\.png/ })).toBeInTheDocument();
  });

  it("uploads from the first tile and selects the new image", async () => {
    const { onPick } = mount();
    const user = userEvent.setup();

    expect(await screen.findByRole("button", { name: /Upload/ })).toBeInTheDocument();
    fireEvent.change(screen.getByLabelText("Choose an image file"), {
      target: { files: [new File(["x"], "fresh.png", { type: "image/png" })] },
    });
    const created = assetBody(3, { name: "fresh.png" });
    act(() => FakeRequest.all[0]!.respond(201, created));

    expect(await screen.findByRole("radio", { name: /fresh\.png/ })).toBeChecked();
    await user.click(screen.getByRole("button", { name: "Use image" }));
    expect(onPick).toHaveBeenCalledWith({ asset: { id: created.id, url: created.url } });
  });

  it("has no Upload tile for a reader", async () => {
    mount({ role: null });

    await screen.findByRole("radio", { name: /photo-1\.png/ });
    expect(screen.queryByRole("button", { name: /Upload/ })).not.toBeInTheDocument();
  });

  it("shows the Theme logo tile only when asked, and returns it", async () => {
    const { onPick } = mount({ themeLogo: true });
    const user = userEvent.setup();

    await user.click(await screen.findByRole("radio", { name: /Theme logo/ }));
    await user.click(screen.getByRole("button", { name: "Use image" }));

    expect(onPick).toHaveBeenCalledWith({ themeLogo: true });
  });

  it("has no Theme logo tile without themeLogo", async () => {
    mount();

    await screen.findByRole("radio", { name: /photo-1\.png/ });
    expect(screen.queryByRole("radio", { name: /Theme logo/ })).not.toBeInTheDocument();
  });

  it("takes a web address only when it is https", async () => {
    const { onPick } = mount();
    const user = userEvent.setup();

    await user.click(await screen.findByRole("button", { name: "Use a web address" }));
    const field = screen.getByLabelText("Web address");
    await user.type(field, "http://example.test/a.png");
    expect(screen.getByRole("button", { name: "Use image" })).toBeDisabled();
    expect(screen.getByText("Use an address that starts with https://")).toBeInTheDocument();
    await user.clear(field);
    await user.type(field, "https://example.test/a.png");
    await user.click(screen.getByRole("button", { name: "Use image" }));

    expect(onPick).toHaveBeenCalledWith({ url: "https://example.test/a.png" });
  });

  it("keeps one radio in the tab order when the search hides the chosen image", async () => {
    mount();
    const user = userEvent.setup();

    await user.click(await screen.findByRole("radio", { name: /banner\.jpg/ }));
    await user.type(screen.getByLabelText("Search images"), "photo");

    expect(screen.getByRole("radio", { name: /photo-1\.png/ })).toHaveAttribute("tabindex", "0");
  });

  it("stops an upload when the picker is cancelled, and ignores a late answer", async () => {
    const { onClose } = mount();
    const user = userEvent.setup();

    await screen.findByRole("radio", { name: /photo-1\.png/ });
    fireEvent.change(screen.getByLabelText("Choose an image file"), {
      target: { files: [new File(["x"], "fresh.png", { type: "image/png" })] },
    });
    await user.click(screen.getByRole("button", { name: "Cancel" }));

    expect(onClose).toHaveBeenCalled();
    expect(FakeRequest.all[0]!.aborted).toBe(true);
    act(() => FakeRequest.all[0]!.respond(201, assetBody(3, { name: "fresh.png" })));
    expect(screen.queryByRole("radio", { name: /fresh\.png/ })).not.toBeInTheDocument();
  });

  it("stops an upload when the picker goes away", async () => {
    const { unmount } = mount();

    await screen.findByRole("radio", { name: /photo-1\.png/ });
    fireEvent.change(screen.getByLabelText("Choose an image file"), {
      target: { files: [new File(["x"], "fresh.png", { type: "image/png" })] },
    });
    unmount();

    expect(FakeRequest.all[0]!.aborted).toBe(true);
  });

  it("says so when there are no images, and still offers Upload to a member", async () => {
    mount({ list: listBody([]) });

    expect(await screen.findByText("No images yet")).toBeInTheDocument();
    expect(screen.getByText("Upload a PNG, JPEG, GIF or WebP, or drop it here.")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: /Upload/ })).toBeInTheDocument();
  });

  it("says so when no image matches the search", async () => {
    mount();
    const user = userEvent.setup();

    await user.type(await screen.findByLabelText("Search images"), "zzz");

    expect(screen.getByText("No image is named like that.")).toBeInTheDocument();
  });

  it("says so when more images cannot be loaded", async () => {
    mount({
      list: listBody([A], { next_before: A.id }),
    });
    const user = userEvent.setup();

    await user.click(await screen.findByRole("button", { name: "Show more" }));

    expect(await screen.findByText(/Could not load more images/)).toBeInTheDocument();
  });

  it("renders nothing when closed", () => {
    render(
      <MemoryRouter>
        <AssetPicker open={false} onPick={vi.fn()} onClose={vi.fn()} />
      </MemoryRouter>,
    );
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  });
});

describe("httpsAddress", () => {
  it("accepts https and refuses everything else", () => {
    expect(httpsAddress(" https://example.test/x.png ")).toBe("https://example.test/x.png");
    expect(httpsAddress("http://example.test/x.png")).toBeNull();
    expect(httpsAddress("javascript:alert(1)")).toBeNull();
    expect(httpsAddress("example.test/x.png")).toBeNull();
    expect(httpsAddress("")).toBeNull();
  });
});
