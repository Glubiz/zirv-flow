import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useState } from "react";
import { describe, expect, it } from "vitest";

import { TransformEditor } from "./TransformEditor";
import { newTransform, type TransformDraft } from "./transforms";

function Harness({ initial = [] as TransformDraft[], disabled = false }) {
  const [steps, setSteps] = useState(initial);
  return (
    <>
      <TransformEditor column="Email" others={["Name"]} steps={steps} disabled={disabled} onChange={setSteps} />
      <output aria-label="ops">{steps.map((step) => step.op).join(",")}</output>
    </>
  );
}

describe("the transform editor", () => {
  it("says a cell is used as it is while it has no steps", () => {
    render(<Harness />);

    expect(screen.getByText("The cell is used as it is.")).toBeInTheDocument();
  });

  it("adds, changes and removes steps", async () => {
    const user = userEvent.setup();
    render(<Harness />);

    await user.selectOptions(screen.getByLabelText("Add a step to Email"), "trim");
    await user.selectOptions(screen.getByLabelText("Add a step to Email"), "constant");
    await user.type(screen.getByLabelText("Fixed value of step 2 of Email"), "x");
    await user.selectOptions(screen.getByLabelText("Type of step 1 of Email"), "lower");
    expect(screen.getByLabelText("ops")).toHaveTextContent("lower,constant");
    await user.click(screen.getByRole("button", { name: "Remove step 1 of Email" }));

    expect(screen.getByLabelText("ops")).toHaveTextContent("constant");
    expect(screen.getByLabelText("Fixed value of step 1 of Email")).toHaveValue("x");
  });

  it("offers the other columns to a join and locks everything when disabled", async () => {
    const user = userEvent.setup();
    const { unmount } = render(<Harness initial={[newTransform("concat")]} />);

    await user.click(screen.getByRole("checkbox", { name: "Name" }));
    expect(screen.getByRole("checkbox", { name: "Name" })).toBeChecked();
    unmount();
    render(<Harness initial={[newTransform("trim")]} disabled />);

    expect(screen.getByLabelText("Add a step to Email")).toBeDisabled();
    expect(screen.getByRole("button", { name: "Remove step 1 of Email" })).toBeDisabled();
  });
});
