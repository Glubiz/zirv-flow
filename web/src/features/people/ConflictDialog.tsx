import { useState } from "react";

import { ApiError, client, describeError, unwrap, useResource, type components, type ErrorSummary } from "../../api";
import { Button, Dialog, formatDateTime, InlineError, Pill, ResourceView, shortId } from "../../ui";
import { kindLabel, reasonExplanation, resolutionLabel } from "./identityKinds";
import type { Conflict } from "./useConflicts";

type Person = components["schemas"]["PersonIdentitiesBody"];
type Outcome = { outcome: "keep_separate" } | { outcome: "move_identifier"; to_person_id: string };

const LETTERS = ["A", "B"];

function letterOf(index: number): string {
  return LETTERS[index] ?? String(index + 1);
}

interface ConflictDialogProps {
  workspaceId: string;
  conflict: Conflict;
  canManage: boolean;
  onClose: () => void;
  onResolved: (message: string) => void;
}

export function ConflictDialog({ workspaceId, conflict, canManage, onClose, onResolved }: ConflictDialogProps) {
  const detail = useResource(
    () =>
      unwrap(
        client.GET("/api/v1/workspaces/{ws}/identity-conflicts/{id}", {
          params: { path: { ws: workspaceId, id: conflict.id } },
        }),
      ),
    [workspaceId, conflict.id],
  );
  const [moving, setMoving] = useState<{ from: number; to: number } | null>(null);
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);
  const isOpen = !conflict.resolved_at;
  const kind = kindLabel(conflict.kind);

  async function resolve(body: Outcome, message: string) {
    setBusy(true);
    setFailure(null);
    try {
      await unwrap(
        client.POST("/api/v1/workspaces/{ws}/identity-conflicts/{id}/resolve", {
          params: { path: { ws: workspaceId, id: conflict.id } },
          body,
        }),
      );
      onResolved(message);
    } catch (error) {
      setFailure(
        error instanceof ApiError && error.status === 409 && error.detail.startsWith("conflict_changed")
          ? { title: "This conflict changed; reload", detail: "The people were merged since you opened it.", traceId: error.traceId }
          : describeError(error),
      );
      setBusy(false);
    }
  }

  return (
    <>
      <Dialog title="Review conflict" onClose={onClose} wide>
        <div className="stack">
          <p className="muted">{reasonExplanation(conflict.reason, conflict.kind)}</p>
          <ResourceView resource={detail} loadingLabel="Loading the two people">
            {(data) => {
              const owner = data.people.findIndex((person) => person.identities.some((identity) => identity.shared));
              const masked = data.people.some((person) => person.identities.some((identity) => identity.masked));
              const target = moving ? data.people[moving.to] : undefined;
              return (
                <>
                  <div className="compare">
                    {data.people.map((person, index) => (
                      <PersonColumn key={person.person_id} person={person} letter={letterOf(index)} />
                    ))}
                  </div>
                  {masked ? (
                    <p className="muted">
                      Identifier values are hidden. Only workspace admins and the instance owner can see them.
                    </p>
                  ) : null}
                  {!isOpen ? (
                    <p className="muted">
                      <Pill>{resolutionLabel(conflict.resolution)}</Pill>
                      {conflict.resolved_at ? <> on {formatDateTime(conflict.resolved_at)}</> : null}
                    </p>
                  ) : null}
                  {isOpen && canManage && data.people.length === 2 && owner >= 0 ? (
                    <div className="dialog-actions dialog-actions-wrap">
                      <Button
                        onClick={() => void resolve({ outcome: "keep_separate" }, "Kept the two people separate.")}
                        disabled={busy}
                      >
                        Keep separate
                      </Button>
                      {data.people.map((person, index) =>
                        index === owner ? null : (
                          <Button
                            key={person.person_id}
                            variant="primary"
                            onClick={() => {
                              setFailure(null);
                              setMoving({ from: owner, to: index });
                            }}
                            disabled={busy}
                          >
                            Move identifier to person {letterOf(index)}
                          </Button>
                        ),
                      )}
                    </div>
                  ) : null}
                  {isOpen && !canManage ? (
                    <p className="muted">Only workspace admins and the instance owner can resolve conflicts.</p>
                  ) : null}
                  {failure && !moving ? <InlineError {...failure} /> : null}
                  {moving && target ? (
                    <Dialog title={`Move the ${kind.toLowerCase()} to person ${letterOf(moving.to)}?`} onClose={() => setMoving(null)}>
                      <div className="stack">
                        <p className="muted">
                          The {kind.toLowerCase()} will identify person {letterOf(moving.to)} instead of person{" "}
                          {letterOf(moving.from)}. Everything else stays where it is, and the two people are not merged.
                        </p>
                        {failure ? <InlineError {...failure} /> : null}
                        <div className="dialog-actions">
                          <Button onClick={() => setMoving(null)}>Cancel</Button>
                          <Button
                            variant="primary"
                            disabled={busy}
                            onClick={() =>
                              void resolve(
                                { outcome: "move_identifier", to_person_id: target.person_id },
                                `Moved the ${kind.toLowerCase()} to person ${letterOf(moving.to)}.`,
                              )
                            }
                          >
                            {busy ? "Moving" : `Move ${kind.toLowerCase()}`}
                          </Button>
                        </div>
                      </div>
                    </Dialog>
                  ) : null}
                </>
              );
            }}
          </ResourceView>
          <div className="dialog-actions">
            <Button onClick={onClose}>Close</Button>
          </div>
        </div>
      </Dialog>
    </>
  );
}

function PersonColumn({ person, letter }: { person: Person; letter: string }) {
  return (
    <section className="compare-column" aria-label={`Person ${letter}`}>
      <h3 className="compare-title">
        Person {letter} <span className="faint mono">{shortId(person.person_id)}</span>
      </h3>
      <ul className="identity-list">
        {person.identities.map((identity, index) => (
          <li key={`${identity.kind}-${index}`} className={identity.shared ? "identity identity-shared" : "identity"}>
            <span className="identity-kind">{kindLabel(identity.kind)}</span>
            {identity.masked ? <span className="faint">Hidden</span> : <span className="mono identity-value">{identity.value}</span>}
            {identity.shared ? <Pill tone="attention">Shared</Pill> : null}
          </li>
        ))}
      </ul>
    </section>
  );
}
