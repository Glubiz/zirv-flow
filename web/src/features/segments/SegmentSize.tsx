import { client, describeError, unwrap, useResource, type components } from "../../api";
import { Button, DayBars, EmptyState, InlineError, LoadingState, Panel } from "../../ui";

type SegmentSize = components["schemas"]["SegmentSizeBody"];

const DAYS = 30;

/** The text alternative of the chart, worked out from the series. */
export function sizeLabel(size: SegmentSize): string {
  const counts = size.days.map((day) => day.size);
  const at = (value: number) => value.toLocaleString("en");
  return `Segment size at the end of each of the last ${size.days.length} days, between ${at(Math.min(...counts))} and ${at(Math.max(...counts))} people, ${at(size.current)} now`;
}

/** How many people the segment held at the end of each day. */
export function SegmentSizePanel({ workspaceId, segmentId }: { workspaceId: string; segmentId: string }) {
  const size = useResource(
    () =>
      unwrap(
        client.GET("/api/v1/workspaces/{ws}/segments/{id}/size", {
          params: { path: { ws: workspaceId, id: segmentId }, query: { days: DAYS } },
        }),
      ),
    [workspaceId, segmentId],
  );
  return (
    <Panel title={`Size, last ${DAYS} days`}>
      {size.status === "loading" ? (
        <LoadingState label="Loading size" />
      ) : size.status === "error" ? (
        <div className="stack">
          <InlineError {...describeError(size.error)} />
          <Button onClick={size.reload}>Try again</Button>
        </div>
      ) : size.data.days.every((day) => day.size === 0) ? (
        <EmptyState title={`No one has been in this segment in the last ${DAYS} days`} />
      ) : (
        <>
          <p className="strong">{size.data.current.toLocaleString("en")} people now</p>
          <DayBars
            days={size.data.days.map((day) => ({ date: day.date, value: day.size }))}
            noun="people"
            valueLabel="Members"
            label={sizeLabel(size.data)}
            caption="Days are UTC."
          />
        </>
      )}
    </Panel>
  );
}
