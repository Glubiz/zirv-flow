import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { json, MEMBER, mockApi, problem } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { DESTINATION_ID, SENDER_ID, templateRoutes, TEMPLATE_ID, WS } from "../../test/templatesApi";
import { TemplatePage } from "./TemplatePage";

function mount() {
  return renderRoute(`/w/ws1/templates/${TEMPLATE_ID}`, "/w/:ws/templates/:id", <TemplatePage />);
}

const WAIT = { timeout: 4000 };
const SEND = `POST ${WS}/templates/${TEMPLATE_ID}/test-send`;

async function openDialog(user: ReturnType<typeof userEvent.setup>) {
  await user.click(await screen.findByRole("button", { name: "Test send" }));
  const dialog = await screen.findByRole("dialog", { name: "Send a test" });
  await within(dialog).findByRole("form", { name: "Test send" });
  return dialog;
}

function sent(calls: { method: string; path: string; body: unknown }[]) {
  return calls.filter((call) => call.method === "POST" && call.path.endsWith("/test-send")).map((call) => call.body as Record<string, unknown>);
}

const ROW = {
  id: "ts-1",
  template_id: TEMPLATE_ID,
  destination_id: DESTINATION_ID,
  sender_identity_id: SENDER_ID,
  created_at: "2026-10-11T09:00:00Z",
  outcome: "queued",
  render_context: "sample",
  template_version: null,
};

describe("the test-send dialog", () => {
  it("sends the draft to a member through a destination", async () => {
    const calls = mockApi({ ...templateRoutes(), [SEND]: () => json(ROW, 202) });
    const user = userEvent.setup();
    mount();

    const dialog = await openDialog(user);
    expect(within(dialog).getByRole("button", { name: "Send test" })).toBeDisabled();
    await user.selectOptions(within(dialog).getByLabelText("Destination"), DESTINATION_ID);
    await user.selectOptions(within(dialog).getByLabelText("Member"), MEMBER.id);
    await user.click(within(dialog).getByRole("button", { name: "Send test" }));

    expect(await within(dialog).findByText(/Test queued/)).toBeInTheDocument();
    expect(sent(calls)).toEqual([{ destination_id: DESTINATION_ID, sender_identity_id: SENDER_ID, to: { member: MEMBER.id } }]);
  });

  it("sends to an allowlisted address and shows what is on the allowlist", async () => {
    const calls = mockApi({ ...templateRoutes(), [SEND]: () => json(ROW, 202) });
    const user = userEvent.setup();
    mount();

    const dialog = await openDialog(user);
    await user.selectOptions(within(dialog).getByLabelText("Send it to"), "address");
    expect(within(dialog).getByText(/q\*\*\*@example\.com/)).toBeInTheDocument();
    await user.selectOptions(within(dialog).getByLabelText("Destination"), DESTINATION_ID);
    await user.type(within(dialog).getByLabelText("Address"), "qa@example.com");
    await user.click(within(dialog).getByRole("button", { name: "Send test" }));

    await waitFor(() => expect(sent(calls)).toHaveLength(1), WAIT);
    expect(sent(calls)[0]?.["to"]).toEqual({ address: "qa@example.com" });
  });

  it("shows the rate-limit error", async () => {
    mockApi({
      ...templateRoutes(),
      [SEND]: () => problem(429, "Too many requests", "You have used your 10 test sends this hour.", undefined, "test_send_rate_limited"),
    });
    const user = userEvent.setup();
    mount();

    const dialog = await openDialog(user);
    await user.selectOptions(within(dialog).getByLabelText("Destination"), DESTINATION_ID);
    await user.selectOptions(within(dialog).getByLabelText("Member"), MEMBER.id);
    await user.click(within(dialog).getByRole("button", { name: "Send test" }));

    expect(await within(dialog).findByText("Too many requests")).toBeInTheDocument();
    expect(within(dialog).getByText("You have used your 10 test sends this hour.")).toBeInTheDocument();
  });

  it("shows a refused recipient", async () => {
    mockApi({
      ...templateRoutes(),
      [SEND]: () => problem(422, "Unprocessable", "That recipient is not allowed.", undefined, "recipient_not_allowed"),
    });
    const user = userEvent.setup();
    mount();

    const dialog = await openDialog(user);
    await user.selectOptions(within(dialog).getByLabelText("Destination"), DESTINATION_ID);
    await user.selectOptions(within(dialog).getByLabelText("Member"), MEMBER.id);
    await user.click(within(dialog).getByRole("button", { name: "Send test" }));

    expect(await within(dialog).findByText("That recipient is not allowed.")).toBeInTheDocument();
  });

  it("shows the test-send log with an outcome in words", async () => {
    mockApi({
      ...templateRoutes(),
      [`GET ${WS}/test-sends`]: () =>
        json({
          test_sends: [
            { ...ROW, id: "a", outcome: "sent" },
            { ...ROW, id: "b", outcome: "sender_unverified", reason: "sender_unverified" },
          ],
          next_before: null,
        }),
    });
    const user = userEvent.setup();
    mount();

    const dialog = await openDialog(user);

    const log = await within(dialog).findByRole("table", { name: "Recent tests of this template" });
    expect(within(log).getByText("Sent")).toBeInTheDocument();
    expect(within(log).getByText("Not sent, sender not verified")).toBeInTheDocument();
  });

  it("shows an empty log", async () => {
    mockApi(templateRoutes());
    const user = userEvent.setup();
    mount();

    const dialog = await openDialog(user);

    expect(await within(dialog).findByText("No tests sent yet")).toBeInTheDocument();
  });

  it("shows an error with a retry when the recipients cannot load", async () => {
    let attempts = 0;
    mockApi({
      ...templateRoutes(),
      [`GET ${WS}/destinations`]: () => {
        attempts += 1;
        return attempts === 1 ? problem(500, "Internal Server Error", "internal error") : json([]);
      },
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Test send" }));
    const dialog = await screen.findByRole("dialog", { name: "Send a test" });
    await user.click(await within(dialog).findByRole("button", { name: "Try again" }));

    expect(await within(dialog).findByRole("form", { name: "Test send" })).toBeInTheDocument();
  });
});
