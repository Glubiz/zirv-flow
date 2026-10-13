import { useState } from "react";
import { Link, useParams } from "react-router";

import { client, describeError, unwrap, useResource, type ErrorSummary } from "../../api";
import { Button, EmptyState, InlineError, PageHeader, Panel, ResourceView, Stepper, TextField } from "../../ui";
import { useWorkspaceAccess } from "../members";
import { FieldInputs } from "./FieldInputs";
import {
  configFrom,
  credentialFrom,
  filled,
  initialValues,
  shapesValid,
  withoutSecrets,
  type Source,
  type SourceKind,
  type Stream,
  type Values,
} from "./labels";
import { STEPS, type Step } from "./steps";
import { StreamsPanel } from "./StreamsPanel";

const MAX_NAME_CHARS = 128;

/** The source types that read a database, which the setup guide covers. */
const DATABASE_KINDS = ["postgres", "mysql"];
const DATABASE_GUIDE = "https://github.com/Glubiz/zirv-flow#database-sources";

export function NewSourcePage() {
  const { ws = "" } = useParams();
  return <NewSource key={ws} workspaceId={ws} />;
}

interface TestResult {
  ok: boolean;
  error?: string | null;
}

function NewSource({ workspaceId }: { workspaceId: string }) {
  const access = useWorkspaceAccess(workspaceId);
  const kinds = useResource(() => unwrap(client.GET("/api/v1/source-kinds")), []);
  const [step, setStep] = useState<Step>("type");
  const [kind, setKind] = useState<SourceKind | null>(null);
  const [name, setName] = useState("");
  const [values, setValues] = useState<Values>({});
  const [source, setSource] = useState<Source | null>(null);
  const [result, setResult] = useState<TestResult | null>(null);
  const [streams, setStreams] = useState<Stream[] | null>(null);
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);

  function choose(chosen: SourceKind) {
    setKind(chosen);
    setValues(initialValues([...chosen.config_fields, ...chosen.credential_fields]));
    setSource(null);
    setResult(null);
    setStreams(null);
    setFailure(null);
    setStep("settings");
  }

  async function saveAndTest() {
    if (!kind) return;
    setBusy(true);
    setFailure(null);
    try {
      const config = configFrom(kind.config_fields, values);
      const credential = credentialFrom(kind.credential_fields, values);
      // The first attempt creates the source. A retry after a failed test changes it, and sends
      // the credential only when it was typed again: the server never returns it to prefill.
      const saved = source
        ? await unwrap(
            client.PATCH("/api/v1/workspaces/{ws}/sources/{id}", {
              params: { path: { ws: workspaceId, id: source.id } },
              body: {
                name: name.trim(),
                config,
                ...(Object.keys(credential).length > 0 ? { credential } : {}),
              },
            }),
          )
        : await unwrap(
            client.POST("/api/v1/workspaces/{ws}/sources", {
              params: { path: { ws: workspaceId } },
              body: { kind: kind.kind, name: name.trim(), config, credential },
            }),
          );
      setSource(saved);
      setValues((current) => withoutSecrets(kind.credential_fields, current));
      setResult(
        await unwrap(
          client.POST("/api/v1/workspaces/{ws}/sources/{id}/test", {
            params: { path: { ws: workspaceId, id: saved.id } },
          }),
        ),
      );
      setStep("test");
    } catch (error) {
      setFailure(describeError(error));
    } finally {
      setBusy(false);
    }
  }

  async function discover() {
    if (!source) return;
    setBusy(true);
    setFailure(null);
    try {
      setStreams(
        await unwrap(
          client.POST("/api/v1/workspaces/{ws}/sources/{id}/discover", {
            params: { path: { ws: workspaceId, id: source.id } },
          }),
        ),
      );
    } catch (error) {
      setFailure(describeError(error));
    } finally {
      setBusy(false);
    }
  }

  const nameOk = name.trim() !== "" && name.trim().length <= MAX_NAME_CHARS;
  // A retry may leave the credential blank to keep the one already stored.
  const requiredOk = kind
    ? filled(kind.config_fields, values) &&
      shapesValid(kind.config_fields, values) &&
      (source !== null || filled(kind.credential_fields, values))
    : false;

  return (
    <div className="page">
      <PageHeader
        title="New source"
        lede="Choose a type, fill in its settings, and test the connection. Then discover its streams and pick the ones to read. The credential is stored encrypted and is never shown again."
      />
      <Stepper steps={STEPS} current={step} label="Source setup steps" />
      <Panel>
        <ResourceView resource={access} loadingLabel="Checking your access">
          {({ canManage }) =>
            !canManage ? (
              <p className="muted">
                Only workspace admins can add sources. <Link to={`/w/${workspaceId}/sources`}>Back to sources</Link>
              </p>
            ) : step === "type" ? (
              <ResourceView
                resource={kinds}
                loadingLabel="Loading source types"
                isEmpty={(data) => data.length === 0}
                empty={
                  <div className="stack">
                    <EmptyState title="No source types are available on this deployment." />
                    <Link to={`/w/${workspaceId}/sources`}>Back to sources</Link>
                  </div>
                }
              >
                {(data) => (
                  <div className="stack">
                    <ul className="stack list-plain" aria-label="Source types">
                      {data.map((entry) => (
                        <li key={entry.kind}>
                          <Button onClick={() => choose(entry)} aria-label={`Choose ${entry.label}`}>
                            {entry.label}
                          </Button>{" "}
                          <span className="muted">{entry.description}</span>
                        </li>
                      ))}
                    </ul>
                    <Link to={`/w/${workspaceId}/sources`}>Cancel</Link>
                  </div>
                )}
              </ResourceView>
            ) : step === "settings" && kind ? (
              <form
                className="stack"
                onSubmit={(event) => {
                  event.preventDefault();
                  void saveAndTest();
                }}
              >
                <p className="strong">{kind.label}</p>
                <TextField
                  label="Name"
                  value={name}
                  onChange={(event) => setName(event.target.value)}
                  maxLength={MAX_NAME_CHARS}
                  hint="Shown in the list of sources. Two sources cannot share a name."
                  autoComplete="off"
                />
                <FieldInputs
                  fields={kind.config_fields}
                  values={values}
                  onChange={(key, value) => setValues((current) => ({ ...current, [key]: value }))}
                  disabled={busy}
                />
                {DATABASE_KINDS.includes(kind.kind) ? (
                  <p className="muted">
                    Connect with a role that can only read what should be synced.{" "}
                    <a href={DATABASE_GUIDE} target="_blank" rel="noreferrer">
                      How to create one
                    </a>
                    .
                  </p>
                ) : null}
                {kind.credential_fields.length > 0 ? (
                  <fieldset className="stack">
                    <legend className="strong">Credential</legend>
                    <p className="muted">
                      {source
                        ? "Leave these blank to keep the credential you saved. They are never shown again."
                        : "Stored encrypted. It cannot be read back."}
                    </p>
                    <FieldInputs
                      fields={kind.credential_fields}
                      values={values}
                      onChange={(key, value) => setValues((current) => ({ ...current, [key]: value }))}
                      disabled={busy}
                    />
                  </fieldset>
                ) : null}
                {failure ? <InlineError {...failure} /> : null}
                <div className="row">
                  <Button variant="primary" type="submit" disabled={busy || !nameOk || !requiredOk}>
                    {busy ? "Saving and testing" : "Save and test"}
                  </Button>
                  {source ? null : <Button onClick={() => setStep("type")}>Back</Button>}
                  <Link to={`/w/${workspaceId}/sources`}>{source ? "Finish later" : "Cancel"}</Link>
                </div>
              </form>
            ) : step === "test" && source && result ? (
              <div className="stack">
                {result.ok ? (
                  <p className="notice" role="status">
                    The connection to {source.name} works.
                  </p>
                ) : (
                  <InlineError title="The connection test failed" detail={result.error ?? undefined} />
                )}
                <div className="row">
                  {result.ok ? (
                    <Button variant="primary" onClick={() => setStep("streams")}>
                      Continue to streams
                    </Button>
                  ) : null}
                  <Button onClick={() => setStep("settings")}>{result.ok ? "Change settings" : "Back to settings"}</Button>
                </div>
              </div>
            ) : step === "streams" && source ? (
              <div className="stack">
                {streams === null ? (
                  <>
                    <p className="muted">Ask {source.name} which streams it offers. None is read until you turn it on.</p>
                    {failure ? <InlineError {...failure} /> : null}
                    <div className="row">
                      <Button variant="primary" onClick={() => void discover()} disabled={busy}>
                        {busy ? "Discovering" : "Discover streams"}
                      </Button>
                      <Link to={`/w/${workspaceId}/sources/${source.id}`}>Skip for now</Link>
                    </div>
                  </>
                ) : (
                  <>
                    <StreamsPanel
                      workspaceId={workspaceId}
                      sourceId={source.id}
                      streams={streams}
                      canManage
                      onChanged={(saved) =>
                        setStreams((current) => (current ?? []).map((item) => (item.id === saved.id ? saved : item)))
                      }
                    />
                    <div className="row">
                      <Link className="btn btn-primary" to={`/w/${workspaceId}/sources/${source.id}`}>
                        Finish
                      </Link>
                    </div>
                  </>
                )}
              </div>
            ) : null
          }
        </ResourceView>
      </Panel>
    </div>
  );
}
