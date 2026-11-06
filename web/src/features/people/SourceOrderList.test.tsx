import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useState } from "react";
import { describe, expect, it } from "vitest";

import { json, mockApi, problem } from "../../test/mockApi";
import { SourceOrderList } from "./SourceOrderList";

const CRM = "01a1b057-bdba-7000-8000-000000000001";
const SHOP = "01a1b057-bdba-7000-8000-000000000002";
const GONE = "01a1b057-bdba-7000-8000-000000000003";

function source(id: string, name: string) {
  return { id, name, kind: "postgres", config: {}, enabled: true };
}

function Harness({ initial, onValue }: { initial: string[]; onValue: (value: string[]) => void }) {
  const [value, setValue] = useState(initial);
  return (
    <SourceOrderList
      workspaceId="ws1"
      value={value}
      onChange={(next) => {
        setValue(next);
        onValue(next);
      }}
    />
  );
}

describe("source order", () => {
  it("adds sources by name, stores their ids and reorders them", async () => {
    mockApi({ "GET /api/v1/workspaces/ws1/sources": () => json([source(CRM, "CRM"), source(SHOP, "Shop")]) });
    const user = userEvent.setup();
    let sent: string[] = [];
    render(<Harness initial={[]} onValue={(value) => (sent = value)} />);

    const add = await screen.findByLabelText("Add a source");
    await user.selectOptions(add, CRM);
    await user.selectOptions(screen.getByLabelText("Add a source"), SHOP);
    await user.selectOptions(screen.getByLabelText("Add a source"), "api");
    expect(sent).toEqual([CRM, SHOP, "api"]);

    await user.click(screen.getByRole("button", { name: "Move Shop up" }));
    expect(sent).toEqual([SHOP, CRM, "api"]);
    await user.click(screen.getByRole("button", { name: "Move CRM down" }));
    expect(sent).toEqual([SHOP, "api", CRM]);
    await user.click(screen.getByRole("button", { name: "Remove API" }));
    expect(sent).toEqual([SHOP, CRM]);

    const items = within(screen.getByRole("list", { name: "Source order" })).getAllByRole("listitem");
    expect(items.map((item) => item.textContent)).toEqual([
      expect.stringContaining("Shop"),
      expect.stringContaining("CRM"),
    ]);
    expect(screen.getByRole("button", { name: "Move Shop up" })).toBeDisabled();
  });

  it("shows an entry that matches nothing as an unknown source that can be removed", async () => {
    mockApi({ "GET /api/v1/workspaces/ws1/sources": () => json([source(CRM, "CRM")]) });
    const user = userEvent.setup();
    let sent: string[] = [GONE, CRM];
    render(<Harness initial={sent} onValue={(value) => (sent = value)} />);

    expect(await screen.findByText("Unknown source")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Remove Unknown source" }));
    expect(sent).toEqual([CRM]);
  });

  it("says when the sources did not load, and loads them again on retry", async () => {
    let failing = true;
    mockApi({
      "GET /api/v1/workspaces/ws1/sources": () =>
        failing ? problem(500, "Server error", "try later") : json([source(CRM, "CRM")]),
    });
    const user = userEvent.setup();
    render(<Harness initial={[]} onValue={() => undefined} />);

    expect(await screen.findByText("Sources did not load")).toBeInTheDocument();
    failing = false;
    await user.click(screen.getByRole("button", { name: "Try again" }));
    expect(await screen.findByLabelText("Add a source")).toBeInTheDocument();
  });
});
