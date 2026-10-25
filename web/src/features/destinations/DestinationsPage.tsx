import { Link, useParams } from "react-router";

import { client, unwrap, useResource } from "../../api";
import { EmptyState, formatDateTime, PageHeader, Panel, Pill, ResourceView } from "../../ui";
import { RequireAdmin } from "../members";
import { kindLabel, plural } from "./labels";
import { SigningSecretPanel } from "./SigningSecretPanel";

export function DestinationsPage() {
  const { ws = "" } = useParams();
  return (
    <RequireAdmin key={ws} workspaceId={ws}>
      <Destinations workspaceId={ws} />
    </RequireAdmin>
  );
}

function NewDestinationLink({ workspaceId }: { workspaceId: string }) {
  return (
    <Link className="btn btn-primary" to={`/w/${workspaceId}/destinations/new`}>
      New destination
    </Link>
  );
}

function Destinations({ workspaceId }: { workspaceId: string }) {
  const destinations = useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/destinations", { params: { path: { ws: workspaceId } } })),
    [workspaceId],
  );

  return (
    <div className="page">
      <PageHeader
        title="Destinations"
        lede="Where messages and events are delivered: a mail server, an email provider or a webhook. A destination holds its settings and uses a shared workspace credential."
        actions={<NewDestinationLink workspaceId={workspaceId} />}
      />
      <Panel flush>
        <ResourceView
          resource={destinations}
          loadingLabel="Loading destinations"
          isEmpty={(data) => data.length === 0}
          empty={
            <EmptyState title="No destinations yet" action={<NewDestinationLink workspaceId={workspaceId} />}>
              Add a destination, check it, and use it to send email or forward events.
            </EmptyState>
          }
        >
          {(data) => (
            <>
              <div className="table-scroll">
                <table className="table">
                  <caption>Destinations of this workspace</caption>
                  <thead>
                    <tr>
                      <th scope="col">Name</th>
                      <th scope="col">Type</th>
                      <th scope="col">Credential</th>
                      <th scope="col">Rate</th>
                      <th scope="col">Burst</th>
                      <th scope="col">Updated</th>
                    </tr>
                  </thead>
                  <tbody>
                    {data.map((destination) => (
                      <tr key={destination.id}>
                        <td className="strong">
                          <Link to={`/w/${workspaceId}/destinations/${destination.id}`}>{destination.name}</Link>
                        </td>
                        <td>{kindLabel(destination.kind)}</td>
                        <td>
                          <Pill tone={destination.credential_id ? "neutral" : "attention"}>
                            {destination.credential_id ? "Credential set" : "No credential"}
                          </Pill>
                        </td>
                        <td>{destination.rate} per second</td>
                        <td>{destination.burst}</td>
                        <td>{formatDateTime(destination.updated_at)}</td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
              <p className="panel-footer faint">{plural(data.length, "destination", "destinations")} in all.</p>
            </>
          )}
        </ResourceView>
      </Panel>
      <SigningSecretPanel workspaceId={workspaceId} />
    </div>
  );
}
