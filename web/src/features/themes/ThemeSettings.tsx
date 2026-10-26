import { useState } from "react";
import { Link } from "react-router";

import { client, unwrap, useResource } from "../../api";
import { Button, SelectField } from "../../ui";
import { AssetPicker, type AssetPick } from "../assets";
import { usePublishedSnippets } from "../snippets";
import { FONT_STACKS } from "../templates";
import { COLOURS, CORNERS, MAX_WIDTH, MIN_WIDTH, WARNING_AFTER, contrastNote, cornersOf, isContrast, isHex, type ColourKey, type ThemeIssue, type ThemeTokens } from "./model";

interface Props {
  workspaceId: string;
  tokens: ThemeTokens;
  issues: readonly ThemeIssue[];
  readOnly: boolean;
  onChange: (patch: Partial<ThemeTokens>) => void;
}

function useLogo(workspaceId: string, id: string) {
  return useResource(async () => {
    if (id === "") return null;
    return unwrap(client.GET("/api/v1/workspaces/{ws}/assets/{id}", { params: { path: { ws: workspaceId, id } } }));
  }, [workspaceId, id]);
}

/** The settings column: logo, colours, text and layout, buttons and links, footer. */
export function ThemeSettings({ workspaceId, tokens, issues, readOnly, onChange }: Props) {
  return (
    <form className="th-settings" aria-label="Theme settings" onSubmit={(event) => event.preventDefault()}>
      <LogoSection workspaceId={workspaceId} logo={tokens.logo} readOnly={readOnly} onChange={onChange} />
      <ColourSection tokens={tokens} issues={issues} readOnly={readOnly} onChange={onChange} />
      <section className="th-section" aria-labelledby="th-type">
        <h2 id="th-type" className="th-h">
          Text and layout
        </h2>
        <div className="th-grid-2">
          <SelectField label="Font" value={tokens.font} disabled={readOnly} onChange={(event) => onChange({ font: event.target.value })}>
            {FONT_STACKS.map((stack) => (
              <option key={stack.id} value={stack.id}>
                {stack.label}
              </option>
            ))}
          </SelectField>
          <WidthField width={tokens.content_width} readOnly={readOnly} onChange={(width) => onChange({ content_width: width })} />
        </div>
      </section>
      <section className="th-section" aria-labelledby="th-buttons">
        <h2 id="th-buttons" className="th-h">
          Buttons and links
        </h2>
        <div className="row">
          <div role="group" aria-label="Button corners" className="row">
            <span>Corners</span>
            <div className="seg-tabs">
              {CORNERS.map((corners) => (
                <button
                  key={corners.id}
                  type="button"
                  className={cornersOf(tokens.button_radius) === corners.id ? "seg seg-on" : "seg"}
                  aria-pressed={cornersOf(tokens.button_radius) === corners.id}
                  disabled={readOnly}
                  onClick={() => onChange({ button_radius: corners.radius })}
                >
                  {corners.label}
                </button>
              ))}
            </div>
          </div>
          <label className="check">
            <input type="checkbox" checked={tokens.link_underline} disabled={readOnly} onChange={(event) => onChange({ link_underline: event.target.checked })} />
            <span>Underline links</span>
          </label>
        </div>
      </section>
      <FooterSection workspaceId={workspaceId} footer={tokens.footer_snippet} readOnly={readOnly} onChange={onChange} />
    </form>
  );
}

function LogoSection({ workspaceId, logo, readOnly, onChange }: { workspaceId: string; logo: string | null | undefined; readOnly: boolean; onChange: Props["onChange"] }) {
  const [picking, setPicking] = useState(false);
  const asset = useLogo(workspaceId, logo ?? "");
  function pick(choice: AssetPick) {
    setPicking(false);
    if ("asset" in choice) onChange({ logo: choice.asset.id });
  }
  return (
    <section className="th-section" aria-labelledby="th-logo">
      <h2 id="th-logo" className="th-h">
        Logo
      </h2>
      <div className="row">
        <div className="th-logo">
          {asset.status === "ready" && asset.data ? (
            <img src={asset.data.url} alt={`Logo: ${asset.data.name}`} />
          ) : (
            <span className="faint">{logo ? (asset.status === "error" ? "Logo unavailable" : "Loading") : "No logo"}</span>
          )}
        </div>
        {readOnly ? null : (
          <>
            <Button onClick={() => setPicking(true)}>Change</Button>
            {logo ? <Button onClick={() => onChange({ logo: null })}>Remove</Button> : null}
          </>
        )}
      </div>
      <AssetPicker open={picking} onPick={pick} onClose={() => setPicking(false)} />
    </section>
  );
}

