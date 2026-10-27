import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { json, mockApi } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { emailDocument, previewBody, templateRoutes, TEMPLATE_ID, useBody, WS } from "../../test/templatesApi";
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
  it("lists the versions and shows one read only", async () => {
    mockApi({
      ...templateRoutes({
        versions: [{ version: 2, published_at: "2026-10-05T09:00:00Z", blocking: 0, warnings: 1, published_by: null }],
      }),
      [`GET ${WS}/templates/${TEMPLATE_ID}/versions/2`]: () =>
        json({
          version: 2,
          published_at: "2026-10-05T09:00:00Z",
          document: emailDocument({}, { subject: "Old subject", html: "<p>old body</p>" }),
          warnings: [{ code: "insecure_link", severity: "warning", at: "/email/html" }],
        }),
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("tab", { name: "Versions" }));
    await user.click(await screen.findByRole("button", { name: "View version 2" }));

    const view = await screen.findByRole("region", { name: "Version 2" });
    expect(await within(view).findByLabelText("Subject")).toHaveTextContent("Old subject");
    expect(within(view).getByLabelText("HTML source")).toHaveTextContent("<p>old body</p>");
    expect(within(view).getByText(/A link uses http instead of https/)).toBeInTheDocument();
    expect(within(view).queryByRole("textbox")).not.toBeInTheDocument();
  });

  it("shows an empty state before the first publish", async () => {
    mockApi(templateRoutes());
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("tab", { name: "Versions" }));

    expect(await screen.findByText("Nothing published yet")).toBeInTheDocument();
  });

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

  it("shows an error with a retry when the versions cannot load", async () => {
    let attempts = 0;
    mockApi({
      ...templateRoutes(),
      [`GET ${WS}/templates/${TEMPLATE_ID}/versions`]: () => {
        attempts += 1;
        return attempts === 1 ? new Response("{}", { status: 500, headers: { "content-type": "application/json" } }) : json([]);
      },
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("tab", { name: "Versions" }));
    await user.click(await screen.findByRole("button", { name: "Try again" }));

    expect(await screen.findByText("Nothing published yet")).toBeInTheDocument();
  });
});
