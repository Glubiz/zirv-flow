import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter } from "react-router";
import { describe, expect, it } from "vitest";

import { json, mockApi, problem } from "../../test/mockApi";
import { SessionProvider } from "./SessionProvider";
import { SignInPage } from "./SignInPage";

function mount() {
  return render(
    <MemoryRouter>
      <SessionProvider>
        <SignInPage />
      </SessionProvider>
    </MemoryRouter>,
  );
}

describe("the sign-in page", () => {
  it("ties a failed sign-in to both fields and moves focus to the first", async () => {
    mockApi({
      "GET /api/v1/session": () => problem(401, "Unauthorized", "sign in first"),
      "GET /api/v1/version": () => json({ version: "1.0.0" }),
      "GET /api/v1/session/methods": () => json({ local: true, oidc: false }),
      "POST /api/v1/session": () => problem(401, "Unauthorized", "The email or password is wrong."),
    });
    const user = userEvent.setup();
    mount();

    const email = await screen.findByLabelText("Email");
    const password = screen.getByLabelText("Password");
    expect(email).not.toHaveAttribute("aria-invalid");
    await user.type(email, "ada@example.test");
    await user.type(password, "wrong");
    await user.click(screen.getByRole("button", { name: "Sign in" }));

    const alert = await screen.findByRole("alert");
    await waitFor(() => expect(email).toHaveFocus());
    for (const field of [email, password]) {
      expect(field).toHaveAttribute("aria-invalid", "true");
      expect(field).toHaveAttribute("aria-describedby", alert.id);
    }
    expect(alert.id).not.toBe("");
  });
});