function ColourSection({ tokens, issues, readOnly, onChange }: { tokens: ThemeTokens; issues: readonly ThemeIssue[]; readOnly: boolean; onChange: Props["onChange"] }) {
  /** What was typed in a field that is not a colour yet: held here and never saved. */
  const [typed, setTyped] = useState<Partial<Record<ColourKey, string>>>({});

  function type(key: ColourKey, text: string) {
    if (isHex(text)) {
      setTyped((held) => Object.fromEntries(Object.entries(held).filter(([name]) => name !== key)));
      onChange({ [key]: text });
    } else {
      setTyped((held) => ({ ...held, [key]: text }));
    }
  }

  return (
    <section className="th-section" aria-labelledby="th-colours">
      <h2 id="th-colours" className="th-h">
        Colours
      </h2>
      <div className="th-grid-2">
        {COLOURS.map(({ key, label }) => {
          const shown = typed[key] ?? tokens[key];
          const bad = typed[key] !== undefined;
          return (
            <div key={key} className="th-colour-cell">
              <div className="field">
                <label className="field-label" htmlFor={`th-colour-${key}`}>
                  {label}
                </label>
                <span className="th-colour">
                  <span className="th-swatch" aria-hidden="true" style={{ background: bad ? tokens[key] : shown }} />
                  <input
                    id={`th-colour-${key}`}
                    className="input input-mono"
                    value={shown}
                    readOnly={readOnly}
                    spellCheck={false}
                    autoComplete="off"
                    aria-invalid={bad || undefined}
                    onChange={(event) => type(key, event.target.value.trim())}
                  />
                </span>
                {bad ? <span className="field-error">Use #rgb or #rrggbb. The last good colour is kept.</span> : null}
              </div>
              {WARNING_AFTER[key].flatMap((at) =>
                issues.filter((issue) => issue.at === at && isContrast(issue)).map((issue) => (
                  <p key={at} className="th-warn" data-th-warning tabIndex={-1}>
                    {contrastNote(issue)}
                  </p>
                )),
              )}
            </div>
          );
        })}
      </div>
      <p className="faint">Mail apps in dark mode can still change these colours.</p>
    </section>
  );
}

function WidthField({ width, readOnly, onChange }: { width: number; readOnly: boolean; onChange: (width: number) => void }) {
  const [typed, setTyped] = useState<string | null>(null);
  const shown = typed ?? String(width);
  const value = Number(shown);
  const bad = typed !== null && !(Number.isInteger(value) && value >= MIN_WIDTH && value <= MAX_WIDTH);
  return (
    <div className="field">
      <label className="field-label" htmlFor="th-width">
        Content width
      </label>
      <span className="th-colour">
        <input
          id="th-width"
          className="input"
          type="number"
          min={MIN_WIDTH}
          max={MAX_WIDTH}
          value={shown}
          readOnly={readOnly}
          aria-invalid={bad || undefined}
          onChange={(event) => {
            const text = event.target.value;
            const next = Number(text);
            if (text !== "" && Number.isInteger(next) && next >= MIN_WIDTH && next <= MAX_WIDTH) {
              setTyped(null);
              onChange(next);
            } else {
              setTyped(text);
            }
          }}
        />
        <span className="faint">px</span>
      </span>
      {bad ? <span className="field-error">{MIN_WIDTH} to {MAX_WIDTH}. The last good width is kept.</span> : null}
    </div>
  );
}

function FooterSection({ workspaceId, footer, readOnly, onChange }: { workspaceId: string; footer: string | null | undefined; readOnly: boolean; onChange: Props["onChange"] }) {
  const snippets = usePublishedSnippets(workspaceId);
  const options = snippets.status === "ready" ? snippets.data : [];
  const orphan = footer && snippets.status === "ready" && !options.some((snippet) => snippet.id === footer) ? footer : null;
  return (
    <section className="th-section" aria-labelledby="th-footer">
      <h2 id="th-footer" className="th-h">
        Footer
      </h2>
      <SelectField
        label="Footer snippet"
        value={footer ?? ""}
        disabled={readOnly || snippets.status === "loading"}
        onChange={(event) => onChange({ footer_snippet: event.target.value === "" ? null : event.target.value })}
      >
        <option value="">No footer</option>
        {options.map((snippet) => (
          <option key={snippet.id} value={snippet.id}>
            {snippet.name}
          </option>
        ))}
        {orphan ? <option value={orphan}>A snippet that is not published</option> : null}
      </SelectField>
      {footer ? <Link to={`/w/${workspaceId}/snippets/${footer}`}>Open snippet</Link> : null}
    </section>
  );
}
