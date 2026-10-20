import { screen } from "@testing-library/react";
import type userEvent from "@testing-library/user-event";

import { FlowPage } from "../features/flows/FlowPage";
import { FLOW_ID, WS } from "./flowsApi";
import type { Call } from "./mockApi";
import { renderRoute } from "./renderFeature";

/** Helpers for tests that drive the flow editor page. */

export type User = ReturnType<typeof userEvent.setup>;

export function mountEditor() {
  return renderRoute(`/w/ws1/flows/${FLOW_ID}`, "/w/:ws/flows/:id", <FlowPage />);
}

/** The bodies of the saves so far, oldest first. */
export function saves(calls: Call[]): Record<string, unknown>[] {
  return calls
    .filter((call) => call.method === "PATCH" && call.path === `${WS}/flows/${FLOW_ID}`)
    .map((call) => call.body as Record<string, unknown>);
}

export interface SavedGraph {
  nodes: { id: string; [key: string]: unknown }[];
  edges: { id: string; from: string; port: string; to: string }[];
}

/** The graph of the newest save, or null when nothing has been saved or the save held no graph. */
export function lastGraph(calls: Call[]): SavedGraph | null {
  const last = saves(calls).at(-1) as { graph?: SavedGraph } | undefined;
  return last?.graph ?? null;
}

/** Clicks the step with this accessible name and returns the sheet that opens. */
export async function openSheetOf(user: User, name: RegExp | string) {
  await user.click(await screen.findByRole("group", { name }));
  return screen.findByRole("dialog");
}
