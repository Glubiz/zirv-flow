import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";

import { Visibility } from "./Visibility";

vi.mock("../../segments", () => ({
  ConditionEditor: () => {
    throw new Error("a shape the editor cannot draw");
  },
}));

describe("the condition guard", () => {
  it("shows the notice and the replace action when the condition editor throws", async () => {
    vi.spyOn(console, "error").mockImplementation(() => undefined);
    const onChange = vi.fn();
    render(<Visibility workspaceId="ws1" value={{ all: [] }} readOnly={false} issues={[]} onChange={onChange} />);

    expect(screen.getByText("This condition can't be shown here.")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Always" })).toBeInTheDocument();
    await userEvent.setup().click(screen.getByRole("button", { name: "Replace with a new condition" }));
    expect(onChange).toHaveBeenCalledWith({ all: [] });
  });
});
