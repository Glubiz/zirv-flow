import { useState } from "react";
import { useParams } from "react-router";

import { client, describeError, unwrap, useResource, type ErrorSummary } from "../../api";
import { Button, Dialog, EmptyState, InlineError, PageHeader, Panel, Pill, ResourceView } from "../../ui";
import { useWorkspaceAccess } from "../members";
import { policyHelp, policyLabel } from "./labels";
import { TopicDialog, type Topic } from "./TopicDialog";

export function TopicsPage() {
  const { ws = "" } = useParams();
  const access = useWorkspaceAccess(ws);
  const canManage = access.status === "ready" && access.data.canManage;

  return (
    <div className="page">
      <PageHeader
        title="Topics"
        lede="What people can subscribe to. A topic's setting decides whether silence means yes or no: opt-in topics need a subscription, opt-out topics reach everyone who has not unsubscribed."
      />
      <TopicList key={ws} workspaceId={ws} canManage={canManage} />
    </div>
  );
}

function TopicList({ workspaceId, canManage }: { workspaceId: string; canManage: boolean }) {
  const [showArchived, setShowArchived] = useState(false);
  const topics = useResource(
    () =>
      unwrap(
        client.GET("/api/v1/workspaces/{ws}/topics", {
          params: { path: { ws: workspaceId }, query: { archived: showArchived } },
        }),
      ),
    [workspaceId, showArchived],
  );
  const [adding, setAdding] = useState(false);
  const [editing, setEditing] = useState<Topic | null>(null);
  const [archiving, setArchiving] = useState<Topic | null>(null);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);
  const [busyId, setBusyId] = useState<string | null>(null);

  async function setArchived(topic: Topic, archived: boolean) {
    setBusyId(topic.id);
    setFailure(null);
    try {
      await unwrap(
        client.PATCH("/api/v1/workspaces/{ws}/topics/{id}", {
          params: { path: { ws: workspaceId, id: topic.id } },
          body: { archived },
        }),
      );
      setArchiving(null);
      topics.reload();
    } catch (error) {
      setFailure(describeError(error));
      setArchiving(null);
    } finally {
      setBusyId(null);
    }
  }

  return (
    <>
      <div className="page-actions">
        {canManage ? (
          <Button variant="primary" onClick={() => setAdding(true)}>
            Add topic
          </Button>
        ) : null}
        <label className="check">
          <input type="checkbox" checked={showArchived} onChange={(event) => setShowArchived(event.target.checked)} />
          <span>Show archived topics</span>
        </label>
      </div>
      {failure ? <InlineError {...failure} /> : null}
      <Panel flush>
        <ResourceView
          resource={topics}
          loadingLabel="Loading topics"
          isEmpty={(data) => data.length === 0}
          empty={
            <EmptyState title={showArchived ? "No topics yet" : "No active topics"}>
              {canManage
                ? "Add a topic so people can choose what they hear from you."
                : "Workspace admins add the topics people can subscribe to."}
            </EmptyState>
          }
        >
          {(data) => (
            <div className="table-scroll">
              <table className="table">
                <caption>Topics of this workspace</caption>
                <thead>
                  <tr>
                    <th scope="col">Topic</th>
                    <th scope="col">Without an answer</th>
                    <th scope="col">Status</th>
                    {canManage ? (
                      <th scope="col">
                        <span className="visually-hidden">Actions</span>
                      </th>
                    ) : null}
                  </tr>
                </thead>
                <tbody>
                  {data.map((topic) => (
                    <tr key={topic.id}>
                      <td className="strong">
                        {topic.name}
                        <div className="faint mono">{topic.key}</div>
                        {topic.description ? <div className="faint">{topic.description}</div> : null}
                      </td>
                      <td>
                        {policyLabel(topic.policy)}
                        <div className="faint">{policyHelp(topic.policy)}</div>
                      </td>
                      <td>{topic.archived ? <Pill tone="error">Archived</Pill> : <Pill>Active</Pill>}</td>
                      {canManage ? (
                        <td className="num">
                          <Button onClick={() => setEditing(topic)} aria-label={`Edit ${topic.name}`}>
                            Edit
                          </Button>{" "}
                          {topic.archived ? (
                            <Button
                              onClick={() => void setArchived(topic, false)}
                              disabled={busyId === topic.id}
                              aria-label={`Restore ${topic.name}`}
                            >
                              Restore
                            </Button>
                          ) : (
                            <Button onClick={() => setArchiving(topic)} aria-label={`Archive ${topic.name}`}>
                              Archive
                            </Button>
                          )}
                        </td>
                      ) : null}
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )}
        </ResourceView>
      </Panel>
      {!canManage ? <p className="muted">Only workspace admins and the instance owner can add or change topics.</p> : null}
      {adding ? (
        <TopicDialog
          workspaceId={workspaceId}
          onClose={() => setAdding(false)}
          onSaved={() => {
            setAdding(false);
            topics.reload();
          }}
        />
      ) : null}
      {editing ? (
        <TopicDialog
          workspaceId={workspaceId}
          topic={editing}
          onClose={() => setEditing(null)}
          onSaved={() => {
            setEditing(null);
            topics.reload();
          }}
        />
      ) : null}
      {archiving ? (
        <Dialog title={`Archive ${archiving.name}?`} onClose={() => setArchiving(null)}>
          <p className="muted">
            People keep what they chose for <span className="strong">{archiving.name}</span>, and sources can still read it. You can
            restore the topic later.
          </p>
          <div className="dialog-actions">
            <Button onClick={() => setArchiving(null)}>Keep topic</Button>
            <Button variant="primary" onClick={() => void setArchived(archiving, true)} disabled={busyId === archiving.id}>
              {busyId === archiving.id ? "Archiving" : `Archive ${archiving.name}`}
            </Button>
          </div>
        </Dialog>
      ) : null}
    </>
  );
}
