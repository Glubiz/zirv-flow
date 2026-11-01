import { useState } from "react";
import { Link, useNavigate, useParams } from "react-router";

import { ApiError, client, describeError, unwrap, unwrapEmpty, useResource, type ErrorSummary } from "../../api";
import {
  Button,
  Dialog,
  EmptyState,
  ErrorState,
  formatDateTime,
  HiddenValue,
  InlineError,
  LoadingState,
  PageHeader,
  Panel,
  Pill,
} from "../../ui";
import { ContactForm } from "./ContactForm";
import { identifierLabel, showValue, useContactAccess, useContactSchema, type Contact, type SchemaField } from "./schema";

export function ContactPage() {
  const { ws = "", key = "" } = useParams();
  return <ContactView key={`${ws}/${key}`} workspaceId={ws} contactKey={key} />;
}

function ContactView({ workspaceId, contactKey }: { workspaceId: string; contactKey: string }) {
  const { canWrite } = useContactAccess(workspaceId);
  const schema = useContactSchema(workspaceId);
  const contact = useResource(
    () =>
      unwrap(
        client.GET("/api/v1/workspaces/{ws}/contacts/{key}", {
          params: { path: { ws: workspaceId, key: contactKey } },
        }),
      ),
    [workspaceId, contactKey],
  );
  const [editing, setEditing] = useState(false);
  const [deleting, setDeleting] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);

  if (contact.status === "loading" || schema.status === "loading") {
    return (
      <div className="page">
        <LoadingState label="Loading the contact" />
      </div>
    );
  }
  if (contact.status === "error") {
    const missing = contact.error instanceof ApiError && contact.error.status === 404;
    return (
      <div className="page">
        <Panel flush>
          {missing ? (
            <EmptyState
              title="No such contact"
              action={
                <Link className="btn" to={`/w/${workspaceId}/contacts`}>
                  Back to contacts
                </Link>
              }
            >
              It may have been deleted.
            </EmptyState>
          ) : (
            <ErrorState {...describeError(contact.error)} onRetry={contact.reload} />
          )}
        </Panel>
      </div>
    );
  }
  if (schema.status === "error") {
    return (
      <div className="page">
        <Panel flush>
          <ErrorState {...describeError(schema.error)} onRetry={schema.reload} />
        </Panel>
      </div>
    );
  }

  const data = contact.data;
  return (
    <div className="page">
      <PageHeader
        title={data.key}
        breadcrumb={{ group: "Audience", section: "Contacts", to: `/w/${workspaceId}/contacts` }}
        lede={`Updated ${formatDateTime(data.updated_at)}. Created ${formatDateTime(data.created_at)}.`}
        primary={
          canWrite ? (
            <Button variant="primary" onClick={() => setEditing(true)}>
              Edit
            </Button>
          ) : null
        }
        secondary={
          <Link className="btn" to={`/w/${workspaceId}/people/${data.person_id}`}>
            Open the profile
          </Link>
        }
        actions={canWrite ? <Button onClick={() => setDeleting(true)}>Delete</Button> : null}
      />
      {notice ? (
        <p className="notice" role="status">
          {notice}
        </p>
      ) : null}
      <Panel title="Fields">
        <Fields contact={data} fields={schema.data.fields} defined={schema.data.defined} />
      </Panel>
      {editing ? (
        <ContactForm
          workspaceId={workspaceId}
          schema={schema.data}
          contact={data}
          onClose={() => setEditing(false)}
          onSaved={() => {
            setEditing(false);
            setNotice("Saved.");
            contact.reload();
          }}
        />
      ) : null}
      {deleting ? (
        <DeleteDialog workspaceId={workspaceId} contact={data} onClose={() => setDeleting(false)} />
      ) : null}
    </div>
  );
}

function Fields({ contact, fields, defined }: { contact: Contact; fields: SchemaField[]; defined: boolean }) {
  const declared = new Set(fields.map((field) => field.key));
  // Values the definition does not list, such as fields of an older definition.
  const others = Object.keys(contact.fields).filter((key) => !declared.has(key));
  const rows = [...fields.map((field) => field.key), ...others];
  if (rows.length === 0) {
    return (
      <p className="muted">
        {defined
          ? "The contact definition has no fields, so this contact is only a key."
          : "This workspace has no contact definition yet, so this contact is only a key."}
      </p>
    );
  }
  return (
    <dl className="facts">
      {rows.map((key) => {
        const field = fields.find((candidate) => candidate.key === key);
        const text = showValue(contact.fields[key]);
        return (
          <div className="fact" key={key}>
            <dt>
              {key}
              {field?.identifier ? <span className="faint"> · {identifierLabel(field.identifier)}</span> : null}
            </dt>
            <dd>
              {contact.masked_fields.includes(key) ? (
                <HiddenValue what="This field" />
              ) : text === "" ? (
                <Pill>Not set</Pill>
              ) : (
                text
              )}
            </dd>
          </div>
        );
      })}
    </dl>
  );
}

function DeleteDialog({
  workspaceId,
  contact,
  onClose,
}: {
  workspaceId: string;
  contact: Contact;
  onClose: () => void;
}) {
  const navigate = useNavigate();
  const settings = useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/privacy/settings", { params: { path: { ws: workspaceId } } })),
    [workspaceId],
  );
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);

  async function remove() {
    setBusy(true);
    setFailure(null);
    try {
      await unwrapEmpty(
        client.DELETE("/api/v1/workspaces/{ws}/contacts/{key}", {
          params: { path: { ws: workspaceId, key: contact.key } },
        }),
      );
      void navigate(`/w/${workspaceId}/contacts`);
    } catch (error) {
      setFailure(describeError(error));
      setBusy(false);
    }
  }

  return (
    <Dialog title={`Delete ${contact.key}?`} onClose={onClose}>
      <p className="muted">
        This removes the contact record and the fields it set. Anything another source or an event says about the same person
        stays.
      </p>
      {settings.status === "ready" ? (
        <p className="muted">
          {settings.data.orphan_policy === "delete"
            ? "This workspace deletes a person left with no record, event or explicit consent, so if nothing else holds this person they are deleted with the contact."
            : "This workspace keeps a person left with no record, event or explicit consent, so the person stays even if nothing else holds them."}
        </p>
      ) : settings.status === "error" ? (
        <p className="muted">Whether the person is deleted too depends on the workspace's setting for people left with nothing.</p>
      ) : (
        <LoadingState label="Checking what happens to the person" />
      )}
      {failure ? <InlineError {...failure} /> : null}
      <div className="dialog-actions">
        <Button onClick={onClose}>Keep contact</Button>
        <Button variant="primary" onClick={() => void remove()} disabled={busy}>
          {busy ? "Deleting" : "Delete contact"}
        </Button>
      </div>
    </Dialog>
  );
}
