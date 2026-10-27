import { useState, type SyntheticEvent } from "react";
import { useParams } from "react-router";

import { ApiError, client, describeError, unwrap, unwrapEmpty, useResource, type ErrorSummary } from "../../api";
import { Button, Dialog, EmptyState, formatDate, InlineError, PageHeader, Panel, Pill, ResourceView, TextField } from "../../ui";
import { useSignedInUser } from "../session";
import { useWorkspaces } from "../workspaces";
import { accessFor, useMembers, type Member, type MemberRole } from "./useWorkspaceAccess";

export function MembersPage() {
  const { ws = "" } = useParams();
  return <WorkspaceMembers key={ws} ws={ws} />;
}

const ROLE_OPTIONS = [
  { value: "admin", label: "Admin", help: "Access, credentials, settings" },
  { value: "publisher", label: "Publisher", help: "Publishes flows and templates" },
  { value: "editor", label: "Editor", help: "Edits drafts, runs tests" },
  { value: "viewer", label: "Viewer", help: "Reads only" },
];

const LAST_ADMIN = "A workspace needs an admin";

function roleLabel(role: string): string {
  return ROLE_OPTIONS.find((option) => option.value === role)?.label ?? role;
}

function roleFrom(value: string): MemberRole {
  return value === "admin" || value === "publisher" || value === "viewer" ? value : "editor";
}

function RolesDisclosure() {
  return (
    <details className="disclosure">
      <summary>Roles</summary>
      <dl className="facts">
        {ROLE_OPTIONS.map((option) => (
          <div className="fact" key={option.value}>
            <dt>{option.label}</dt>
            <dd>{option.help}</dd>
          </div>
        ))}
      </dl>
    </details>
  );
}

