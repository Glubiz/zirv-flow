import { useState } from "react";
import { Link } from "react-router";

import { Button, Dialog, SelectField } from "../../../ui";
import { fontCss, readTokens } from "../../themes";
import { DocumentStyleForm } from "./DocumentPanel";
import type { BlocksContent, ThemeRef } from "./document";
import type { ThemeScope } from "./themeScope";
import type { TemplateIssue } from "../model";

const FOLLOW_DEFAULT = "default";

interface Props {
  workspaceId: string;
  scope: ThemeScope;
  content: BlocksContent;
  onChange: (next: BlocksContent) => void;
  readOnly: boolean;
  issues: readonly TemplateIssue[];
}

function setTheme(content: BlocksContent, theme: ThemeRef): BlocksContent {
  return { ...content, theme };
}

/** The Theme tab: follow the workspace default or a named theme, and read what the theme sets. A document with no theme edits its own style here. */
export function ThemePanel({ workspaceId, scope, content, onChange, readOnly, issues }: Props) {
  const [switching, setSwitching] = useState(false);

  if (scope.ref === "none") {
    return (
      <div className="stack">
        <p className="notice" role="status">
          This email has its own style and no theme.
        </p>
        {readOnly ? null : (
          <div>
            <Button onClick={() => setSwitching(true)}>Switch to a theme</Button>
          </div>
        )}
        <DocumentStyleForm content={content} readOnly={readOnly} issues={issues} onChange={onChange} />
        {switching ? (
          <Dialog title="Switch to a theme?" onClose={() => setSwitching(false)}>
            <p>The look of this email will come from the workspace default theme instead of the style set here. This can only be undone from a version.</p>
            <div className="row">
              <Button
                variant="primary"
                onClick={() => {
                  setSwitching(false);
                  onChange(setTheme(content, "default"));
                }}
              >
                Switch to a theme
              </Button>
              <Button onClick={() => setSwitching(false)}>Cancel</Button>
            </div>
          </Dialog>
        ) : null}
      </div>
    );
  }

  const chosen = scope.ref === "default" ? FOLLOW_DEFAULT : scope.ref.id;
  const listed = scope.choices.some((theme) => theme.id === chosen);
  const tokens = scope.current ? readTokens(scope.current.tokens) : null;
  return (
    <div className="stack" role="group" aria-label="Theme">
      <SelectField
        label="Theme"
        value={chosen}
        disabled={readOnly}
        onChange={(event) => onChange(setTheme(content, event.target.value === FOLLOW_DEFAULT ? "default" : { id: event.target.value }))}
      >
        <option value={FOLLOW_DEFAULT}>Follow the workspace default</option>
        {scope.choices.map((theme) => (
          <option key={theme.id} value={theme.id}>
            {theme.name}
          </option>
        ))}
        {chosen !== FOLLOW_DEFAULT && !listed ? <option value={chosen}>{scope.name}</option> : null}
      </SelectField>
      {scope.current && tokens ? (
        <>
          <p className="faint">
            {scope.ref === "default" ? `The workspace default is ${scope.current.name}.` : `${scope.current.name} sets the look of this email.`}
          </p>
          <dl className="facts" aria-label="What the theme sets">
            {(
              [
                ["Primary", tokens.primary],
                ["Background", tokens.background],
                ["Text", tokens.text],
                ["Link", tokens.link],
                ["Button", tokens.button],
                ["Button text", tokens.button_text],
              ] as const
            ).map(([label, colour]) => (
              <div key={label} className="fact">
                <dt>{label}</dt>
                <dd>
                  <span className="th-swatch" aria-hidden="true" style={{ background: colour }} /> <span className="mono">{colour}</span>
                </dd>
              </div>
            ))}
            <div className="fact">
              <dt>Font</dt>
              <dd style={{ fontFamily: fontCss(tokens.font) }}>{tokens.font}</dd>
            </div>
            <div className="fact">
              <dt>Content width</dt>
              <dd>{tokens.content_width} px</dd>
            </div>
          </dl>
          <Link to={`/w/${workspaceId}/themes/${scope.current.id}`}>Edit theme</Link>
        </>
      ) : (
        <p className="faint">{scope.ref === "default" ? "No theme is set as the workspace default yet. The email uses its own style until one is." : "This theme could not be found."}</p>
      )}
    </div>
  );
}
