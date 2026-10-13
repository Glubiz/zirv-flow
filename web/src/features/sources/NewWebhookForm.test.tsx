import { screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { json, mockApi, OWNER, problem, session } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { NewSourcePage } from "./NewSourcePage";
import { aWebhook, FAKE_KIND, WEBHOOK_KIND } from "./testData";

const BASE = {
  "GET /api/v1/session": () => json(session(OWNER, "csrf-hooks")),
  "GET /api/v1/workspaces/ws1/members": () =>
    json([{ user_id: OWNER.id, email: OWNER.email, role: "admin", added_at: "2026-10-01T09:00:00Z" }]),
  "GET /api/v1/source-kinds": () => json([FAKE_KIND, WEBHOOK_KIND]),
};

function mount() {
  return renderRoute("/w/ws1/sources/new", "/w/:ws/sources/new", <NewSourcePage />);
}

async function chooseWebhook(user: ReturnType<typeof userEvent.setup>) {
  await user.click(await screen.findByRole("button", { name: "Choose Webhook" }));
}

describe("creating a webhook", () => {
  it("skips the credential and test steps and asks for a name, a dedupe field and a stale time", async () => {
    mockApi(BASE);
    const user = userEvent.setup();
    mount();

    await chooseWebhook(user);

    expect(screen.getByLabelText("Name")).toBeInTheDocument();
    expect(screen.getByLabelText("Dedupe field")).toBeInTheDocument();
    expect(screen.getByLabelText("Expect a payload at least every (minutes)")).toBeInTheDocument();
    expect(screen.queryByRole("navigation", { name: "Source setup steps" })).not.toBeInTheDocument();
    expect(screen.queryByText("Credential")).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Create webhook" })).toBeDisabled();
  });

  it("sends the defaults, then shows the URL and the secret once", async () => {
    const calls = mockApi({
      ...BASE,
      "POST /api/v1/workspaces/ws1/webhooks": () => json({ ...aWebhook(), secret: "zfwh_0123456789abcdef" }, 201),
    });
    const user = userEvent.setup();
    mount();

    await chooseWebhook(user);
    await user.type(screen.getByLabelText("Name"), "Orders");
    await user.type(screen.getByLabelText("Dedupe field"), "id");
    await user.type(screen.getByLabelText("Expect a payload at least every (minutes)"), "30");
    await user.click(screen.getByRole("button", { name: "Create webhook" }));

    const dialog = await screen.findByRole("dialog", { name: "Your webhook is ready" });
    expect(dialog).toHaveTextContent("zfwh_0123456789abcdef");
    expect(dialog).toHaveTextContent(`${window.location.origin}/hooks/hook-1`);
    expect(dialog).toHaveTextContent("not shown again");
    expect(screen.getByRole("button", { name: "Copy the secret" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Copy the URL" })).toBeInTheDocument();
    expect(screen.getByLabelText("Example command")).toHaveTextContent("openssl dgst -sha256");
    const created = calls.find((call) => call.method === "POST" && call.path.endsWith("/webhooks"));
    expect(created?.body).toEqual({
      name: "Orders",
      dedupe_field: "id",
      signature_header: "X-Zirv-Signature",
      signature_prefix: "sha256=",
      timestamp_header: "X-Zirv-Timestamp",
      tolerance_seconds: 300,
      staleness_threshold_seconds: 1800,
    });
    expect(created?.headers.get("x-csrf-token")).toBe("csrf-hooks");
  });

  it("signs the body alone when the timestamp is switched off, and hides its header and tolerance", async () => {
    const calls = mockApi({
      ...BASE,
      "POST /api/v1/workspaces/ws1/webhooks": () => json({ ...aWebhook({ timestamp_header: null }), secret: "zfwh_x" }, 201),
    });
    const user = userEvent.setup();
    mount();

    await chooseWebhook(user);
    await user.type(screen.getByLabelText("Name"), "Forms");
    await user.type(screen.getByLabelText("Dedupe field"), "submission.id");
    await user.selectOptions(screen.getByLabelText("How the sender signs"), "body");

    expect(screen.queryByLabelText("Timestamp header")).not.toBeInTheDocument();
    expect(screen.queryByLabelText("Tolerance (seconds)")).not.toBeInTheDocument();
    expect(screen.getByText(/Nothing but the dedupe field stops a captured request/)).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Create webhook" }));

    await screen.findByRole("dialog", { name: "Your webhook is ready" });
    const created = calls.find((call) => call.method === "POST" && call.path.endsWith("/webhooks"));
    expect(created?.body).toMatchObject({ timestamp_header: null, dedupe_field: "submission.id" });
    expect(created?.body).not.toHaveProperty("tolerance_seconds");
    expect(screen.getByLabelText("Example command")).not.toHaveTextContent("$TS");
  });

  it("refuses a bad dedupe path, a header that repeats the signature header and a bad tolerance", async () => {
    mockApi(BASE);
    const user = userEvent.setup();
    mount();

    await chooseWebhook(user);
    await user.type(screen.getByLabelText("Name"), "Orders");
    await user.type(screen.getByLabelText("Dedupe field"), "a..b");

    expect(screen.getByText("Use a dotted path without empty parts.")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Create webhook" })).toBeDisabled();
    await user.clear(screen.getByLabelText("Dedupe field"));
    await user.type(screen.getByLabelText("Dedupe field"), "id");
    expect(screen.getByRole("button", { name: "Create webhook" })).toBeEnabled();

    const timestamp = screen.getByLabelText("Timestamp header");
    await user.clear(timestamp);
    await user.type(timestamp, "x-zirv-signature");
    expect(screen.getByText("Use a header name that differs from the signature header.")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Create webhook" })).toBeDisabled();
    await user.clear(timestamp);
    await user.type(timestamp, "X-Ts");
    const tolerance = screen.getByLabelText("Tolerance (seconds)");
    await user.clear(tolerance);
    await user.type(tolerance, "0");
    expect(screen.getByText("Use a whole number from 1 to 86400.")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Create webhook" })).toBeDisabled();
  });

  it("shows the refusal of the server and keeps the form", async () => {
    mockApi({
      ...BASE,
      "POST /api/v1/workspaces/ws1/webhooks": () => problem(409, "Conflict", "a source with that name already exists"),
    });
    const user = userEvent.setup();
    mount();

    await chooseWebhook(user);
    await user.type(screen.getByLabelText("Name"), "Orders");
    await user.type(screen.getByLabelText("Dedupe field"), "id");
    await user.click(screen.getByRole("button", { name: "Create webhook" }));

    expect(await screen.findByText("a source with that name already exists")).toBeInTheDocument();
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Create webhook" })).toBeEnabled();
  });
});
