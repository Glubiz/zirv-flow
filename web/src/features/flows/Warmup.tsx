import { useState } from "react";

import type { components } from "../../api";
import { Button, Dialog } from "../../ui";
import { DurationField } from "./fields";

export type Warmup = components["schemas"]["WarmupBody"];

export const WARMUP_MIN_SECONDS = 60;
export const WARMUP_MAX_SECONDS = 86_400;

export interface WarmupProgress {
  /** Whole percent of the window that has passed, 0 to 99. */
  percent: number;
  /** Seconds left in the window, at least 1. */
  left: number;
}

/** Where the window is at `now`, or null when it is not holding anyone back. */
export function warmupProgress(warmup: Warmup, now: number): WarmupProgress | null {
  if (!warmup.enabled || warmup.opened_at || !warmup.started_at || !warmup.ends_at) return null;
  const start = Date.parse(warmup.started_at);
  const end = Date.parse(warmup.ends_at);
  if (!(now < end)) return null;
  const share = (now - start) / (end - start);
  return {
    percent: Math.min(99, Math.max(0, Math.floor(share * 100))),
    left: Math.max(1, Math.ceil((end - now) / 1000)),
  };
}

/** "3 min left", "under 1 min left", "2 h 5 min left". */
export function timeLeft(seconds: number): string {
  if (seconds <= 60) return "under 1 min left";
  const minutes = Math.ceil(seconds / 60);
  if (minutes < 120) return `${minutes} min left`;
  const hours = Math.floor(minutes / 60);
  const rest = minutes % 60;
  return rest === 0 ? `${hours} h left` : `${hours} h ${rest} min left`;
}

export function warmupLength(seconds: number): string {
  if (seconds % 3600 === 0) return `${seconds / 3600} h`;
  if (seconds % 60 === 0) return `${seconds / 60} min`;
  return `${seconds} s`;
}

interface LineProps {
  warmup: Warmup;
  progress: WarmupProgress;
  /** Admins may end the window early. */
  canOpen: boolean;
  busy: boolean;
  onOpen: () => void;
}

/** The state of a window that is running: how far along, how many are in and how many wait. */
export function WarmupLine({ warmup, progress, canOpen, busy, onOpen }: LineProps) {
  const [asking, setAsking] = useState(false);
  return (
    <div className="fl-warmup" role="status">
      <span>
        Warming up · {progress.percent}% · {timeLeft(progress.left)}
      </span>
      <span className="faint">
        {warmup.admitted} admitted · {warmup.waiting} waiting
      </span>
      {canOpen ? (
        <Button disabled={busy} onClick={() => setAsking(true)}>
          Open now
        </Button>
      ) : null}
      {asking ? (
        <Dialog title="Open the flow now?" onClose={() => setAsking(false)}>
          <p className="muted">
            Everyone waiting is let in at once. {warmup.waiting} {warmup.waiting === 1 ? "entry is" : "entries are"} waiting.
          </p>
          <div className="dialog-actions">
            <Button onClick={() => setAsking(false)}>Cancel</Button>
            <Button
              variant="primary"
              disabled={busy}
              onClick={() => {
                setAsking(false);
                onOpen();
              }}
            >
              Open now
            </Button>
          </div>
        </Dialog>
      ) : null}
    </div>
  );
}

interface SettingProps {
  warmup: Warmup;
  /** Publishers may change it. */
  canChange: boolean;
  busy: boolean;
  onSave: (enabled: boolean, seconds: number) => void;
}

/** The warm-up setting of the flow. It belongs to the flow, not to a version, so it saves on its own. */
export function WarmupSetting({ warmup, canChange, busy, onSave }: SettingProps) {
  const [enabled, setEnabled] = useState(warmup.enabled);
  const [seconds, setSeconds] = useState(warmup.seconds);
  const [seen, setSeen] = useState(warmup);
  if (seen.enabled !== warmup.enabled || seen.seconds !== warmup.seconds) {
    // Saved elsewhere, or by this form: the saved values are the ones shown.
    setSeen(warmup);
    setEnabled(warmup.enabled);
    setSeconds(warmup.seconds);
  }
  const outOfRange = seconds < WARMUP_MIN_SECONDS || seconds > WARMUP_MAX_SECONDS;
  const changed = enabled !== warmup.enabled || (enabled && seconds !== warmup.seconds);
  return (
    <div className="stack">
      <label className="check">
        <input type="checkbox" checked={enabled} disabled={!canChange} onChange={(event) => setEnabled(event.target.checked)} />
        Warm up when the flow starts
      </label>
      {enabled ? (
        <DurationField
          legend="Warm-up length"
          seconds={seconds}
          onChange={setSeconds}
          disabled={!canChange}
          error={outOfRange ? "Between 1 minute and 24 hours." : undefined}
        />
      ) : null}
      {canChange && changed ? (
        <div>
          <Button
            variant="primary"
            disabled={busy || (enabled && outOfRange)}
            onClick={() => onSave(enabled, enabled ? seconds : warmup.seconds)}
          >
            Save warm-up
          </Button>
        </div>
      ) : null}
    </div>
  );
}
