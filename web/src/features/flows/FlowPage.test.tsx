import { screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it } from "vitest";

import { FLOW_ID, flowRoutes, WS } from "../../test/flowsApi";
import { mountEditor as mount } from "../../test/flowsEditor";
import { mockApi, problem } from "../../test/mockApi";
import { stubReactFlow } from "../../test/reactFlowStubs";

beforeEach(() => {
  stubReactFlow();
});

describe("flow editor states", () => {
  it("shows a loading state while the flow loads", async () => {
    mockApi({ ...flowRoutes(), [`GET ${WS}/flows/${FLOW_ID}`]: () => new Promise<Response>(() => undefined) });
    mount();

    expect(await screen.findByText("Loading the flow")).toBeInTheDocument();
  });

  it("says so when the flow does not exist", async () => {
    mockApi({ ...flowRoutes(), [`GET ${WS}/flows/${FLOW_ID}`]: () => problem(404, "Not Found", "no such flow") });
    mount();

    expect(await screen.findByText("This flow does not exist")).toBeInTheDocument();
  });

  it("shows a retry when loading fails, and loads again", async () => {
    let attempts = 0;
    const routes = flowRoutes();
    const load = routes[`GET ${WS}/flows/${FLOW_ID}`]!;
    mockApi({
      ...routes,
      [`GET ${WS}/flows/${FLOW_ID}`]: (request) => {
        attempts += 1;
        return attempts === 1 ? problem(500, "Internal Server Error", "internal error") : load(request);
      },
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Try again" }));

    expect(await screen.findByRole("group", { name: /^Wait: 1 day/ })).toBeInTheDocument();
    expect(attempts).toBe(2);
  });
});

describe("flow editor canvas", () => {
  it("draws the trigger and every step with a name that says where it is", async () => {
    mockApi(flowRoutes());
    mount();

    expect(await screen.findByRole("group", { name: "Trigger: Enters Big spenders" })).toBeInTheDocument();
    expect(screen.getByRole("group", { name: "Wait: 1 day, step 1 of 5" })).toBeInTheDocument();
    expect(screen.getByRole("group", { name: /^Branch: .*, step 2 of 5$/ })).toBeInTheDocument();
    expect(screen.getByRole("group", { name: "Exit: Leaves the flow, step 5 of 5" })).toBeInTheDocument();
    expect(screen.getByLabelText(/^Yes output of /)).toBeInTheDocument();
    expect(screen.getByLabelText(/^No output of /)).toBeInTheDocument();
    expect(screen.getByRole("heading", { name: "Welcome series" })).toBeInTheDocument();
    expect(screen.getByRole("navigation", { name: "Flow views" })).toBeInTheDocument();
  });

  it("labels the outputs of an event wait and a random split", async () => {
    const graph = {
      v: 1,
      nodes: [
        { id: "n1", type: "wait.until_event", event: "purchase", timeout_seconds: 259_200 },
        { id: "n2", type: "split.random", arms: [{ weight: 30 }, { weight: 70 }] },
        { id: "n3", type: "exit" },
      ],
      edges: [
        { id: "e1", from: "n1", port: "event", to: "n3" },
        { id: "e2", from: "n1", port: "timeout", to: "n2" },
        { id: "e3", from: "n2", port: "arm_0", to: "n3" },
        { id: "e4", from: "n2", port: "arm_1", to: "n3" },
      ],
    };
    mockApi(flowRoutes({ flow: { graph } }));
    mount();

    expect(await screen.findByLabelText(/^Arrived output of /)).toBeInTheDocument();
    expect(screen.getByLabelText(/^Timed out output of /)).toBeInTheDocument();
    expect(screen.getByLabelText(/^Arm 1 \(30%\) output of /)).toBeInTheDocument();
    expect(screen.getByLabelText(/^Arm 2 \(70%\) output of /)).toBeInTheDocument();
  });
});
