import { createContext, useContext, type ReactNode } from "react";

import { useSenderOptions, type Resource, type components } from "../../api";

export type SenderOptions = Resource<components["schemas"]["SenderIdentityOptionBody"][]>;

const Shared = createContext<SenderOptions | null>(null);

function Own({ workspaceId, children }: { workspaceId: string; children: (senders: SenderOptions) => ReactNode }) {
  return <>{children(useSenderOptions(workspaceId))}</>;
}

/** Reads the sender options once for everything below it that picks a sender. Off, each reader reads them itself. */
export function SenderOptionsProvider({ workspaceId, enabled, children }: { workspaceId: string; enabled: boolean; children: ReactNode }) {
  if (!enabled) return <>{children}</>;
  return <Own workspaceId={workspaceId}>{(senders) => <Shared.Provider value={senders}>{children}</Shared.Provider>}</Own>;
}

/** The sender options of the nearest provider, or a read of its own when there is none. */
export function WithSenderOptions({ workspaceId, children }: { workspaceId: string; children: (senders: SenderOptions) => ReactNode }) {
  const shared = useContext(Shared);
  if (shared) return <>{children(shared)}</>;
  return <Own workspaceId={workspaceId}>{children}</Own>;
}
