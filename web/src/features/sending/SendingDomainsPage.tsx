import { useState } from "react";
import { Link, useNavigate, useParams } from "react-router";

import { client, describeError, unwrap, useResource, type ErrorSummary } from "../../api";
import { Button, Dialog, EmptyState, formatDateTime, InlineError, PageHeader, Panel, Pill, plural, ResourceView, TextField } from "../../ui";
import { RequireAdmin } from "../members";
import { matching } from "./labels";

export function SendingDomainsPage() {
  const { ws = "" } = useParams();
  return (
    <RequireAdmin key={ws} workspaceId={ws}>
      <Domains workspaceId={ws} />
    </RequireAdmin>
  );
}

function Domains({ workspaceId }: { workspaceId: string }) {
  const navigate = useNavigate();
  const domains = useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/sending-domains", { params: { path: { ws: workspaceId } } })),
    [workspaceId],
  );
  const [adding, setAdding] = useState(false);

  const add = (
    <Button variant="primary" onClick={() => setAdding(true)}>
      Add a domain
    </Button>
  );

  return (
    <div className="page">
      <PageHeader
        title="Sending domains" breadcrumb={{ group: "Sending", section: "Sending domains" }}
        lede="A domain you send email from. Publish its DKIM, SPF and DMARC records; once all three match, its sender identities can send."
        actions={add}
      />
      <Panel flush>
        <ResourceView
          resource={domains}
          loadingLabel="Loading sending domains"
          isEmpty={(data) => data.length === 0}
          empty={
            <EmptyState title="No sending domains yet" action={add}>
              Add the domain you send from. You get the DNS records to publish, and we check them for you.
            </EmptyState>
          }
        >
          {(data) => (
            <>
              <div className="table-scroll">
                <table className="table">
                  <caption>Sending domains of this workspace</caption>
                  <thead>
                    <tr>
                      <th scope="col">Domain</th>
                      <th scope="col">Status</th>
                      <th scope="col">Records matching</th>
                      <th scope="col">Last checked</th>
                    </tr>
                  </thead>
                  <tbody>
                    {data.map((domain) => (
                      <tr key={domain.id}>
                        <td className="strong">
                          <Link to={`/w/${workspaceId}/sending-domains/${domain.id}`}>{domain.domain}</Link>
                        </td>
                        <td>
                          <Pill tone={domain.status === "verified" ? "neutral" : "attention"}>
                            {domain.status === "verified" ? "Verified" : "Not verified"}
                          </Pill>
                        </td>
                        <td>
                          {matching(domain.records)} of {domain.records.length}
                        </td>
                        <td>{domain.last_checked_at ? formatDateTime(domain.last_checked_at) : <span className="faint">Not yet</span>}</td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
              <p className="panel-footer faint">{plural(data.length, "domain", "domains")} in all.</p>
            </>
          )}
        </ResourceView>
      </Panel>
      {adding ? (
        <AddDomainDialog
          workspaceId={workspaceId}
          onClose={() => setAdding(false)}
          onAdded={(id) => void navigate(`/w/${workspaceId}/sending-domains/${id}`)}
        />
      ) : null}
    </div>
  );
}

function AddDomainDialog({ workspaceId, onClose, onAdded }: { workspaceId: string; onClose: () => void; onAdded: (id: string) => void }) {
  const [domain, setDomain] = useState("");
  const [selector, setSelector] = useState("");
  const [spf, setSpf] = useState("");
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);

  async function save() {
    setBusy(true);
    setFailure(null);
    try {
      const created = await unwrap(
        client.POST("/api/v1/workspaces/{ws}/sending-domains", {
          params: { path: { ws: workspaceId } },
          body: {
            domain: domain.trim(),
            ...(selector.trim() !== "" ? { selector: selector.trim() } : {}),
            ...(spf.trim() !== "" ? { spf_mechanism: spf.trim() } : {}),
          },
        }),
      );
      onAdded(created.id);
    } catch (error) {
      setFailure(describeError(error));
      setBusy(false);
    }
  }

  return (
    <Dialog title="Add a sending domain" onClose={onClose}>
      <form
        className="stack"
        onSubmit={(event) => {
          event.preventDefault();
          void save();
        }}
      >
        <TextField
          label="Domain"
          value={domain}
          onChange={(event) => setDomain(event.target.value)}
          hint="Such as mail.example.com. Give an internationalized name in punycode."
          autoComplete="off"
        />
        <TextField
          label="DKIM selector (optional)"
          value={selector}
          onChange={(event) => setSelector(event.target.value)}
          hint="Letters, digits and hyphens. Default zirv."
          autoComplete="off"
        />
        <TextField
          label="SPF mechanism (optional)"
          value={spf}
          onChange={(event) => setSpf(event.target.value)}
          hint="One mechanism the SPF record must contain, such as include:spf.relay.example or ip4:203.0.113.7."
          autoComplete="off"
          mono
        />
        {failure ? <InlineError title={failure.title} detail={failure.detail} traceId={failure.traceId} /> : null}
        <div className="dialog-actions">
          <Button onClick={onClose}>Cancel</Button>
          <Button type="submit" variant="primary" disabled={domain.trim() === "" || busy}>
            Add the domain
          </Button>
        </div>
      </form>
    </Dialog>
  );
}
