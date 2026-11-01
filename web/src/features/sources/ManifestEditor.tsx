import { useEffect, useMemo, useRef, useState } from "react";

import { client, describeError, unwrap, type ErrorSummary } from "../../api";
import { AssistantDock, AssistantPanel, AssistantToggle, useAssistantDock } from "../assistant";
import { Button, EmptyState, InlineError, LoadingState, SelectField, TextAreaField } from "../../ui";
import { issueText } from "./labels";
import { analyze, lineOfPointer, offsetOfLine, prettyPrint, STARTERS, starterManifest, type AuthType } from "./manifest";

/** How long the text rests before it is sent to be checked. */
const PAUSE_MS = 400;

interface Issue {
  code: string;
  path: string;
}

interface ManifestStream {
  name: string;
  fields: { name: string; type: string }[];
}

interface Verdict {
  /** The text the answer is about. */
  text: string;
  valid: boolean;
  issues: Issue[];
  streams: ManifestStream[];
}

type Sample =
  | { status: "idle" }
  | { status: "loading" }
  | { status: "error"; failure: ErrorSummary }
  | { status: "ready"; records: Record<string, unknown>[]; truncated: boolean };

interface ManifestEditorProps {
  label: string;
  hint?: string;
  /** The manifest as text. */
  value: string;
  onChange: (value: string) => void;
  workspaceId: string;
  /** The saved source, once there is one: only then can a sample be read with its credential. */
  sourceId?: string;
  disabled?: boolean;
}

function cell(value: unknown): string {
  if (value === null || value === undefined) return "";
  return typeof value === "string" ? value : JSON.stringify(value);
}

/**
 * The editor of an `http` source's manifest: the JSON as text, checked by the server as it is
 * typed. Each problem is a sentence with its place in the manifest, and a click puts the caret on
 * its line. With a saved source it also reads a few raw records, to see the API answer before
 * anything is synced.
 */
