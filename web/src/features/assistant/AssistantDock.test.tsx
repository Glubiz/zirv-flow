import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { createMemoryRouter, RouterProvider, useLocation } from "react-router";
import { describe, expect, it } from "vitest";

import { AssistantDock } from "./AssistantDock";
import { AssistantToggle } from "./AssistantToggle";
import { useAssistantDock } from "./useAssistantDock";

function Probe() {
  return <p data-testid="search">{useLocation().search}</p>;
}

function Editor({ explain = true, draft = true }: { explain?: boolean; draft?: boolean }) {
  const dock = useAssistantDock();
  return (
    <>
      <AssistantToggle open={dock.open} onToggle={dock.toggle} />
      {dock.open ? (
        <AssistantDock
          thing="flow"
          onClose={() => dock.setOpen(false)}
          draft={draft ? <p>Draft pane</p> : undefined}
          explain={explain ? <p>Explain pane</p> : undefined}
        />
      ) : null}
      <Probe />
    </>
  );
}

function mount(url: string, props: { explain?: boolean; draft?: boolean } = {}) {
  const router = createMemoryRouter([{ path: "*", element: <Editor {...props} /> }], { initialEntries: [url] });
  render(<RouterProvider router={router} />);
  return router;
}

describe("the assistant dock", () => {
  it("opens from the toggle, keeps its state in ?assistant=1 and is an aside named Assistant", async () => {
    const user = userEvent.setup();
    mount("/x");

    await user.click(screen.getByRole("button", { name: "Assistant" }));

    expect(screen.getByTestId("search")).toHaveTextContent("?assistant=1");
    expect(screen.getByRole("complementary", { name: "Assistant" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Assistant" })).toHaveAttribute("aria-pressed", "true");
  });

  it("is open when the URL says so", () => {
    mount("/x?assistant=1");

    expect(screen.getByRole("complementary", { name: "Assistant" })).toBeInTheDocument();
  });

  it("offers the draft tab and the explain tab of the thing, the first one active", () => {
    mount("/x?assistant=1");

    const tabs = screen.getByRole("navigation", { name: "Assistant" });
    expect(within(tabs).getByRole("link", { name: "Draft a change" })).toHaveAttribute("aria-current", "page");
    expect(within(tabs).getByRole("link", { name: "Explain this flow" })).not.toHaveAttribute("aria-current");
  });

  it("switches tabs through the URL and keeps the draft pane mounted behind the explanation", async () => {
    const user = userEvent.setup();
    mount("/x?assistant=1");

    await user.click(screen.getByRole("link", { name: "Explain this flow" }));

    expect(screen.getByTestId("search")).toHaveTextContent("assistant=1&dock=explain");
    expect(screen.getByText("Explain pane")).toBeVisible();
    expect(screen.getByText("Draft pane")).not.toBeVisible();
  });

  it("hides the draft tab when the editor is read-only", () => {
    mount("/x?assistant=1", { draft: false });

    const tabs = screen.getByRole("navigation", { name: "Assistant" });
    expect(within(tabs).queryByRole("link", { name: "Draft a change" })).not.toBeInTheDocument();
    expect(within(tabs).getByRole("link", { name: "Explain this flow" })).toHaveAttribute("aria-current", "page");
  });

  it("closes with the x button, takes its parameters out of the URL and returns focus to the toggle", async () => {
    const user = userEvent.setup();
    mount("/x?assistant=1&dock=explain");

    await user.click(screen.getByRole("button", { name: "Close the assistant" }));

    expect(screen.queryByRole("complementary", { name: "Assistant" })).not.toBeInTheDocument();
    expect(screen.getByTestId("search")).toHaveTextContent(/^$/);
    expect(screen.getByRole("button", { name: "Assistant" })).toHaveFocus();
  });

  it("closes on Escape and returns focus to the toggle", async () => {
    const user = userEvent.setup();
    mount("/x?assistant=1");
    screen.getByRole("link", { name: "Explain this flow" }).focus();

    await user.keyboard("{Escape}");

    expect(screen.queryByRole("complementary", { name: "Assistant" })).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Assistant" })).toHaveFocus();
  });
});
