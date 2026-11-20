import { formatCount, formatDateTime, formatDateUtc } from "../../ui";

export interface Bar {
  start: string;
  count: number;
}

const WIDTH = 640;
const HEIGHT = 220;
const PAD = { top: 12, right: 12, bottom: 28, left: 48 };

/** Rounds a maximum up to a tidy axis top: 1, 2 or 5 times a power of ten. */
export function niceMax(value: number): number {
  if (value <= 1) return 1;
  const power = 10 ** Math.floor(Math.log10(value));
  const fraction = value / power;
  const step = fraction <= 1 ? 1 : fraction <= 2 ? 2 : fraction <= 5 ? 5 : 10;
  return step * power;
}

/** The fixed length of one bucket in UTC, as the server buckets with date_trunc. */
const STEP_MS: Record<string, number> = { hour: 3_600_000, day: 86_400_000, week: 604_800_000 };

/**
 * Where each bar sits, in slots from the left. Buckets are placed by their start time, so a quiet
 * stretch with no events keeps its width. An unknown unit or an unreadable date falls back to the
 * position in the list.
 */
function place(bars: readonly Bar[], unit: string): { positions: number[]; slots: number } {
  const step = STEP_MS[unit];
  const origin = bars[0] ? Date.parse(bars[0].start) : Number.NaN;
  if (step !== undefined && Number.isFinite(origin)) {
    const positions = bars.map((bar) => Math.round((Date.parse(bar.start) - origin) / step));
    if (positions.every((position) => Number.isFinite(position) && position >= 0)) {
      return { positions, slots: Math.max(1, ...positions.map((position) => position + 1)) };
    }
  }
  return { positions: bars.map((_, index) => index), slots: Math.max(1, bars.length) };
}

/**
 * Counts over time as an inline SVG bar chart, with the same numbers in a table for anyone who
 * cannot read the chart. The axis starts at zero.
 */
export function BarChart({ bars, unit }: { bars: readonly Bar[]; unit: string }) {
  const total = bars.reduce((sum, bar) => sum + bar.count, 0);
  const top = niceMax(Math.max(0, ...bars.map((bar) => bar.count)));
  const inner = { width: WIDTH - PAD.left - PAD.right, height: HEIGHT - PAD.top - PAD.bottom };
  const { positions, slots } = place(bars, unit);
  const slot = inner.width / slots;
  // The server cuts a day or a week at UTC midnight, so its date is shown in UTC. An hour is an
  // instant, so it reads in the browser's zone.
  const label = unit === "hour" ? formatDateTime : formatDateUtc;
  const first = bars[0];
  const last = bars.at(-1);
  const summary =
    first && last
      ? `Events per ${unit}: ${total.toLocaleString("en-US")} in total from ${label(first.start)} to ${label(last.start)}`
      : `Events per ${unit}: none`;

  return (
    <figure className="chart">
      <svg viewBox={`0 0 ${WIDTH} ${HEIGHT}`} role="img" aria-label={summary} preserveAspectRatio="xMidYMid meet">
        <line className="chart-axis" x1={PAD.left} x2={WIDTH - PAD.right} y1={PAD.top + inner.height} y2={PAD.top + inner.height} />
        <line className="chart-axis" x1={PAD.left} x2={PAD.left} y1={PAD.top} y2={PAD.top + inner.height} />
        <text className="chart-label" x={PAD.left - 8} y={PAD.top + 4} textAnchor="end">
          {formatCount(top)}
        </text>
        <text className="chart-label" x={PAD.left - 8} y={PAD.top + inner.height} textAnchor="end">
          0
        </text>
        {bars.map((bar, index) => {
          const height = (bar.count / top) * inner.height;
          return (
            <rect
              key={bar.start}
              className="chart-bar"
              x={PAD.left + (positions[index] ?? index) * slot + slot * 0.1}
              y={PAD.top + inner.height - height}
              width={Math.max(1, slot * 0.8)}
              height={height}
            >
              <title>{`${label(bar.start)}: ${bar.count.toLocaleString("en-US")}`}</title>
            </rect>
          );
        })}
        {first ? (
          <text className="chart-label" x={PAD.left} y={HEIGHT - 8} textAnchor="start">
            {label(first.start)}
          </text>
        ) : null}
        {last && bars.length > 1 ? (
          <text className="chart-label" x={WIDTH - PAD.right} y={HEIGHT - 8} textAnchor="end">
            {label(last.start)}
          </text>
        ) : null}
      </svg>
      <details className="chart-table">
        <summary>Show the numbers as a table</summary>
        <div className="table-scroll">
          <table className="table">
            <caption>Events per {unit}</caption>
            <thead>
              <tr>
                <th scope="col">Start of the {unit}{unit === "hour" ? "" : " (UTC)"}</th>
                <th scope="col" className="num">
                  Events
                </th>
              </tr>
            </thead>
            <tbody>
              {bars.map((bar) => (
                <tr key={bar.start}>
                  <td>{label(bar.start)}</td>
                  <td className="num">{bar.count.toLocaleString("en-US")}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      </details>
    </figure>
  );
}