export function ManifestEditor({
  label,
  hint,
  value,
  onChange,
  workspaceId,
  sourceId,
  disabled = false,
}: ManifestEditorProps) {
  const area = useRef<HTMLTextAreaElement>(null);
  const dock = useAssistantDock();
  /** Once opened, the dock stays mounted while closed, so a draft under review survives closing it. */
  const [dockUsed, setDockUsed] = useState(dock.open);
  if (dock.open && !dockUsed) setDockUsed(true);
  const analysis = useMemo(() => analyze(value), [value]);
  const [verdict, setVerdict] = useState<Verdict | null>(null);
  const [unreachable, setUnreachable] = useState<string | null>(null);
  const [stream, setStream] = useState("");
  const [limit, setLimit] = useState("10");
  const [sample, setSample] = useState<Sample>({ status: "idle" });
  const blank = value.trim() === "";
  // The text the editor opened with, or last loaded from a draft: edits since are unsaved.
  const baseline = useRef(value);

  useEffect(() => {
    if (blank || !analysis.ok) return;
    let current = true;
    const timer = setTimeout(() => {
      void (async () => {
        try {
          const body = await unwrap(
            client.POST("/api/v1/workspaces/{ws}/source-manifests/validate", {
              params: { path: { ws: workspaceId } },
              body: { manifest: analysis.value as never },
            }),
          );
          if (!current) return;
          setUnreachable(null);
          setVerdict({
            text: value,
            valid: body.valid,
            issues: body.issues,
            streams: body.streams.map((entry) => ({ name: entry.name, fields: entry.fields })),
          });
        } catch (error) {
          if (current) setUnreachable(describeError(error).title);
        }
      })();
    }, PAUSE_MS);
    return () => {
      current = false;
      clearTimeout(timer);
    };
  }, [value, blank, analysis, workspaceId]);

  const answered = verdict !== null && verdict.text === value ? verdict : null;
  const issues: Issue[] = blank
    ? []
    : !analysis.ok
      ? [{ code: "syntax_error", path: "" }]
      : (answered?.issues ?? []);
  const streams = answered?.valid ? answered.streams : [];
  const chosen = streams.find((entry) => entry.name === stream) ?? streams[0];
  const checking = !blank && analysis.ok && answered === null && unreachable === null;

  function show(issue: Issue) {
    const line = analysis.ok ? lineOfPointer(analysis.lines, issue.path) : analysis.problem.line;
    const at = offsetOfLine(value, line);
    area.current?.focus();
    area.current?.setSelectionRange(at, at);
  }

  function format() {
    const pretty = prettyPrint(value);
    if (pretty !== null) onChange(pretty);
  }

  async function read() {
    if (!sourceId || !chosen || !analysis.ok) return;
    setSample({ status: "loading" });
    try {
      const body = await unwrap(
        client.POST("/api/v1/workspaces/{ws}/sources/{id}/manifest/sample", {
          params: { path: { ws: workspaceId, id: sourceId } },
          body: {
            manifest: analysis.value as never,
            stream: chosen.name,
            limit: Number(limit),
          },
        }),
      );
      setSample({ status: "ready", records: body.records, truncated: body.truncated });
    } catch (error) {
      setSample({ status: "error", failure: describeError(error) });
    }
  }

  return (
    <div className="dock-layout">
    <div className="stack">
      {blank ? (
        <SelectField
          label="Start from"
          hint="Fills in a small manifest to edit."
          value=""
          disabled={disabled}
          onChange={(event) => {
            if (event.target.value) onChange(starterManifest(event.target.value as AuthType));
          }}
        >
          <option value="">Choose a starter</option>
          {STARTERS.map((starter) => (
            <option key={starter.id} value={starter.id}>
              {starter.label}
            </option>
          ))}
        </SelectField>
      ) : null}
      <TextAreaField
        ref={area}
        label={label}
        hint={hint}
        value={value}
        rows={18}
        mono
        spellCheck={false}
        disabled={disabled}
        onChange={(event) => onChange(event.target.value)}
      />
      <div className="row">
        <Button onClick={format} disabled={disabled || !analysis.ok || blank}>
          Format
        </Button>
        {disabled ? null : <AssistantToggle open={dock.open} onToggle={dock.toggle} />}
        <span className="faint" role="status">
          {blank
            ? "Nothing yet"
            : checking
              ? "Checking"
              : issues.length === 0 && answered?.valid
                ? "The manifest is valid"
                : unreachable
                  ? "Could not check the manifest"
                  : ""}
        </span>
      </div>
      {unreachable ? <InlineError title="The manifest could not be checked" detail={unreachable} /> : null}
      {issues.length > 0 ? (
        <ul className="stack list-plain" aria-label="Problems in the manifest">
          {issues.map((issue, index) => (
            <li key={`${issue.code}${issue.path}${index}`}>
              <button type="button" className="link-button" onClick={() => show(issue)}>
                {issueText(issue.code)}
              </button>{" "}
              {issue.path !== "" ? <span className="mono faint">{issue.path}</span> : null}
              {issue.code === "syntax_error" && !analysis.ok ? (
                <span className="faint">{` Line ${analysis.problem.line}, column ${analysis.problem.column}.`}</span>
              ) : null}
            </li>
          ))}
        </ul>
      ) : null}
      {sourceId ? (
        <div className="stack">
          <div className="row">
            <SelectField
              label="Stream to read"
              value={chosen?.name ?? ""}
              disabled={disabled || streams.length === 0}
              onChange={(event) => {
                setStream(event.target.value);
                setSample({ status: "idle" });
              }}
            >
              {streams.length === 0 ? <option value="">No valid stream yet</option> : null}
              {streams.map((entry) => (
                <option key={entry.name} value={entry.name}>
                  {entry.name}
                </option>
              ))}
            </SelectField>
            <SelectField
              label="Records"
              value={limit}
              disabled={disabled}
              onChange={(event) => setLimit(event.target.value)}
            >
              {["5", "10", "20"].map((count) => (
                <option key={count} value={count}>
                  {count}
                </option>
              ))}
            </SelectField>
            <Button
              onClick={() => void read()}
              disabled={disabled || !chosen || sample.status === "loading"}
            >
              Test with sample
            </Button>
          </div>
          {sample.status === "idle" ? (
            <p className="muted">Reads a few records with the saved credential and shows them as the API returns them.</p>
          ) : sample.status === "loading" ? (
            <LoadingState label="Reading a sample" />
          ) : sample.status === "error" ? (
            <InlineError {...sample.failure} />
          ) : sample.records.length === 0 ? (
            <EmptyState title="The API returned no records" />
          ) : (
            <div className="stack">
              <div className="table-scroll">
                <table className="table" aria-label="Sample records">
                  <thead>
                    <tr>
                      {(chosen?.fields ?? []).map((field) => (
                        <th key={field.name} scope="col">
                          {field.name}
                        </th>
                      ))}
                    </tr>
                  </thead>
                  <tbody>
                    {sample.records.map((record, index) => (
                      <tr key={index}>
                        {(chosen?.fields ?? []).map((field) => (
                          <td key={field.name}>{cell(record[field.name])}</td>
                        ))}
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
              <p className="muted">
                Raw values, as the API returned them, before any mapping.
                {sample.truncated ? " The stream has more records than shown." : ""}
              </p>
            </div>
          )}
        </div>
      ) : (
        <p className="muted">Save the source to read a sample with its credential.</p>
      )}
    </div>
    {dockUsed && !disabled ? (
      <div className="dock-slot">
        <AssistantDock
          thing="manifest"
          hidden={!dock.open}
          onClose={() => dock.setOpen(false)}
          draft={
                <AssistantPanel
                  workspaceId={workspaceId}
                  kind="manifest"
                  sourceId={sourceId}
                  onAccept={(content) => {
                    const next = JSON.stringify(content, null, 2);
                    baseline.current = next;
                    onChange(next);
                  }}
                  confirmAccept={() => value === baseline.current || window.confirm("Replace the manifest you have edited with the assistant's draft?")}
                />
          }
        />
      </div>
    ) : null}
    </div>
  );
}
