import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it } from "vitest";

import { flowRoutes } from "../../test/flowsApi";
import { mountEditor as mount, openInspectorOf } from "../../test/flowsEditor";
import { mockApi } from "../../test/mockApi";
import { stubReactFlow } from "../../test/reactFlowStubs";
import { issueText } from "./issues";
import type { FlowIssue } from "./model";

beforeEach(() => {
  stubReactFlow();
});

const ISSUES = [
  { code: "invalid_url", node: "n4", at: "/url" },
  { code: "invalid_cron", at: "/trigger/cron" },
];

describe("validation on the canvas", () => {
  it("marks the step with its issue and lists every issue in words", async () => {
    mockApi(flowRoutes({ issues: ISSUES }));
    const user = userEvent.setup();
    mount();

    expect(await screen.findByRole("group", { name: /^Webhook: .*, step 4 of 5, 1 issue$/ })).toBeInTheDocument();
    await user.click(await screen.findByRole("button", { name: "2 issues" }));

    const list = screen.getByRole("list", { name: "Issues in this flow" });
    expect(within(list).getByText("The web address is not valid. Use an http or https address.")).toBeInTheDocument();
    expect(within(list).getByText("The schedule is not a valid cron expression.")).toBeInTheDocument();
  });

  it("selects and focuses the step an issue is on", async () => {
    mockApi(flowRoutes({ issues: ISSUES }));
    const user = userEvent.setup();
    mount();
    await user.click(await screen.findByRole("button", { name: "2 issues" }));

    await user.click(screen.getByRole("button", { name: /The web address is not valid/ }));

    const step = await screen.findByRole("group", { name: /^Webhook: / });
    await waitFor(() => expect(step).toHaveFocus());
    const sheet = await screen.findByRole("region", { name: "Step inspector" });
    expect(within(sheet).getByText("This step has a problem")).toBeInTheDocument();
    expect(within(sheet).getByLabelText("URL")).toHaveAccessibleDescription(/not valid/);
  });

  it("opens the flow settings at the section an issue points into", async () => {
    mockApi(flowRoutes({ issues: ISSUES }));
    const user = userEvent.setup();
    mount();
    await user.click(await screen.findByRole("button", { name: "2 issues" }));

    await user.click(screen.getByRole("button", { name: /not a valid cron expression/ }));

    expect(await screen.findByRole("region", { name: "Flow settings" })).toBeInTheDocument();
    const heading = await screen.findByRole("heading", { name: "Trigger" });
    await waitFor(() => expect(heading).toHaveFocus());
    const section = screen.getByRole("region", { name: "Trigger" });
    expect(within(section).getByText("The schedule is not a valid cron expression.")).toBeInTheDocument();
  });

  it("checks again after a save and clears an issue that is fixed", async () => {
    mockApi(flowRoutes({ issues: (asked) => (asked === 1 ? [{ code: "invalid_url", node: "n4", at: "/url" }] : []) }));
    const user = userEvent.setup();
    mount();
    const sheet = await openInspectorOf(user, /^Webhook: /);
    const url = within(sheet).getByLabelText("URL");

    await user.clear(url);
    await user.type(url, "https://crm.example.com/v2");

    expect(await screen.findByText("No issues", undefined, { timeout: 4000 })).toBeInTheDocument();
  });

  it("names the output of an unwired port", () => {
    const graph = {
      v: 1 as const,
      nodes: [{ id: "n1", type: "branch" as const, conditions: [{ trait: "city", op: "eq" as const }] }],
      edges: [],
    };
    const issue: FlowIssue = { code: "port_not_wired", node: "n1", at: "/ports/case_0" };

    expect(issueText(issue, graph)).toBe("The Yes output is not connected to a next step.");
  });

  it("falls back to the raw code for a code it has no words for", () => {
    const issue = { code: "something_new" } as unknown as FlowIssue;

    expect(issueText(issue)).toBe("This flow has a problem (something_new).");
  });
});