function WorkspaceMembers({ ws }: { ws: string }) {
  const user = useSignedInUser();
  const workspaces = useWorkspaces();
  const members = useMembers(ws);
  const [adding, setAdding] = useState(false);
  const [removing, setRemoving] = useState<Member | null>(null);
  const [busyUser, setBusyUser] = useState<string | null>(null);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);
  const [refused, setRefused] = useState<string | null>(null);

  const name = workspaces.status === "ready" ? workspaces.data.find((item) => item.id === ws)?.name : undefined;
  const access = members.status === "ready" ? accessFor(members.data, user.id, user.is_owner) : null;
  const canAdmin = access?.canAdmin ?? false;
  const adminCount = members.status === "ready" ? members.data.filter((member) => member.role === "admin").length : 0;
  const lastAdmin = (member: Member) => member.role === "admin" && adminCount <= 1;
  const reasonShown = (member: Member) => lastAdmin(member) || refused === member.user_id;
  const reasonId = (member: Member) => `last-admin-${member.user_id}`;

  async function save(member: Member, body: { role: string; view_pii?: boolean }) {
    setBusyUser(member.user_id);
    setFailure(null);
    setRefused(null);
    try {
      await unwrap(
        client.PUT("/api/v1/workspaces/{ws}/members/{user_id}", {
          params: { path: { ws, user_id: member.user_id } },
          body,
        }),
      );
      members.reload();
    } catch (error) {
      if (error instanceof ApiError && error.code === "last_admin") {
        setRefused(member.user_id);
      } else {
        setFailure(describeError(error));
      }
    } finally {
      setBusyUser(null);
    }
  }

  return (
    <div className="page">
      <PageHeader
        title="Members"
        lede={name ? `People who can work in ${name}.` : "People who can work in this workspace."}
        actions={
          canAdmin ? (
            <Button variant="primary" onClick={() => setAdding(true)}>
              Add member
            </Button>
          ) : null
        }
      />
      <RolesDisclosure />
      {failure ? <InlineError {...failure} /> : null}
      <Panel flush>
        <ResourceView
          resource={members}
          loadingLabel="Loading members"
          isEmpty={(data) => data.length === 0}
          empty={
            <EmptyState title="No members yet">
              {canAdmin ? "Add the first person to this workspace." : "Nobody has been added to this workspace."}
            </EmptyState>
          }
        >
          {(data) => (
            <div className="table-scroll">
              <table className="table">
                <caption>Members of this workspace</caption>
                <thead>
                  <tr>
                    <th scope="col">Email</th>
                    <th scope="col">Role</th>
                    {canAdmin ? <th scope="col">Personal data</th> : null}
                    <th scope="col">Added</th>
                    {canAdmin ? (
                      <th scope="col">
                        <span className="visually-hidden">Actions</span>
                      </th>
                    ) : null}
                  </tr>
                </thead>
                <tbody>
                  {data.map((member) => (
                    <tr key={member.user_id}>
                      <td className="strong">
                        {member.email}
                        {member.user_id === user.id ? <span className="faint"> (you)</span> : null}
                      </td>
                      <td>
                        {canAdmin ? (
                          <div className="stack-tight">
                            <select
                              className="select"
                              aria-label={`Role for ${member.email}`}
                              aria-describedby={reasonShown(member) ? reasonId(member) : undefined}
                              value={member.role}
                              disabled={busyUser === member.user_id || lastAdmin(member)}
                              onChange={(event) => void save(member, { role: event.target.value })}
                            >
                              {ROLE_OPTIONS.map((option) => (
                                <option key={option.value} value={option.value}>
                                  {option.label}
                                </option>
                              ))}
                            </select>
                            {reasonShown(member) ? (
                              <span id={reasonId(member)} className="faint">
                                {LAST_ADMIN}
                              </span>
                            ) : null}
                          </div>
                        ) : (
                          <Pill>{roleLabel(member.role)}</Pill>
                        )}
                      </td>
                      {canAdmin ? (
                        <td>
                          <input
                            type="checkbox"
                            role="switch"
                            aria-label={`Personal data for ${member.email}`}
                            checked={member.role === "admin" || member.view_pii}
                            disabled={member.role === "admin" || busyUser === member.user_id}
                            onChange={(event) => void save(member, { role: member.role, view_pii: event.target.checked })}
                          />
                        </td>
                      ) : null}
                      <td>{formatDate(member.added_at)}</td>
                      {canAdmin ? (
                        <td className="num">
                          <Button
                            onClick={() => setRemoving(member)}
                            aria-label={`Remove ${member.email}`}
                            aria-describedby={lastAdmin(member) ? reasonId(member) : undefined}
                            disabled={lastAdmin(member)}
                          >
                            Remove
                          </Button>
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
      {adding && members.status === "ready" ? (
        <AddMemberDialog
          workspaceId={ws}
          existing={members.data}
          isOwner={user.is_owner}
          onClose={() => setAdding(false)}
          onAdded={() => {
            setAdding(false);
            members.reload();
          }}
        />
      ) : null}
      {removing ? (
        <RemoveMemberDialog
          workspaceId={ws}
          member={removing}
          onClose={() => setRemoving(null)}
          onRemoved={() => {
            setRemoving(null);
            members.reload();
          }}
        />
      ) : null}
    </div>
  );
}

interface AddMemberDialogProps {
  workspaceId: string;
  existing: Member[];
  isOwner: boolean;
  onClose: () => void;
  onAdded: () => void;
}

function AddMemberDialog({ workspaceId, existing, isOwner, onClose, onAdded }: AddMemberDialogProps) {
  const users = useResource(
    async () => (isOwner ? unwrap(client.GET("/api/v1/users")) : []),
    [isOwner],
  );
  const [userId, setUserId] = useState("");
  const [role, setRole] = useState<MemberRole>("editor");
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);

  const taken = new Set(existing.map((member) => member.user_id));
  const candidates = users.status === "ready" ? users.data.filter((candidate) => !taken.has(candidate.id)) : [];

  async function submit(event: SyntheticEvent) {
    event.preventDefault();
    setBusy(true);
    setFailure(null);
    try {
      await unwrap(
        client.PUT("/api/v1/workspaces/{ws}/members/{user_id}", {
          params: { path: { ws: workspaceId, user_id: userId.trim() } },
          body: { role },
        }),
      );
      onAdded();
    } catch (error) {
      setFailure(describeError(error));
      setBusy(false);
    }
  }

  return (
    <Dialog title="Add a member" onClose={onClose}>
      <form className="stack" onSubmit={(event) => void submit(event)} noValidate>
        {isOwner ? (
          <ResourceView
            resource={users}
            loadingLabel="Loading people"
            isEmpty={() => candidates.length === 0}
            empty={<EmptyState title="Everyone is already here">Every user on this instance is a member of this workspace.</EmptyState>}
          >
            {() => (
              <div className="field">
                <label className="field-label" htmlFor="add-member-user">
                  Person
                </label>
                <select
                  id="add-member-user"
                  className="select"
                  value={userId}
                  onChange={(event) => setUserId(event.target.value)}
                  required
                >
                  <option value="">Choose a person</option>
                  {candidates.map((candidate) => (
                    <option key={candidate.id} value={candidate.id}>
                      {candidate.email}
                    </option>
                  ))}
                </select>
              </div>
            )}
          </ResourceView>
        ) : (
          <TextField
            label="User ID"
            mono
            hint="Ask the instance owner for the ID of the person to add."
            value={userId}
            onChange={(event) => setUserId(event.target.value)}
            required
          />
        )}
        <div className="field">
          <label className="field-label" htmlFor="add-member-role">
            Role
          </label>
          <select id="add-member-role" className="select" value={role} onChange={(event) => setRole(roleFrom(event.target.value))}>
            {ROLE_OPTIONS.map((option) => (
              <option key={option.value} value={option.value}>
                {option.label}
              </option>
            ))}
          </select>
        </div>
        {failure ? <InlineError {...failure} /> : null}
        <div className="dialog-actions">
          <Button onClick={onClose}>Cancel</Button>
          <Button type="submit" variant="primary" disabled={busy || userId.trim() === ""}>
            {busy ? "Adding" : "Add member"}
          </Button>
        </div>
      </form>
    </Dialog>
  );
}

interface RemoveMemberDialogProps {
  workspaceId: string;
  member: Member;
  onClose: () => void;
  onRemoved: () => void;
}

function RemoveMemberDialog({ workspaceId, member, onClose, onRemoved }: RemoveMemberDialogProps) {
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);

  async function remove() {
    setBusy(true);
    setFailure(null);
    try {
      await unwrapEmpty(
        client.DELETE("/api/v1/workspaces/{ws}/members/{user_id}", {
          params: { path: { ws: workspaceId, user_id: member.user_id } },
        }),
      );
      onRemoved();
    } catch (error) {
      setFailure(describeError(error));
      setBusy(false);
    }
  }

  return (
    <Dialog title={`Remove ${member.email}?`} onClose={onClose}>
      <p className="muted">They lose access to this workspace straight away. You can add them again later.</p>
      {failure ? <InlineError {...failure} /> : null}
      <div className="dialog-actions">
        <Button onClick={onClose}>Keep member</Button>
        <Button variant="primary" onClick={() => void remove()} disabled={busy}>
          {busy ? "Removing" : `Remove ${member.email}`}
        </Button>
      </div>
    </Dialog>
  );
}
