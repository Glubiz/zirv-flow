import { client, unwrap } from "./client";
import type { components } from "./schema";
import { useResource } from "./useResource";

/*
 * What a writer picks from: ids, names and state, never settings or secrets. These reads need the
 * write role; a read-only member gets a refusal and the pickers fall back to the chosen id.
 */

export function useDestinationOptions(workspaceId: string) {
  return useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/destinations/options", { params: { path: { ws: workspaceId } } })),
    [workspaceId],
  );
}

type DestinationOption = components["schemas"]["DestinationOptionBody"];

/** The destinations a step can name: not archived and able to do what it needs. */
export function offeredDestinations(options: readonly DestinationOption[], capability: "send" | "sync"): DestinationOption[] {
  return options.filter((entry) => !entry.archived && entry.capabilities[capability]);
}

export function useSenderOptions(workspaceId: string) {
  return useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/sender-identities/options", { params: { path: { ws: workspaceId } } })),
    [workspaceId],
  );
}

export function useAllowlistOptions(workspaceId: string) {
  return useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/test-send-allowlist/options", { params: { path: { ws: workspaceId } } })),
    [workspaceId],
  );
}
