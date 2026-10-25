import { useNavigate, useParams } from "react-router";

import { PageHeader, Panel } from "../../ui";
import { RequireAdmin } from "../members";
import { DestinationForm } from "./DestinationForm";

export function NewDestinationPage() {
  const { ws = "" } = useParams();
  const navigate = useNavigate();
  return (
    <RequireAdmin key={ws} workspaceId={ws}>
      <div className="page">
        <PageHeader
          title="New destination"
          lede="Choose a type and fill in its settings. The credential is a shared workspace credential: stored encrypted, never shown again, and rotated in one place."
        />
        <Panel>
          <DestinationForm workspaceId={ws} onSaved={(saved) => void navigate(`/w/${ws}/destinations/${saved.id}`)} />
        </Panel>
      </div>
    </RequireAdmin>
  );
}
