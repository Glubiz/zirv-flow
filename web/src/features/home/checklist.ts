import type { components } from "../../api";

export interface SetupFacts {
  sources: Pick<components["schemas"]["SourceBody"], "id">[];
  domains: Pick<components["schemas"]["SendingDomainBody"], "status">[];
  destinations: Pick<components["schemas"]["DestinationBody"], "archived">[];
  templates: Pick<components["schemas"]["TemplateBody"], "archived_at">[];
  segments: Pick<components["schemas"]["SegmentBody"], "archived_at">[];
  flows: Pick<components["schemas"]["FlowBody"], "state" | "current_version">[];
}

export function setupChecklist(data: SetupFacts) {
  const flows = data.flows.filter((flow) => flow.state !== "archived");
  return [
    { label: "Connect a source", route: "sources", done: data.sources.length > 0 },
    { label: "Verify a sending domain", route: "sending-domains", done: data.domains.some((domain) => domain.status === "verified") },
    { label: "Add a destination", route: "destinations", done: data.destinations.some((destination) => !destination.archived) },
    { label: "Create a template", route: "templates", done: data.templates.some((template) => !template.archived_at) },
    { label: "Create a segment", route: "segments", done: data.segments.some((segment) => !segment.archived_at) },
    { label: "Build a flow", route: "flows", done: flows.length > 0 },
    { label: "Publish a flow", route: "flows", done: flows.some((flow) => flow.current_version != null) },
  ];
}
