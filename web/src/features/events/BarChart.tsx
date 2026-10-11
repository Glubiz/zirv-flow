import { formatDateTime } from "../../ui";

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

/**
 * Counts over time as an inline SVG bar chart, with the same numbers in a table for anyone who
 * cannot read the chart. The axis starts at zero.
 */
export function BarChart({ bars, unit }: { bars: readonly Bar[]; unit: string }) {
  const total = bars.reduce((sum, bar) => sum + bar.count, 0);
  const top = niceMax(Math.max(0, ...bars.map((bar) => bar.count)));
  const inner = { width: WIDTH - PAD.left - PAD.right, height: HEIGHT - PAD.top - PAD.bottom };
  const slot = bars.length === 0 ? inner.width : inner.width / bars.length;
  const first = bars[0];
  const last = bars.at(-1);
  const summary =
    first && last
      ? `Events per ${unit}: ${total} in total from ${formatDateTime(first.start)} to ${formatDateTime(last.start)}`
      : `Events per ${unit}: none`;

  return (
    <figure className="chart">
      <svg viewBox={`0 0 ${WIDTH} ${HEIGHT}`} role="img" aria-label={summary} preserveAspectRatio="xMidYMid meet">
        <line className="chart-axis" x1={PAD.left} x2={WIDTH - PAD.right} y1={PAD.top + inner.height} y2={PAD.top + inner.height} />
        <line className="chart-axis" x1={PAD.left} x2={PAD.left} y1={PAD.top} y2={PAD.top + inner.height} />
        <text className="chart-label" x={PAD.left - 8} y={PAD.top + 4} textAnchor="end">
          {top}
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
              x={PAD.left + index * slot + slot * 0.1}
              y={PAD.top + inner.height - height}
              width={Math.max(1, slot * 0.8)}
              height={height}
            >
              <title>{`${formatDateTime(bar.start)}: ${bar.count}`}</title>
            </rect>
          );
        })}
        {first ? (
          <text className="chart-label" x={PAD.left} y={HEIGHT - 8} textAnchor="start">
            {formatDateTime(first.start)}
          </text>
        ) : null}
        {last && bars.length > 1 ? (
          <text className="chart-label" x={WIDTH - PAD.right} y={HEIGHT - 8} textAnchor="end">
            {formatDateTime(last.start)}
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
                <th scope="col">Start of the {unit}</th>
                <th scope="col" className="num">
                  Events
                </th>
              </tr>
            </thead>
            <tbody>
              {bars.map((bar) => (
                <tr key={bar.start}>
                  <td>{formatDateTime(bar.start)}</td>
                  <td className="num">{bar.count}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      </details>
    </figure>
  );
}
