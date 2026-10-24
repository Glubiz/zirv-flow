import { render, screen, within } from "@testing-library/react";
import { MemoryRouter } from "react-router";
import { describe, expect, it } from "vitest";

import { recordLabel } from "./labels";
import { TraceTree, type TraceTreeRecord } from "./index";

function record(id: string, outcome: string, cause: { kind: string; id: string }, reason?: string): TraceTreeRecord {
  return { id, recorded_at: "2026-10-20T09:00:00Z", action: "flow.enrollment", outcome, cause, reason, subject_ids: {} };
}

const START = { kind: "trait_change", id: "c1" };
const OPERATOR = { kind: "manual", id: "u1" };

describe("trace tree", () => {
  it("keeps the order of the records and opens a branch for each decision with another cause", () => {
    render(
      <TraceTree
        label="Trace"
        records={[
          record("1", "enrolled", START),
          record("2", "failed", START, "boom"),
          record("3", "retried", OPERATOR, "operator"),
          record("4", "failed", START, "boom"),
        ]}
      />,
    );

    const tree = screen.getByRole("list", { name: "Trace" });
    const rows = Array.from(tree.querySelectorAll("li.trace-record")).map((row) => row.firstElementChild?.nextElementSibling?.textContent);
    expect(rows).toEqual(["enrolled", "failed", "retried", "failed"]);
    const retried = Array.from(tree.querySelectorAll("li.trace-record"))[2] as HTMLElement;
    expect(within(retried).getByRole("list")).toHaveTextContent("failed · boom");
  });

  it("roots a truncated trace on no cause, so an operator's decision is never the root", () => {
    render(<TraceTree label="Trace" truncated records={[record("3", "retried", OPERATOR, "operator"), record("4", "failed", START, "boom")]} />);

    const tree = screen.getByRole("list", { name: "Trace" });
    expect(tree.firstElementChild).toHaveTextContent("Earlier decisions are not shown");
    // The first thing in the tree is the notice; the operator's record is a row below it, with its own cause named.
    expect(tree.firstElementChild?.firstElementChild).toHaveTextContent("Earlier decisions are not shown");
    expect(tree.firstElementChild?.firstElementChild).not.toHaveTextContent("An operator");
    const rows = Array.from(tree.querySelectorAll("li.trace-record"));
    expect(rows).toHaveLength(2);
    expect(rows[0]).toHaveTextContent("An operator");
    expect(rows[1]).toHaveTextContent("A change of a field or segment");
  });

  it("draws nothing for no records", () => {
    const { container } = render(<TraceTree label="Trace" records={[]} />);

    expect(container).toBeEmptyDOMElement();
  });

  it("renders a send in order: its checks, the skip, a destination attempt and the delivery feedback, with words and links", () => {
    const subjects = { message: "m1", enrollment: "e1", person: "p1", template: "t1", destination: "d1", flow: "f1" };
    const send = (id: string, action: string, outcome: string, reason: string | null, extra: Partial<TraceTreeRecord> = {}): TraceTreeRecord => ({
      ...record(id, outcome, START, reason ?? undefined),
      action,
      subject_ids: subjects,
      ...extra,
    });
    render(
      <MemoryRouter>
        <TraceTree
          label="Send path"
          workspaceId="ws1"
          records={[
            send("1", "send.gate", "passed", "address"),
            send("2", "send.gate", "blocked", "consent"),
            send("3", "message.send", "skipped", "no_consent"),
            send("4", "job.attempt", "failed", "http_503"),
            send("5", "message.feedback", "recorded", "complained"),
          ]}
        />
      </MemoryRouter>,
    );

    const rows = Array.from(screen.getByRole("list", { name: "Send path" }).querySelectorAll("li.trace-record"));
    expect(rows.map((row) => row.querySelector(".strong")?.textContent.trim())).toEqual([
      "Address check passed",
      "Consent check stopped the send",
      "Message skipped: There is no consent to send this message",
      "Attempt to reach the destination: failed",
      "The person complained (email.complained)",
    ]);
    expect(rows[1]).toHaveTextContent("send.gate");
    expect(rows[1]).toHaveTextContent("blocked");
    const log = within(rows[2] as HTMLElement).getByRole("link", { name: "Messages of this person" });
    expect(log).toHaveAttribute("href", "/w/ws1/people/p1?tab=messages");
    expect(within(rows[2] as HTMLElement).getByRole("link", { name: "Flow" })).toHaveAttribute("href", "/w/ws1/flows/f1");
  });

  it("labels a worker's attempt record that has no subjects, with no link", () => {
    const attempt = (outcome: string, reason: string | null, number: number | null): TraceTreeRecord => ({
      ...record("9", outcome, START, reason ?? undefined),
      action: "job.attempt",
      attempt: number,
    });
    render(
      <MemoryRouter>
        <TraceTree label="Attempts" workspaceId="ws1" records={[attempt("failed", "http_503", 2), attempt("succeeded", null, 3)]} />
      </MemoryRouter>,
    );

    const rows = Array.from(screen.getByRole("list", { name: "Attempts" }).querySelectorAll("li.trace-record"));
    expect(rows.map((row) => row.querySelector(".strong")?.textContent.trim())).toEqual(["Attempt 2 failed: http 503", "Attempt 3 succeeded"]);
    expect(within(rows[0] as HTMLElement).queryByRole("link")).not.toBeInTheDocument();
  });

  it("reads a final failed attempt as failed, with its reason", () => {
    const last: TraceTreeRecord = { ...record("9", "exhausted", START, "http_503"), action: "job.attempt", attempt: 5 };
    render(
      <MemoryRouter>
        <TraceTree label="Attempts" workspaceId="ws1" records={[last]} />
      </MemoryRouter>,
    );

    const row = screen.getByRole("list", { name: "Attempts" }).querySelector("li.trace-record");
    expect(row?.querySelector(".strong")?.textContent.trim()).toBe("Attempt 5 failed: http 503");
  });

  it("words the delivery feedback and the tracking events, and leaves a decision it does not know as its codes", () => {
    const labelled = (action: string, outcome: string, reason: string | null) => recordLabel({ action, outcome, reason, subject_ids: {} });

    expect(labelled("message.feedback", "recorded", "delivered")).toBe("Delivery confirmed (email.delivered)");
    expect(labelled("message.feedback", "recorded", "bounced_hard")).toBe("Bounced for good (email.bounced, hard)");
    expect(labelled("message.feedback", "recorded", "bounced_soft")).toBe("Bounced for now (email.bounced, soft)");
    expect(labelled("email.opened", "recorded", null)).toBe("Opened");
    expect(labelled("email.clicked", "recorded", null)).toBe("A link was clicked");
    expect(labelled("email.unsubscribed", "recorded", null)).toBe("Unsubscribed");
    expect(labelled("message.send", "deferred", "quiet_hours")).toBe("Message held for later");
    expect(labelled("flow.enrollment", "enrolled", null)).toBeNull();
  });
});
