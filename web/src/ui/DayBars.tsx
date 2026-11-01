import { useState } from "react";

import { formatDay } from "./format";

export interface DayValue {
  /** The calendar day, `YYYY-MM-DD`. */
  date: string;
  value: number;
}

interface DayBarsProps {
  /** One item per day, oldest first, zero days included. */
  days: DayValue[];
  /** What a bar counts, as it reads after the number: "people entered". */
  noun: string;
  /** The text alternative of the whole chart, worked out from the data by the caller. */
  label: string;
  /** The heading of the table's value column. */
  valueLabel: string;
  caption?: string;
}

/** The top of the scale: the max rounded up to 1, 2 or 5 times a power of ten. */
function niceTop(max: number): number {
  if (max <= 1) return 1;
  const unit = 10 ** Math.floor(Math.log10(max));
  const step = [1, 2, 5, 10].find((candidate) => candidate * unit >= max) ?? 10;
  return step * unit;
}

/** A bar per day, scaled to the largest, with the first and last day under it and the same data as a table. */
export function DayBars({ days, noun, label, valueLabel, caption }: DayBarsProps) {
  const [hover, setHover] = useState<number | null>(null);
  const top = niceTop(Math.max(0, ...days.map((day) => day.value)));
  const first = days[0];
  const last = days[days.length - 1];
  const hovered = hover === null ? undefined : days[hover];
  return (
    <figure className="daybars">
      <div className="daybars-chart" role="img" aria-label={label}>
        <div className="daybars-y mono" aria-hidden="true">
          <span>{top.toLocaleString("en")}</span>
          <span>0</span>
        </div>
        <div className="daybars-plot" onMouseLeave={() => setHover(null)}>
          {days.map((day, index) => (
            <div key={day.date} className="daybars-col" onMouseEnter={() => setHover(index)}>
              <span className="daybars-bar" style={{ height: `${(day.value / top) * 100}%` }} />
            </div>
          ))}
          {hovered ? (
            <div className="daybars-tip" style={{ left: `${((hover ?? 0) + 0.5) * (100 / days.length)}%` }} aria-hidden="true">
              {formatDay(hovered.date)}: {hovered.value.toLocaleString("en")} {noun}
            </div>
          ) : null}
        </div>
        <div className="daybars-x mono" aria-hidden="true">
          <span>{first ? formatDay(first.date) : ""}</span>
          <span>{last ? formatDay(last.date) : ""}</span>
        </div>
      </div>
      {caption ? <figcaption className="daybars-caption">{caption}</figcaption> : null}
      <details className="daybars-table">
        <summary>Show as table</summary>
        <table className="table">
          <caption className="visually-hidden">{label}</caption>
          <thead>
            <tr>
              <th scope="col">Date</th>
              <th scope="col">{valueLabel}</th>
            </tr>
          </thead>
          <tbody>
            {days.map((day) => (
              <tr key={day.date}>
                <th scope="row">{formatDay(day.date)}</th>
                <td>{day.value.toLocaleString("en")}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </details>
    </figure>
  );
}
