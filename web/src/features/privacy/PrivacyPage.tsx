import { useParams } from "react-router";

import { describeError } from "../../api";
import { EmptyState, ErrorState, LoadingState, PageHeader } from "../../ui";
import { useWorkspaceAccess } from "../members";
import { ErasureSection } from "./ErasureSection";
import { ExportSection } from "./ExportSection";
import { OrphanPolicySection } from "./OrphanPolicySection";

/** Erasure, export and the orphan policy. Only workspace admins and the owner see it. */
export function PrivacyPage() {
  const { ws = "" } = useParams();
  const access = useWorkspaceAccess(ws);

  return (
    <div className="page">
      <PageHeader
        title="Privacy"
        lede="Erase a person on a data-subject request, export what is stored about them, and decide what happens to people nothing holds any more."
      />
      {access.status === "loading" ? <LoadingState label="Checking your access" /> : null}
      {access.status === "error" ? <ErrorState {...describeError(access.error)} onRetry={access.reload} /> : null}
      {access.status === "ready" && !access.data.canAdmin ? (
        <EmptyState title="Only workspace admins can use this page">
          Ask an admin of this workspace, or the instance owner, to handle erasure and export requests.
        </EmptyState>
      ) : null}
      {access.status === "ready" && access.data.canAdmin ? (
        <div className="stack" key={ws}>
          <ErasureSection workspaceId={ws} />
          <ExportSection workspaceId={ws} />
          <OrphanPolicySection workspaceId={ws} />
        </div>
      ) : null}
    </div>
  );
}
