import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";

import type { Schema } from "./labels";
import { SchemaForm } from "./SchemaForm";

const SCHEMA: Schema = {
  fields: [
    { key: "host", label: "Host", type: "string", required: true, secret: false, options: [] },
    { key: "port", label: "Port", type: "integer", required: true, secret: false, options: [] },
    { key: "tls", label: "TLS mode", type: "enum", required: false, secret: false, options: ["starttls", "tls"] },
    { key: "home", label: "Home page", type: "url", required: false, secret: false, options: [] },
    { key: "password", label: "Password", type: "string", required: true, secret: true, options: [] },
  ],
};

describe("SchemaForm", () => {
  it("draws the non-secret fields from the schema by type", () => {
    render(<SchemaForm schema={SCHEMA} group="public" values={{}} onChange={vi.fn()} />);

    expect(screen.getByLabelText("Host")).toHaveAttribute("type", "text");
    expect(screen.getByLabelText("Port")).toHaveAttribute("type", "number");
    expect(screen.getByLabelText("Home page (optional)")).toHaveAttribute("type", "url");
    const tls = screen.getByLabelText("TLS mode (optional)");
    expect(tls.tagName).toBe("SELECT");
    expect(screen.getAllByRole("option").map((option) => option.textContent)).toEqual(["Not set", "starttls", "tls"]);
    expect(screen.queryByLabelText("Password")).not.toBeInTheDocument();
  });

  it("draws the secret fields as password inputs that start empty whatever the values hold", () => {
    render(<SchemaForm schema={SCHEMA} group="secret" values={{ password: "" }} onChange={vi.fn()} />);

    const password = screen.getByLabelText("Password");
    expect(password).toHaveAttribute("type", "password");
    expect(password).toHaveAttribute("autocomplete", "new-password");
    expect(password).toHaveValue("");
    expect(screen.queryByLabelText("Host")).not.toBeInTheDocument();
  });

  it("reports what is typed and chosen by key", async () => {
    const onChange = vi.fn();
    const user = userEvent.setup();
    render(<SchemaForm schema={SCHEMA} group="public" values={{}} onChange={onChange} />);

    await user.type(screen.getByLabelText("Host"), "h");
    await user.selectOptions(screen.getByLabelText("TLS mode (optional)"), "tls");

    expect(onChange).toHaveBeenCalledWith("host", "h");
    expect(onChange).toHaveBeenCalledWith("tls", "tls");
  });
});
