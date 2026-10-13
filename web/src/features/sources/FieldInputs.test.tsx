import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";

import { FieldInputs } from "./FieldInputs";
import { KAFKA_KIND, POSTGRES_KIND } from "./testData";

const fields = POSTGRES_KIND.config_fields;

describe("the source form fields", () => {
  it("renders a long text setting as a multi-line input and reports what is typed", async () => {
    const onChange = vi.fn();
    const user = userEvent.setup();
    render(<FieldInputs fields={fields} values={{}} onChange={onChange} />);

    const ca = screen.getByLabelText("CA certificate (optional)");
    expect(ca.tagName).toBe("TEXTAREA");
    expect(screen.getByText("A PEM certificate to trust.")).toBeInTheDocument();
    await user.type(ca, "x");

    expect(onChange).toHaveBeenCalledWith("ca_certificate", "x");
  });

  it("renders the queries setting as the query editor on the stored JSON text", () => {
    const stored = JSON.stringify([{ name: "vip", sql: "SELECT 1" }]);
    render(<FieldInputs fields={fields} values={{ queries: stored }} onChange={vi.fn()} />);

    expect(screen.getByLabelText("Query 1 name")).toHaveValue("vip");
    expect(screen.getByLabelText("Query 1 SQL")).toHaveValue("SELECT 1");
  });

  it("reports the whole list when a query changes", async () => {
    const onChange = vi.fn();
    const user = userEvent.setup();
    render(<FieldInputs fields={fields} values={{}} onChange={onChange} />);

    await user.click(screen.getByRole("button", { name: "Add query" }));

    expect(onChange).toHaveBeenCalledWith("queries", JSON.stringify([{ name: "", sql: "" }]));
  });

  it("keeps the credential fields password inputs", () => {
    render(<FieldInputs fields={POSTGRES_KIND.credential_fields} values={{}} onChange={vi.fn()} />);

    expect(screen.getByLabelText("User")).toHaveAttribute("type", "password");
    expect(screen.getByLabelText("Password")).toHaveAttribute("type", "password");
  });
});

describe("the Kafka source form", () => {
  it("renders the broker list as text, the authority as a multi-line input and the credential as passwords", () => {
    render(<FieldInputs fields={KAFKA_KIND.config_fields} values={{}} onChange={vi.fn()} />);

    expect(screen.getByLabelText("Brokers")).toHaveAttribute("type", "text");
    expect(screen.getByText("Comma-separated host:port pairs.")).toBeInTheDocument();
    expect(screen.getByLabelText("Certificate authority (optional)").tagName).toBe("TEXTAREA");
    expect(screen.getByLabelText("Authentication (optional)").tagName).toBe("SELECT");
    expect(screen.getByLabelText("Highest healthy lag (optional)")).toHaveAttribute("type", "number");
  });

  it("keeps the client certificate and key as password inputs that are never filled in", () => {
    render(<FieldInputs fields={KAFKA_KIND.credential_fields} values={{}} onChange={vi.fn()} />);

    for (const label of ["User name (optional)", "Client key (optional)", "Client certificate (optional)"]) {
      expect(screen.getByLabelText(label)).toHaveAttribute("type", "password");
    }
  });
});
