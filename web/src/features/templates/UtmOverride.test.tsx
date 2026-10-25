import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { mockApi, problem } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { templateDetail, templateRoutes, TEMPLATE_ID } from "../../test/templatesApi";
import { TemplatePage } from "./TemplatePage";

function mount() {
  return renderRoute(`/w/ws1/templates/${TEMPLATE_ID}`, "/w/:ws/templates/:id", <TemplatePage />);
}

describe("the UTM override of a template", () => {
  it("follows the workspace by default and can be set to off", async () => {
    const calls = mockApi(templateRoutes());
    const user = userEvent.setup();
    mount();

    const select = await screen.findByRole("combobox", { name: "UTM parameters on links" });
    expect(select).toHaveValue("inherit");
    await user.selectOptions(select, "off");

    await waitFor(() => expect(calls.some((call) => call.method === "PUT" && !call.path.endsWith("/draft"))).toBe(true));
    expect(calls.find((call) => call.method === "PUT" && !call.path.endsWith("/draft"))?.body).toEqual({ utm: "off" });
    await waitFor(() => expect(screen.getByRole("combobox", { name: "UTM parameters on links" })).toHaveValue("off"));
  });

  it("shows a stored override and can go back to inheriting", async () => {
    const calls = mockApi(templateRoutes({ detail: templateDetail({ utm: "off" }) }));
    const user = userEvent.setup();
    mount();

    const select = await screen.findByRole("combobox", { name: "UTM parameters on links" });
    expect(select).toHaveValue("off");
    await user.selectOptions(select, "inherit");

    await waitFor(() => expect(calls.some((call) => call.method === "PUT" && !call.path.endsWith("/draft"))).toBe(true));
    expect(calls.find((call) => call.method === "PUT" && !call.path.endsWith("/draft"))?.body).toEqual({ utm: "inherit" });
  });

  it("is read only for a viewer who cannot write", async () => {
    mockApi(templateRoutes({ role: "reader" }));
    mount();

    expect(await screen.findByRole("combobox", { name: "UTM parameters on links" })).toBeDisabled();
  });

  it("shows the refusal and keeps the stored value", async () => {
    mockApi({
      ...templateRoutes(),
      [`PUT /api/v1/workspaces/ws1/templates/${TEMPLATE_ID}`]: () => problem(422, "Unprocessable", "the template is archived"),
    });
    const user = userEvent.setup();
    mount();

    const select = await screen.findByRole("combobox", { name: "UTM parameters on links" });
    await user.selectOptions(select, "off");

    expect(await screen.findByText(/the template is archived/)).toBeInTheDocument();
    expect(screen.getByRole("combobox", { name: "UTM parameters on links" })).toHaveValue("inherit");
  });

  it("is not offered for a webhook template", async () => {
    mockApi(
      templateRoutes({
        detail: templateDetail({ channel: "webhook", category: "transactional" }, { schema: 1, channel: "webhook", mode: "code", webhook: { headers: {}, body: "{}" } }),
      }),
    );
    mount();

    await screen.findByRole("heading", { name: "Trial nudge" });
    expect(screen.queryByRole("combobox", { name: "UTM parameters on links" })).not.toBeInTheDocument();
  });
});

