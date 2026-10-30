import { Link, useOutletContext, useParams } from "react-router";

import { client, unwrap, useResource } from "../api";
import { PageHeader, Panel, ResourceView } from "../ui";
import { destinationUrl, navigationGroups } from "./navigation";
import type { ShellContext } from "./Shell";

export function SettingsPage() {
  const { ws = "" } = useParams();
  const { destinations } = useOutletContext<ShellContext>();
  const items = destinations.filter((item) => item.area === "settings");
  return (
    <div className="page">
      <PageHeader title="Settings" lede="Set up your workspace, sending and integrations." />
      <div className="settings-grid">
        {navigationGroups(items).map((group) => (
          <Panel key={group} title={group}>
            <ul className="settings-links">
              {items.filter((item) => item.group === group).map((item) => (
                <li key={item.route}>
                  <Link to={destinationUrl(item, `/w/${ws}`) ?? "/w"}>{item.label}</Link>
                  {item.route === "sending-domains" ? <SendingStatus workspaceId={ws} /> : null}
                </li>
              ))}
            </ul>
          </Panel>
        ))}
      </div>
    </div>
  );
}

function SendingStatus({ workspaceId }: { workspaceId: string }) {
  const domains = useResource(() => unwrap(client.GET("/api/v1/workspaces/{ws}/sending-domains", { params: { path: { ws: workspaceId } } })), [workspaceId]);
  return (
    <ResourceView resource={domains} loadingLabel="Checking sending domains">
      {(data) => {
        const count = data.filter((domain) => domain.status !== "verified").length;
        return count > 0 ? <span className="settings-warning">{count} not verified</span> : null;
      }}
    </ResourceView>
  );
}
