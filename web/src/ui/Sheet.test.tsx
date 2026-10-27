import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useState } from "react";
import { describe, expect, it } from "vitest";

import { Button } from "./Button";
import { Sheet } from "./Sheet";

function Harness({ autoFocus = true }: { autoFocus?: boolean }) {
  const [open, setOpen] = useState(false);
  return (
    <>
      <Button onClick={() => setOpen(true)}>Open sheet</Button>
      <Button>Elsewhere</Button>
      {open ? (
        <Sheet title="Step settings" eyebrow="Wait" onClose={() => setOpen(false)} autoFocus={autoFocus}>
          <Button>Inside</Button>
        </Sheet>
      ) : null}
    </>
  );
}

describe("sheet", () => {
  it("is a non-modal dialog named by its heading, and moves focus in when it opens", async () => {
    const user = userEvent.setup();
    render(<Harness />);

    await user.click(screen.getByRole("button", { name: "Open sheet" }));

    const sheet = screen.getByRole("dialog", { name: "Step settings" });
    expect(sheet).toHaveAttribute("aria-modal", "false");
    expect(sheet).toHaveFocus();
  });

  it("stays non-modal by default: no backdrop and nothing made inert", async () => {
    const user = userEvent.setup();
    const { container } = render(<Harness />);
    await user.click(screen.getByRole("button", { name: "Open sheet" }));

    expect(document.querySelector(".sheet-backdrop")).toBeNull();
    expect(container).not.toHaveAttribute("inert");
  });

  it("does not trap focus: Tab leaves it for the page", async () => {
    const user = userEvent.setup();
    render(<Harness />);
    await user.click(screen.getByRole("button", { name: "Open sheet" }));

    await user.tab();
    expect(screen.getByRole("button", { name: "Close" })).toHaveFocus();
    await user.tab();
    expect(screen.getByRole("button", { name: "Inside" })).toHaveFocus();
    await user.tab();
    expect(document.body).toHaveFocus();
  });

  it("closes on Escape and returns focus to the opener", async () => {
    const user = userEvent.setup();
    render(<Harness />);
    const opener = screen.getByRole("button", { name: "Open sheet" });
    await user.click(opener);

    await user.keyboard("{Escape}");

    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(opener).toHaveFocus();
  });

  it("closes from its Close button and gives focus back", async () => {
    const user = userEvent.setup();
    render(<Harness />);
    const opener = screen.getByRole("button", { name: "Open sheet" });
    await user.click(opener);

    await user.click(screen.getByRole("button", { name: "Close" }));

    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(opener).toHaveFocus();
  });

  it("leaves focus where it was when asked not to take it", async () => {
    const user = userEvent.setup();
    render(<Harness autoFocus={false} />);
    const opener = screen.getByRole("button", { name: "Open sheet" });
    await user.click(opener);

    expect(screen.getByRole("dialog", { name: "Step settings" })).toBeInTheDocument();
    expect(opener).toHaveFocus();
  });
});
