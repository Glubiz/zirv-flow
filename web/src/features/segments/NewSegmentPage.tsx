import { useState } from "react";
import { useParams } from "react-router";

import { EmptyState, PageHeader, Panel } from "../../ui";
import { AssistantToggle, useAssistantAccess, useAssistantDock } from "../assistant";
import { useSegmentAccess } from "./access";
import { SegmentForm } from "./SegmentForm";

export function NewSegmentPage() {
  const { ws = "" } = useParams();
  return <NewSegment key={ws} workspaceId={ws} />;
}

function NewSegment({ workspaceId }: { workspaceId: string }) {
  const { canWrite, canUseAssistant } = useSegmentAccess(workspaceId);
  const { enabled: assistantOn, reload: reloadAssistant } = useAssistantAccess(workspaceId, canUseAssistant);
  const dock = useAssistantDock();
  const [dockSlot, setDockSlot] = useState<HTMLElement | null>(null);
  return (
    <div className="dock-layout">
      <div className="page">
        <PageHeader
          title="New segment"
          breadcrumb={{ group: "Audience", section: "Segments", to: `/w/${workspaceId}/segments` }}
          lede="Say who belongs, and watch the count as you build the rule."
          actions={canWrite && assistantOn ? <AssistantToggle open={dock.open} onToggle={dock.toggle} /> : null}
        />
        {canWrite ? (
          <SegmentForm workspaceId={workspaceId} segment={null} canWrite assistant={assistantOn} onAssistantGone={reloadAssistant} dockSlot={dockSlot} />
        ) : (
          <Panel flush>
            <EmptyState title="You cannot create segments">Ask a member of the workspace to create one, or to give you a role.</EmptyState>
          </Panel>
        )}
      </div>
      <div className="dock-slot" ref={setDockSlot} />
    </div>
  );
}
