import { useState } from "react";

import { Button, CopyButton } from "../../ui";
import { kindText, mergedSpf, recordStatus, spfMechanisms, type DomainRecord } from "./labels";

function StatusIcon({ status }: { status: string }) {
  const common = { "aria-hidden": true, width: 16, height: 16, viewBox: "0 0 16 16", fill: "none", stroke: "currentColor", strokeWidth: 1.6, strokeLinecap: "round", strokeLinejoin: "round" } as const;
  if (status === "ok") {
    return (
      <svg {...common}>
        <circle cx="8" cy="8" r="6.25" />
        <path d="M5.5 8.2l1.8 1.8 3.3-3.6" />
      </svg>
    );
  }
  if (status === "mismatch") {
    return (
      <svg {...common}>
        <circle cx="8" cy="8" r="6.25" />
        <path d="M6 6l4 4M10 6l-4 4" />
      </svg>
    );
  }
  if (status === "missing") {
    return (
      <svg {...common}>
        <circle cx="8" cy="8" r="6.25" strokeDasharray="2.4 2.2" />
        <path d="M5.5 8h5" />
      </svg>
    );
  }
  return (
    <svg {...common}>
      <circle cx="8" cy="8" r="6.25" />
      <path d="M8 4.5V8l2.2 1.6" />
    </svg>
  );
}

function ValueRow({ label, value, copyLabel, bad = false }: { label: string; value: string; copyLabel?: string; bad?: boolean }) {
  return (
    <>
      <span className={bad ? "rec-label rec-label-bad" : "rec-label"}>{label}</span>
      <span className={bad ? "rec-value rec-value-bad" : "rec-value"}>{value}</span>
      {copyLabel ? <CopyButton value={value} label={copyLabel} /> : <span />}
    </>
  );
}

interface RecordStepProps {
  number: number;
  record: DomainRecord;
  last: boolean;
}

/** One record of the checklist: what to publish, with copy buttons, and what the last check found. */
export function RecordStep({ number, record, last }: RecordStepProps) {
  const text = kindText(record.kind);
  const view = recordStatus(record.status);
  const done = view.status === "ok";
  const [shown, setShown] = useState(false);
  const name = text.title;
  const observed = record.observed_text;
  const merged = record.kind === "spf" && view.status === "mismatch" ? mergedSpf(observed, record.expected_text) : null;

  return (
    <li className="rec-step">
      <div className="rec-rail">
        <span className={done ? "rec-number rec-number-done" : "rec-number"}>
          {done ? (
            <>
              <svg aria-hidden="true" width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
                <path d="M3.5 8.5l3 3 6-7" />
              </svg>
              <span className="visually-hidden">Step {number}, done</span>
            </>
          ) : (
            <>
              <span aria-hidden="true">{number}</span>
              <span className="visually-hidden">Step {number}</span>
            </>
          )}
        </span>
        {last ? null : <span className="rec-line" aria-hidden="true" />}
      </div>
      <section className="rec-card" aria-label={`${name} record`}>
        <div className="rec-head">
          <h2 className="rec-title">{name}</h2>
          <span className="rec-tag">TXT</span>
          <span className="muted">{text.purpose}</span>
          <span className={view.className} data-status={view.status}>
            <StatusIcon status={view.status} />
            <span>
              {view.word}: {view.reason}
            </span>
          </span>
        </div>
        {done && !shown ? (
          <div className="row">
            <span className="mono rec-value-plain">{record.host}</span>
            <Button onClick={() => setShown(true)} aria-expanded={false} aria-label={`Show the ${name} record`}>
              Show the record
            </Button>
          </div>
        ) : (
          <div className="rec-grid">
            <ValueRow label="Host" value={record.host} copyLabel={`Copy the ${name} host`} />
            <ValueRow label="Publish" value={record.expected_text} copyLabel={`Copy the ${name} value`} />
            {view.status === "mismatch" || (view.status === "missing" && observed.length > 0)
              ? observed.map((found, at) => <ValueRow key={at} label="We found" value={found} bad />)
              : null}
          </div>
        )}
        {merged ? (
          <div className="rec-note">
            <p>
              A host can have only one SPF record, so do not add a second one. Add{" "}
              <span className="mono">{spfMechanisms(record.expected_text).join(" ")}</span> to the one that is there:
            </p>
            <div className="rec-grid">
              <ValueRow label="Merged" value={merged} copyLabel="Copy the merged SPF value" />
            </div>
          </div>
        ) : null}
        {record.checked_at ? null : <p className="faint">Not checked yet.</p>}
      </section>
    </li>
  );
}
