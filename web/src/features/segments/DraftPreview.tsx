import { useMemo } from "react";

import { fromDefinition, type Definition } from "./ast";
import { LiveCount } from "./LiveCount";
import { useLivePreview } from "./useLivePreview";

/**
 * What a drafted definition would match, counted by the same preview request the builder uses.
 * The assistant panel renders this for the draft it shows; the segments feature hands it over as a
 * render prop, so the assistant never imports from here. An incomplete or refused draft shows the
 * count's own states.
 */
export function DraftPreview({ workspaceId, content }: { workspaceId: string; content: unknown }) {
  const root = useMemo(() => fromDefinition(content as Definition), [content]);
  const preview = useLivePreview(workspaceId, root, () => undefined);
  return <LiveCount workspaceId={workspaceId} preview={preview} />;
}
