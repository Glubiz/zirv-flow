import { client, describeError, unwrap, useResource, type components } from "../../api";
import { Button, DayBars, EmptyState, InlineError, LoadingState, Panel } from "../../ui";

export type FlowEntries = components["schemas"]["EntriesBody"];

const DAYS = 30;

export function useFlowEntries(workspaceId: string, flowId: string, published: boolean) {
  return useResource(async (): Promise<FlowEntries | null> => {
    if (!published) return null;
    return unwrap(
      client.GET("/api/v1/workspaces/{ws}/flows/{id}/entries", {
        params: { path: { ws: workspaceId, id: flowId }, query: { days: DAYS } },
      }),
    );
  }, [workspaceId, flowId, published]);
}

/** The text alternative of the chart, worked out from the series. */
export function entriesLabel(entries: FlowEntries): string {
  const counts = entries.days.map((day) => day.entered);
  const at = (value: number) => value.toLocaleString("en");
  return `People entering per day over the last ${entries.days.length} days, between ${at(Math.min(...counts))} and ${at(Math.max(...counts))} a day, ${at(entries.total)} in total`;
}

function change(entries: FlowEntries): string {
  const base = `${entries.total.toLocaleString("en")} in ${entries.days.length} days`;
  if (entries.previous_total === 0) return base;
  const percent = Math.round(((entries.total - entries.previous_total) / entries.previous_total) * 100);
  return `${base} · ${percent >= 0 ? "+" : "−"}${Math.abs(percent)}% on the ${entries.days.length} days before`;
}

/** How many people entered the flow each day. */
export function EntriesPanel({ workspaceId, flowId, published }: { workspaceId: string; flowId: string; published: boolean }) {
  const entries = useFlowEntries(workspaceId, flowId, published);
  return (
    <Panel title="Entered per day">
      {!published ? (
        <EmptyState title="Publish the flow to see entries" />
      ) : entries.status === "loading" ? (
        <LoadingState label="Loading entries" />
      ) : entries.status === "error" ? (
        <div className="stack">
          <InlineError {...describeError(entries.error)} />
          <Button onClick={entries.reload}>Try again</Button>
        </div>
      ) : !entries.data || entries.data.total === 0 ? (
        <EmptyState title={`No one has entered this flow in the last ${DAYS} days`} />
      ) : (
        <>
          <p className="strong">{change(entries.data)}</p>
          <DayBars
            days={entries.data.days.map((day) => ({ date: day.date, value: day.entered }))}
            noun="people entered"
            valueLabel="Entered"
            label={entriesLabel(entries.data)}
            caption="Days are UTC."
          />
        </>
      )}
    </Panel>
  );
}
