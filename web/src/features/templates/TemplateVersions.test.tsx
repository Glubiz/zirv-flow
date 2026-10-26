import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { mockApi } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { previewBody, templateRoutes, TEMPLATE_ID, useBody } from "../../test/templatesApi";
import { TemplatePage } from "./TemplatePage";

function mount() {
  return renderRoute(`/w/ws1/templates/${TEMPLATE_ID}`, "/w/:ws/templates/:id", <TemplatePage />);
}

const WAIT = { timeout: 4000 };

async function ready() {
  const publish = await screen.findByRole("button", { name: "Publish…" });
  await waitFor(() => expect(publish).toBeEnabled(), WAIT);
  return publish;
}

describe("publishing", () => {
  it("opens the Review and publish page once the draft is saved", async () => {
    mockApi(templateRoutes());
    const user = userEvent.setup();
    const { router } = mount();

    await user.click(await ready());

    await waitFor(() => expect(router.state.location.pathname).toBe(`/w/ws1/templates/${TEMPLATE_ID}/publish`), WAIT);
  });

  it("hides publishing from a member who is not an admin, and offers the test send", async () => {
    mockApi(templateRoutes({ role: "member" }));
    mount();

    await screen.findByTitle("Preview of the message");
    expect(screen.queryByRole("button", { name: "Publish…" })).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Send a test" })).toBeInTheDocument();
  });

  it("waits for the preview before it allows a publish", async () => {
    mockApi(templateRoutes({ preview: () => previewBody() }));
    mount();

    expect(await screen.findByRole("button", { name: "Publish…" })).toBeDisabled();
    await ready();
  });
});

describe("versions and where used", () => {
  it("shows where the template is used, active or not", async () => {
    mockApi(templateRoutes({ uses: [useBody(), useBody({ flow_id: "f2", flow_name: "Paused flow", active: false, flow_state: "paused" })] }));
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("tab", { name: "Where used" }));

    const list = await screen.findByRole("list", { name: "Flows that use this template" });
    expect(within(list).getByRole("link", { name: "Welcome series" })).toHaveAttribute("href", expect.stringContaining("/flows/"));
    expect(within(list).getByText("Active")).toBeInTheDocument();
    expect(within(list).getByText("Not running now")).toBeInTheDocument();
  });
});
