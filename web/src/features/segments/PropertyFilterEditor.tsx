import { describeError } from "../../api";
import { ErrorState, LoadingState } from "../../ui";
import type { EventNode, Node, PropertyFilter } from "./ast";
import { BuilderContext, type BuilderApi } from "./builderContext";
import { useCatalog, useEventTypes } from "./catalog";
import { PropertyFilters } from "./Conditions";

interface PropertyFilterEditorProps {
  workspaceId: string;
  /** The exact name of the event whose properties the filters read. */
  eventName: string;
  /** The segment `where` list: every filter must hold. */
  value: PropertyFilter[];
  onChange: (next: PropertyFilter[]) => void;
  readOnly?: boolean;
}

/**
 * Edits the property filters of one event, the segment `where` list, with the same controls the
 * segment builder uses. A forwarding rule keeps this list as its filter.
 */
export function PropertyFilterEditor({ workspaceId, eventName, value, onChange, readOnly = false }: PropertyFilterEditorProps) {
  const catalog = useCatalog(workspaceId);
  const eventTypes = useEventTypes(workspaceId, [eventName]);
  if (catalog.status === "loading") return <LoadingState label="Loading the properties to choose from" />;
  if (catalog.status === "error") return <ErrorState {...describeError(catalog.error)} onRetry={catalog.reload} />;

  const node: EventNode = value.length > 0 ? { event: eventName, where: value } : { event: eventName };
  const api: BuilderApi = {
    readOnly,
    selfId: null,
    mode: "segment",
    dimensions: [],
    catalog: catalog.catalog,
    eventTypes,
    issues: [],
    edit: (_path, next: Node) => onChange("where" in next && next.where ? next.where : []),
    add: () => undefined,
    remove: () => undefined,
    changeKind: () => undefined,
  };
  return (
    <BuilderContext.Provider value={api}>
      <PropertyFilters node={node} path={[]} update={(change) => onChange(change.where ?? [])} />
    </BuilderContext.Provider>
  );
}
