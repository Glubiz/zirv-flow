import { useState, type SyntheticEvent } from "react";
import { Link, useNavigate, useParams } from "react-router";

import { describeError } from "../../api";
import {
  Button,
  EmptyState,
  formatDateTime,
  HiddenValue,
  InlineError,
  PageHeader,
  Panel,
  ResourceView,
  TextField,
} from "../../ui";
import { ContactForm } from "./ContactForm";
import { showValue, useContactAccess, useContactSchema, type Contact, type ContactSchema } from "./schema";
import { useContacts, type ContactQuery } from "./useContacts";

/** How many definition fields the table shows next to the key. */
const COLUMNS = 3;

export function ContactsPage() {
  const { ws = "" } = useParams();
  return <Contacts key={ws} workspaceId={ws} />;
}

function Contacts({ workspaceId }: { workspaceId: string }) {
  const { canWrite, canDefine } = useContactAccess(workspaceId);
  const schema = useContactSchema(workspaceId);
  const [query, setQuery] = useState<ContactQuery>({ run: 0, text: null });
  const list = useContacts(workspaceId, query);
  const [creating, setCreating] = useState(false);
  const navigate = useNavigate();
  const searching = query.text !== null;

  return (
    <div className="page">
      <PageHeader
        title="Contacts" breadcrumb={{ group: "Audience", section: "Contacts" }}
        lede="People you keep in zirv flow itself, for companies without another customer system. A contact is a person with a record of its own, identified by your key."
        actions={
          <>
            {canDefine ? (
              <Link className="btn" to={`/w/${workspaceId}/contact-definition`}>
                Definition
              </Link>
            ) : null}
            {canWrite && schema.status === "ready" ? (
              <Button variant="primary" onClick={() => setCreating(true)}>
                New contact
              </Button>
            ) : null}
          </>
        }
      />
      <SearchForm
        searching={searching}
        onSearch={(text) => setQuery((current) => ({ run: current.run + 1, text }))}
      />
      <Panel flush>
        <ResourceView
          resource={list.resource}
          loadingLabel="Loading contacts"
          isEmpty={(data) => data.length === 0}
          empty={
            searching ? (
              <EmptyState title="No contact matches">
                Search matches the start of a contact key, or a whole email address or phone number.
              </EmptyState>
            ) : (
              <EmptyState
                title="No contacts yet"
                action={
                  canWrite && schema.status === "ready" ? (
                    <Button variant="primary" onClick={() => setCreating(true)}>
                      New contact
                    </Button>
                  ) : null
                }
              >
                Add a contact here, or send many at once through the API.
              </EmptyState>
            )
          }
        >
          {(data) => (
            <>
              <div className="table-scroll">
                <table className="table">
                  <caption>{searching ? "Contacts that match" : "Contacts, ordered by key"}</caption>
                  <ColumnsFor schema={schema.status === "ready" ? schema.data : null} contacts={data} />
                </table>
              </div>
              {list.nextAfter !== null ? (
                <div className="panel-pad">
                  {list.moreError ? <InlineError {...describeError(list.moreError)} /> : null}
                  <Button onClick={list.loadMore} disabled={list.loadingMore}>
                    {list.loadingMore ? "Loading" : "Load more"}
                  </Button>
                </div>
              ) : null}
            </>
          )}
        </ResourceView>
      </Panel>
      {!canWrite && schema.status === "ready" ? (
        <p className="muted">You can look at contacts but not change them.</p>
      ) : null}
      {creating && schema.status === "ready" ? (
        <ContactForm
          workspaceId={workspaceId}
          schema={schema.data}
          onClose={() => setCreating(false)}
          onSaved={(contact) => {
            setCreating(false);
            void navigate(`/w/${workspaceId}/contacts/${encodeURIComponent(contact.key)}`);
          }}
        />
      ) : null}
    </div>
  );
}

function SearchForm({ searching, onSearch }: { searching: boolean; onSearch: (text: string | null) => void }) {
  const [text, setText] = useState("");

  function submit(event: SyntheticEvent) {
    event.preventDefault();
    onSearch(text.trim() === "" ? null : text.trim());
  }

  return (
    <Panel>
      <form className="inline-form" onSubmit={submit} role="search" aria-label="Search contacts">
        <TextField
          label="Search"
          hint="The start of a contact key, or a whole email address or phone number."
          value={text}
          onChange={(event) => setText(event.target.value)}
          autoComplete="off"
        />
        <div className="row">
          <Button type="submit" variant="primary">
            Search
          </Button>
          {searching ? (
            <Button
              onClick={() => {
                setText("");
                onSearch(null);
              }}
            >
              Show all
            </Button>
          ) : null}
        </div>
      </form>
    </Panel>
  );
}

function cell(contact: Contact, key: string) {
  if (contact.masked_fields.includes(key)) return <HiddenValue />;
  const text = showValue(contact.fields[key]);
  return text === "" ? <span className="faint">—</span> : text;
}

function ColumnsFor({ schema, contacts }: { schema: ContactSchema | null; contacts: Contact[] }) {
  const keys = (schema?.fields ?? []).slice(0, COLUMNS).map((field) => field.key);
  return (
    <>
      <thead>
        <tr>
          <th scope="col">Key</th>
          {keys.map((key) => (
            <th scope="col" key={key}>
              {key}
            </th>
          ))}
          <th scope="col">Updated</th>
        </tr>
      </thead>
      <tbody>
        {contacts.map((contact) => (
          <ContactRow key={contact.key} contact={contact} columns={keys} />
        ))}
      </tbody>
    </>
  );
}

function ContactRow({ contact, columns }: { contact: Contact; columns: string[] }) {
  const { ws = "" } = useParams();
  return (
    <tr>
      <td className="strong">
        <Link to={`/w/${ws}/contacts/${encodeURIComponent(contact.key)}`}>{contact.key}</Link>
      </td>
      {columns.map((key) => (
        <td key={key}>{cell(contact, key)}</td>
      ))}
      <td>{formatDateTime(contact.updated_at)}</td>
    </tr>
  );
}
