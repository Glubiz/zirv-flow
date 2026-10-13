import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useState } from "react";
import { describe, expect, it, vi } from "vitest";

import { QueriesField } from "./QueriesField";

/** Holds the form value the way a form does, and shows it so a test can read it. */
function Harness({ initial = "" }: { initial?: string }) {
  const [value, setValue] = useState(initial);
  return (
    <>
      <QueriesField label="Queries" hint="Named SELECT statements." value={value} onChange={setValue} />
      <output aria-label="form value">{value}</output>
    </>
  );
}

const value = () => screen.getByLabelText("form value").textContent;

describe("the query editor", () => {
  it("shows an empty state and a count when there are no queries", () => {
    render(<Harness />);

    expect(screen.getByText("No queries")).toBeInTheDocument();
    expect(screen.getByText("0 of 20")).toBeInTheDocument();
    expect(screen.getByText("Named SELECT statements.")).toBeInTheDocument();
  });

  it("adds a row, keeps the list as JSON text and shows the stream name", async () => {
    const user = userEvent.setup();
    render(<Harness />);

    await user.click(screen.getByRole("button", { name: "Add query" }));
    await user.type(screen.getByLabelText("Query 1 name"), "vip");
    await user.type(screen.getByLabelText("Query 1 SQL"), "SELECT id FROM customers");

    expect(value()).toBe(JSON.stringify([{ name: "vip", sql: "SELECT id FROM customers" }]));
    expect(screen.getByText("Offered as the stream query:vip")).toBeInTheDocument();
    expect(screen.queryByText("No queries")).not.toBeInTheDocument();
    expect(screen.getByText("1 of 20")).toBeInTheDocument();
  });

  it("reads the stored list back into rows and removes one", async () => {
    const user = userEvent.setup();
    const stored = JSON.stringify([
      { name: "vip", sql: "SELECT 1" },
      { name: "recent", sql: "SELECT 2" },
    ]);
    render(<Harness initial={stored} />);

    expect(screen.getByLabelText("Query 2 name")).toHaveValue("recent");
    await user.click(screen.getByRole("button", { name: "Remove query 1" }));

    expect(value()).toBe(JSON.stringify([{ name: "recent", sql: "SELECT 2" }]));
    await user.click(screen.getByRole("button", { name: "Remove query 1" }));
    expect(value()).toBe("");
    expect(screen.getByText("No queries")).toBeInTheDocument();
  });

  it("flags a name that is not allowed or is taken, as it is typed", async () => {
    const user = userEvent.setup();
    const stored = JSON.stringify([
      { name: "vip", sql: "SELECT 1" },
      { name: "other", sql: "SELECT 2" },
    ]);
    render(<Harness initial={stored} />);

    const second = screen.getByLabelText("Query 2 name");
    await user.clear(second);
    await user.type(second, "vip");
    expect(screen.getAllByText("Another query has this name.").length).toBeGreaterThan(0);
    await user.clear(second);
    await user.type(second, "Bad Name");

    expect(second).toHaveAccessibleDescription(/Use 1 to 64 characters: a to z, 0 to 9 and _\./);
    expect(second).toBeInvalid();
  });

  it("flags SQL that ends with a semicolon or does not start a query", async () => {
    const user = userEvent.setup();
    render(<Harness initial={JSON.stringify([{ name: "vip", sql: "SELECT 1;" }])} />);

    expect(screen.getByText("Remove the semicolon at the end.")).toBeInTheDocument();
    const sql = screen.getByLabelText("Query 1 SQL");
    await user.clear(sql);
    await user.type(sql, "DELETE FROM customers");

    expect(screen.getByText("Start with SELECT or WITH.")).toBeInTheDocument();
    await user.clear(sql);
    await user.type(sql, "WITH a AS (SELECT 1) SELECT * FROM a");
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
  });

  it("stops at twenty queries", async () => {
    const user = userEvent.setup();
    const twenty = Array.from({ length: 20 }, (_, n) => ({ name: `q${n}`, sql: "SELECT 1" }));
    render(<Harness initial={JSON.stringify(twenty)} />);

    expect(screen.getByText("20 of 20")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Add query" })).toBeDisabled();
    await user.click(screen.getByRole("button", { name: "Remove query 20" }));
    expect(screen.getByRole("button", { name: "Add query" })).toBeEnabled();
  });

  it("offers to start over when the value is not a list of queries", async () => {
    const user = userEvent.setup();
    render(<Harness initial={'{"not":"a list"}'} />);

    expect(screen.getByRole("alert")).toHaveTextContent("The queries could not be read");
    await user.click(screen.getByRole("button", { name: "Start over" }));

    expect(value()).toBe("");
    expect(screen.getByText("No queries")).toBeInTheDocument();
  });

  it("disables every control while the form is busy", () => {
    render(
      <QueriesField
        label="Queries"
        value={JSON.stringify([{ name: "vip", sql: "SELECT 1" }])}
        onChange={vi.fn()}
        disabled
      />,
    );

    expect(screen.getByLabelText("Query 1 name")).toBeDisabled();
    expect(screen.getByLabelText("Query 1 SQL")).toBeDisabled();
    expect(screen.getByRole("button", { name: "Add query" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "Remove query 1" })).toBeDisabled();
  });
});
