import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { json, mockApi, problem } from "../../test/mockApi";
import { SENDER_ID, SENDER_OPTIONS, WS } from "../../test/templatesApi";
import { SenderPicker } from "./SenderPicker";

const OPTIONS = `GET ${WS}/sender-identities/options`;

function mount(value = SENDER_ID, onChange: (id: string) => void = () => undefined) {
  return render(<SenderPicker workspaceId="ws1" value={value} onChange={onChange} disabled={false} />);
}

describe("the sender picker", () => {
  it("names the senders a writer can pick, from the options read", async () => {
    const calls = mockApi({ [OPTIONS]: () => json(SENDER_OPTIONS) });
    mount();

    expect(await screen.findByRole("option", { name: "Acme <hello@mail.acme.example>" })).toBeInTheDocument();
    expect(screen.getByLabelText("Sender identity")).toHaveValue(SENDER_ID);
    expect(calls.map((call) => call.path)).toEqual([`${WS}/sender-identities/options`]);
  });

  it("says it is loading", () => {
    mockApi({ [OPTIONS]: () => new Promise<Response>(() => undefined) });
    mount();

    expect(screen.getByText("Loading sender identities")).toBeInTheDocument();
  });

  it("shows the chosen id and a retry when the options cannot be read", async () => {
    let attempts = 0;
    mockApi({
      [OPTIONS]: () => {
        attempts += 1;
        return attempts === 1 ? problem(403, "Forbidden", "Needs the write role.") : json(SENDER_OPTIONS);
      },
    });
    const user = userEvent.setup();
    mount();

    expect(await screen.findByText(SENDER_ID)).toBeInTheDocument();
    expect(screen.getByText("Needs the write role.")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Try again" }));

    expect(await screen.findByLabelText("Sender identity")).toHaveValue(SENDER_ID);
  });

  it("says there is no sender yet, and warns about a chosen id that is not listed", async () => {
    mockApi({ [OPTIONS]: () => json([]) });
    mount(SENDER_ID);

    expect(await screen.findByRole("option", { name: SENDER_ID })).toBeInTheDocument();
    expect(screen.getByText(/Warning: the chosen sender is not in this workspace's list/)).toBeInTheDocument();
  });

  it("keeps the name of a chosen sender that is archived and warns", async () => {
    mockApi({ [OPTIONS]: () => json([{ ...SENDER_OPTIONS[0], archived: true }]) });
    mount();

    expect(await screen.findByRole("option", { name: "Acme <hello@mail.acme.example> (archived)" })).toBeInTheDocument();
    expect(screen.getByText(/the chosen sender is archived/)).toBeInTheDocument();
  });
});
