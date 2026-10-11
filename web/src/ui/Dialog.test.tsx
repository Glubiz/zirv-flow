import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useState } from "react";
import { describe, expect, it } from "vitest";

import { Button } from "./Button";
import { Dialog } from "./Dialog";
import { MenuButton } from "./Menu";

function Harness() {
  const [open, setOpen] = useState(false);
  return (
    <>
      <Button onClick={() => setOpen(true)}>Open dialog</Button>
      {open ? (
        <Dialog title="Confirm" onClose={() => setOpen(false)}>
          <Button>First</Button>
          <Button>Last</Button>
        </Dialog>
      ) : null}
    </>
  );
}

describe("dialog", () => {
  it("moves focus in, keeps Tab inside, closes on Escape and returns focus to the opener", async () => {
    const user = userEvent.setup();
    render(<Harness />);
    const opener = screen.getByRole("button", { name: "Open dialog" });

    await user.click(opener);
    expect(screen.getByRole("button", { name: "First" })).toHaveFocus();

    await user.tab();
    expect(screen.getByRole("button", { name: "Last" })).toHaveFocus();
    await user.tab();
    expect(screen.getByRole("button", { name: "First" })).toHaveFocus();

    await user.keyboard("{Escape}");
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(opener).toHaveFocus();
  });
});

describe("menu", () => {
  it("opens with the keyboard, moves with arrows and closes on Escape with focus back on the button", async () => {
    const user = userEvent.setup();
    const picked: string[] = [];
    render(
      <MenuButton
        label="Account menu"
        buttonClassName="avatar-button"
        buttonContent="OW"
        entries={[
          { key: "a", label: "Alpha", onSelect: () => picked.push("a") },
          { key: "b", label: "Beta", onSelect: () => picked.push("b") },
        ]}
      />,
    );
    const button = screen.getByRole("button", { name: "Account menu" });

    button.focus();
    await user.keyboard("{ArrowDown}");
    expect(screen.getByRole("menuitem", { name: "Alpha" })).toHaveFocus();
    await user.keyboard("{ArrowDown}");
    expect(screen.getByRole("menuitem", { name: "Beta" })).toHaveFocus();
    await user.keyboard("{Escape}");
    expect(screen.queryByRole("menu")).not.toBeInTheDocument();
    expect(button).toHaveFocus();

    await user.keyboard("{ArrowDown}{Enter}");
    expect(picked).toEqual(["a"]);
  });
});
