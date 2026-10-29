import { render, screen } from "@testing-library/react";
import { MemoryRouter } from "react-router";
import { describe, expect, it } from "vitest";

import fixture from "./fixtures/send-path-trace.json";
import failingFixture from "./fixtures/failing-send-path-trace.json";
import { TraceTree, type TraceTreeRecord } from "./index";
import { actionLabel, recordLabel } from "./labels";

/**
 * The trace of the end-to-end send test (crates/zirv-flow/src/e2e), exactly as the API list returns it
 * with its ids and times normalised: a track call, the segment entry, the enrollment, the steps, the
 * messages, the unsubscribe, the delivery feedback and the later send skipped for no consent. The
 * integration test writes the file with ZIRV_FLOW_BLESS=1 and otherwise compares against it.
 */
const records = fixture.records as TraceTreeRecord[];

describe("the trace of the send path", () => {
  it("renders the nodes of the API list, in the order the API lists them", () => {
    render(
      <MemoryRouter>
        <TraceTree label="Send path" workspaceId="ws1" records={records} truncated={fixture.truncated} />
      </MemoryRouter>,
    );

    const tree = screen.getByRole("list", { name: "Send path" });
    const rows = Array.from(tree.querySelectorAll("li.trace-record"));
    expect(rows).toHaveLength(records.length);
    const shown = rows.map((row) => ({
      action: row.querySelectorAll(":scope > .mono")[0]?.textContent,
      outcome: row.querySelectorAll(":scope > .mono")[1]?.textContent,
      at: row.querySelector(":scope > time")?.getAttribute("datetime"),
    }));
    expect(shown).toEqual(records.map((record) => ({ action: record.action, outcome: record.outcome, at: record.recorded_at })));
  });

  it("renders one tree: a single root, the cause that started the trace, holding every record", () => {
    render(
      <MemoryRouter>
        <TraceTree label="Send path" workspaceId="ws1" records={records} truncated={fixture.truncated} />
      </MemoryRouter>,
    );

    const tree = screen.getByRole("list", { name: "Send path" });
    const roots = tree.querySelectorAll(":scope > li");
    expect(roots).toHaveLength(1);
    expect(roots[0]?.querySelector(":scope > .mono")?.textContent).toBe(records[0]?.cause.kind);
    expect(roots[0]?.querySelectorAll("li.trace-record")).toHaveLength(records.length);
  });

  it("holds every stage of the send path, ending in the skipped send with its gate reason", () => {
    const stages = ["event.accepted", "segment.entered", "flow.enroll", "flow.step", "message.send", "consent.change", "message.feedback"];
    for (const stage of stages) expect(records.some((record) => record.action === stage)).toBe(true);
    const last = records[records.length - 1];
    expect(last).toMatchObject({ action: "message.send", outcome: "skipped", reason: "no_consent" });
    expect(JSON.stringify(records)).not.toContain("@");
  });
});

describe("the trace of the failing send path", () => {
  it("renders the retries, failed send, bounce and suppression in one labelled tree", () => {
    const records = failingFixture.records as TraceTreeRecord[];
    render(
      <MemoryRouter>
        <TraceTree label="Failing send path" workspaceId="ws1" records={records} truncated={failingFixture.truncated} />
      </MemoryRouter>,
    );
    const tree = screen.getByRole("list", { name: "Failing send path" });
    const roots = tree.querySelectorAll(":scope > li");
    expect(roots).toHaveLength(1);
    const rows = Array.from(roots[0]!.querySelectorAll("li.trace-record"));
    expect(rows).toHaveLength(records.length);
    rows.forEach((row, index) => {
      const record = records[index]!;
      expect(actionLabel(record.action)).toBeTruthy();
      expect(recordLabel(record)).toBeTruthy();
      expect(row.querySelector(":scope > .strong")?.textContent.trim()).toBe(recordLabel(record));
      expect(row.querySelectorAll(":scope > .mono")[0]?.textContent).toBe(record.action);
      expect(row.querySelectorAll(":scope > .mono")[1]?.textContent).toBe(record.outcome);
      expect(row.querySelector(":scope > time")?.getAttribute("datetime")).toBe(record.recorded_at);
    });
    expect(records.filter((record) => record.action === "job.attempt").map((record) => [record.attempt, record.outcome]))
      .toEqual([[1, "failed"], [2, "failed"]]);
    expect(records.slice(-3).map((record) => [record.action, record.outcome, record.reason])).toEqual([
      ["message.send", "failed", "http_5xx"],
      ["message.feedback", "recorded", "bounced_hard"],
      ["suppression.feedback", "added", "organization_hard_bounce"],
    ]);
    expect(JSON.stringify(records)).not.toContain("@");
  });
});
