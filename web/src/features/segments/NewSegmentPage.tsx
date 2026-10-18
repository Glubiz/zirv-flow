import { Link, useParams } from "react-router";

import { EmptyState, PageHeader, Panel } from "../../ui";
import { useAssistantEnabled } from "../assistant";
import { useSegmentAccess } from "./access";
import { SegmentForm } from "./SegmentForm";

export function NewSegmentPage() {
  const { ws = "" } = useParams();
  return <NewSegment key={ws} workspaceId={ws} />;
}

function NewSegment({ workspaceId }: { workspaceId: string }) {
  const { canWrite, canUseAssistant } = useSegmentAccess(workspaceId);
  const assistantOn = useAssistantEnabled(workspaceId, canUseAssistant);
  return (
    <div className="page">
      <p>
        <Link to={`/w/${workspaceId}/segments`}>All segments</Link>
      </p>
      <PageHeader title="New segment" lede="Say who belongs, and watch the count as you build the rule." />
      {canWrite ? (
        <SegmentForm workspaceId={workspaceId} segment={null} canWrite assistant={assistantOn} />
      ) : (
        <Panel flush>
          <EmptyState title="You cannot create segments">Ask a member of the workspace to create one, or to give you a role.</EmptyState>
        </Panel>
      )}
    </div>
  );
}
