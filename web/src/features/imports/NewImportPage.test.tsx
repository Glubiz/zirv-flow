import { fireEvent, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { Route, Routes, useLocation } from "react-router";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { json, MEMBER, mockApi, OWNER, session } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { NewImportPage, uploadUrl } from "./NewImportPage";

const members = (role: "admin" | "member", userId: string) => [
  { user_id: userId, email: "someone@example.test", role, added_at: "2026-10-01T09:00:00Z" },
];

class FakeRequest {
  static all: FakeRequest[] = [];
  method = "";
  url = "";
  headers: Record<string, string> = {};
  body: unknown = null;
  status = 0;
  responseText = "";
  upload: { onprogress: ((event: { lengthComputable: boolean; loaded: number; total: number }) => void) | null } = {
    onprogress: null,
  };
  onload: (() => void) | null = null;
  onerror: (() => void) | null = null;
  onabort: (() => void) | null = null;

  constructor() {
    FakeRequest.all.push(this);
  }

  open(method: string, url: string) {
    this.method = method;
    this.url = url;
  }

  setRequestHeader(name: string, value: string) {
    this.headers[name.toLowerCase()] = value;
  }

  send(body: unknown) {
    this.body = body;
  }

  progress(loaded: number, total: number) {
    this.upload.onprogress?.({ lengthComputable: true, loaded, total });
  }

  respond(status: number, body: unknown) {
    this.status = status;
    this.responseText = JSON.stringify(body);
    this.onload?.();
  }
}

function Landing() {
  const location = useLocation();
  return <p>Now at {location.pathname}</p>;
}

function mount() {
  return renderRoute(
    "/w/ws1/imports/new",
    "/w/:ws/imports/*",
    <Routes>
      <Route path="new" element={<NewImportPage />} />
      <Route path=":importId" element={<Landing />} />
    </Routes>,
  );
}

function mockAsOwner() {
  return mockApi({
    "GET /api/v1/session": () => json(session(OWNER, "csrf-upload")),
    "GET /api/v1/workspaces/ws1/members": () => json(members("admin", OWNER.id)),
  });
}

const CSV = new File(["Email,Plan\nada@example.test,pro\n"], "Customers May.csv", { type: "text/csv" });

describe("uploading a file", () => {
  beforeEach(() => {
    FakeRequest.all = [];
    vi.stubGlobal("XMLHttpRequest", FakeRequest);
  });

  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("builds the upload address from the name, the file, the stream and the delimiter", () => {
    expect(uploadUrl("ws1", { name: "May list", fileName: "may.csv", stream: " ", delimiter: "" })).toBe(
      "/api/v1/workspaces/ws1/imports?name=May+list&file_name=may.csv",
    );
    expect(uploadUrl("ws1", { name: "x", fileName: "x.csv", stream: "people", delimiter: "semicolon" })).toBe(
      "/api/v1/workspaces/ws1/imports?name=x&file_name=x.csv&stream=people&delimiter=semicolon",
    );
  });

  it("names the import after the file, shows progress while it uploads, and opens the import", async () => {
    mockAsOwner();
    const user = userEvent.setup();
    mount();

    await user.upload(await screen.findByLabelText("CSV file"), CSV);
    expect(screen.getByLabelText("Name")).toHaveValue("Customers May");
    await user.click(screen.getByRole("button", { name: "Upload and continue" }));

    const request = FakeRequest.all[0]!;
    expect(request.method).toBe("POST");
    expect(request.url).toBe("/api/v1/workspaces/ws1/imports?name=Customers+May&file_name=Customers+May.csv");
    expect(request.headers["content-type"]).toBe("text/csv");
    expect(request.headers["x-csrf-token"]).toBe("csrf-upload");
    expect(request.body).toBe(CSV);
    request.progress(16, 32);
    expect(await screen.findByText("Uploading 50%")).toBeInTheDocument();
    expect(screen.getByRole("progressbar", { name: "Upload progress" })).toHaveAttribute("value", "16");
    expect(screen.getByRole("button", { name: "Uploading" })).toBeDisabled();
    request.respond(201, { id: "imp-9" });

    expect(await screen.findByText("Now at /w/ws1/imports/imp-9")).toBeInTheDocument();
  });

  it("takes a file dropped on the page", async () => {
    mockAsOwner();
    mount();

    const zone = (await screen.findByText("Drag a CSV file here")).closest(".dropzone")!;
    fireEvent.dragOver(zone);
    expect(zone).toHaveClass("dropzone-over");
    fireEvent.drop(zone, { dataTransfer: { files: [CSV] } });

    expect(await screen.findByText("Customers May.csv")).toBeInTheDocument();
    expect(screen.getByLabelText("Name")).toHaveValue("Customers May");
    expect(zone).not.toHaveClass("dropzone-over");
  });

  it("will not upload before there is a file and a name", async () => {
    mockAsOwner();
    const user = userEvent.setup();
    mount();

    const button = await screen.findByRole("button", { name: "Upload and continue" });
    expect(button).toBeDisabled();
    await user.upload(screen.getByLabelText("CSV file"), CSV);
    expect(button).toBeEnabled();
    await user.clear(screen.getByLabelText("Name"));
    expect(button).toBeDisabled();
  });

  it("sends the stream and the delimiter when they are set", async () => {
    mockAsOwner();
    const user = userEvent.setup();
    mount();

    await user.upload(await screen.findByLabelText("CSV file"), CSV);
    await user.click(screen.getByText("Advanced"));
    await user.type(screen.getByLabelText("Stream"), "people");
    await user.selectOptions(screen.getByLabelText("Delimiter"), "semicolon");
    await user.click(screen.getByRole("button", { name: "Upload and continue" }));

    expect(FakeRequest.all[0]!.url).toBe(
      "/api/v1/workspaces/ws1/imports?name=Customers+May&file_name=Customers+May.csv&stream=people&delimiter=semicolon",
    );
  });

  it("shows why the server refused the file and lets the person try again", async () => {
    mockAsOwner();
    const user = userEvent.setup();
    mount();

    await user.upload(await screen.findByLabelText("CSV file"), CSV);
    await user.click(screen.getByRole("button", { name: "Upload and continue" }));
    FakeRequest.all[0]!.respond(422, {
      title: "Unprocessable Entity",
      status: 422,
      detail: "body: header names must be unique",
      trace_id: "abc123",
    });

    expect(await screen.findByText("body: header names must be unique")).toBeInTheDocument();
    expect(screen.getByText("abc123")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Upload and continue" })).toBeEnabled();
    expect(screen.queryByRole("progressbar")).not.toBeInTheDocument();
  });

  it("says so when the upload does not reach the server", async () => {
    mockAsOwner();
    const user = userEvent.setup();
    mount();

    await user.upload(await screen.findByLabelText("CSV file"), CSV);
    await user.click(screen.getByRole("button", { name: "Upload and continue" }));
    FakeRequest.all[0]!.onerror?.();

    await waitFor(() => expect(screen.getByRole("alert")).toHaveTextContent("did not reach the server"));
  });

  it("offers a reader no upload", async () => {
    mockApi({
      "GET /api/v1/session": () => json(session(MEMBER)),
      "GET /api/v1/workspaces/ws1/members": () => json(members("admin", "someone-else")),
    });
    mount();

    expect(await screen.findByText(/You can look at imports but not start one/)).toBeInTheDocument();
    expect(screen.queryByLabelText("CSV file")).not.toBeInTheDocument();
  });
});
