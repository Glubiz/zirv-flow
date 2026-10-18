import { client, unwrap, useResource } from "../../api";

/**
 * Whether the workspace's assistant is on. Only an admin may read the settings, so the question is
 * asked only when `allowed` (the person is an admin); for anyone else it is never sent and the
 * answer is false. While it loads, and when it cannot be read, the answer is false: the assistant
 * is offered only once it is known to be on.
 */
export function useAssistantEnabled(workspaceId: string, allowed: boolean): boolean {
  const settings = useResource(
    () =>
      allowed
        ? unwrap(client.GET("/api/v1/workspaces/{ws}/ai/settings", { params: { path: { ws: workspaceId } } }))
        : Promise.resolve(null),
    [workspaceId, allowed],
  );
  return settings.status === "ready" && settings.data !== null && settings.data.enabled;
}
