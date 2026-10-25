import { useState } from "react";

import { client, describeError, unwrap, type ErrorSummary } from "../../api";
import { InlineError, SelectField } from "../../ui";
import type { Template } from "./model";

interface Props {
  workspaceId: string;
  template: Template;
  /** Whether the viewer may change the template. */
  editable: boolean;
  onSaved: (next: Template) => void;
}

/** The per-template override of the workspace's automatic UTM parameters: follow the workspace or stay off. */
export function UtmOverride({ workspaceId, template, editable, onSaved }: Props) {
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);

  async function choose(utm: "inherit" | "off") {
    setBusy(true);
    setFailure(null);
    try {
      const saved = await unwrap(
        client.PUT("/api/v1/workspaces/{ws}/templates/{id}", { params: { path: { ws: workspaceId, id: template.id } }, body: { utm } }),
      );
      onSaved(saved);
    } catch (error) {
      setFailure(describeError(error));
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="tp-utm">
      <SelectField
        label="UTM parameters on links"
        hint="Whether the workspace's automatic UTM parameters tag the links of this template."
        value={template.utm}
        disabled={!editable || busy}
        onChange={(event) => void choose(event.target.value === "off" ? "off" : "inherit")}
      >
        <option value="inherit">Follow the workspace setting</option>
        <option value="off">Off for this template</option>
      </SelectField>
      {failure ? <InlineError {...failure} /> : null}
    </div>
  );
}
