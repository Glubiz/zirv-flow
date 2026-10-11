import { client, unwrap, useResource, type Resource } from "../../api";

export function useSetupStatus(): Resource<{ required: boolean }> {
  return useResource(() => unwrap(client.GET("/api/v1/setup")), []);
}
