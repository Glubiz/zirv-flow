import type { ReactNode } from "react";
import { Link } from "react-router";

import { Button, SelectField, TextAreaField, TextField } from "../../../ui";
import { NumberField } from "./NumberField";
import { FieldIssues } from "../IssueList";
import { issuesAt, type TemplateIssue } from "../model";
import { blockLabel, issuesOf, omit, withVisibleIf, type Align, type Block, type ButtonBlock, type ImageBlock, type Section } from "./document";
import { fieldTarget, type InsertTarget } from "./insertTarget";
import { RichTextEditor } from "./RichTextEditor";
import { Visibility } from "./Visibility";

interface Props {
  block: Block;
  onChange: (next: Block) => void;
  readOnly: boolean;
  onTarget: (target: InsertTarget) => void;
  issues: readonly TemplateIssue[];
  /** Opens the image library for an image block. */
  onPickImage?: () => void;
  /** A document: the workspace for the Visibility condition. Absent in a snippet, which has no Visibility and keeps every setting in view. */
  workspaceId?: string;
  /** What a snippet block shows: the snippet's name and live version, when known. */
  snippetInfo?: { name: string; version: number | null; archived: boolean; href: string };
}

/** The settings a document keeps behind a disclosure; a snippet shows them as they are. */
function More({ tucked, children }: { tucked: boolean; children: ReactNode }) {
  return tucked ? (
    <details className="be-more">
      <summary>More settings</summary>
      <div className="stack">{children}</div>
    </details>
  ) : (
    <>{children}</>
  );
}

const ALIGNS: readonly { id: Align; label: string }[] = [
  { id: "left", label: "Left" },
  { id: "center", label: "Center" },
  { id: "right", label: "Right" },
];

/** Sets or, for an empty value, removes an optional field. */
function optional<T extends Block>(block: T, key: string, value: string | number | undefined): T {
  if (value === undefined || value === "") return omit(block, key);
  return { ...block, [key]: value };
}

function UtmOffField<T extends ImageBlock | ButtonBlock>({ block, readOnly, onChange }: { block: T; readOnly: boolean; onChange: (next: T) => void }) {
  return (
    <label className="check">
      <input
        type="checkbox"
        checked={block.utm_off === true}
        disabled={readOnly}
        onChange={(event) => onChange(event.target.checked ? { ...block, utm_off: true as const } : omit(block, "utm_off"))}
      />
      <span>Do not add UTM parameters</span>
    </label>
  );
}

function AlignField({ value, onChange, readOnly, label }: { value: Align; onChange: (next: Align) => void; readOnly: boolean; label: string }) {
  return (
    <SelectField label={label} value={value} disabled={readOnly} onChange={(event) => onChange(event.target.value as Align)}>
      {ALIGNS.map((entry) => (
        <option key={entry.id} value={entry.id}>
          {entry.label}
        </option>
      ))}
    </SelectField>
  );
}

/** The settings of the selected block: one panel per block type. */
export function BlockSettings({ block, onChange, readOnly, onTarget, issues, onPickImage, workspaceId, snippetInfo }: Props) {
  const own = issuesOf(issues, block.id);
  const at = (field: string) => issuesAt(own, `/blocks/block/${block.id}/${field}`);
  const tucked = workspaceId !== undefined;
  const merge = (label: string) => ({
    onFocus: (event: { currentTarget: HTMLInputElement | HTMLTextAreaElement }) => onTarget(fieldTarget(label, event.currentTarget)),
  });

  return (
    <div className="stack" role="group" aria-label={`Settings of the ${blockLabel(block.type).toLowerCase()} block`}>
      {block.type === "heading" ? (
        <>
          <TextField label="Heading text" value={block.text} readOnly={readOnly} autoComplete="off" {...merge("the heading")} onChange={(event) => onChange({ ...block, text: event.target.value })} />
          <FieldIssues issues={at("text")} />
          <More tucked={tucked}>
            <SelectField
              label="Level"
              value={String(block.level)}
              disabled={readOnly}
              onChange={(event) => onChange({ ...block, level: Number(event.target.value) as 1 | 2 | 3 })}
            >
              <option value="1">Heading 1</option>
              <option value="2">Heading 2</option>
              <option value="3">Heading 3</option>
            </SelectField>
            <AlignField label="Alignment" value={block.align ?? "left"} readOnly={readOnly} onChange={(align) => onChange({ ...block, align })} />
          </More>
        </>
      ) : null}
      {block.type === "text" ? (
        <>
          <RichTextEditor label="Text" value={block.content} readOnly={readOnly} onTarget={onTarget} onChange={(content) => onChange({ ...block, content })} />
          <FieldIssues issues={at("content")} />
          <More tucked={tucked}>
            <AlignField label="Alignment" value={block.align ?? "left"} readOnly={readOnly} onChange={(align) => onChange({ ...block, align })} />
          </More>
        </>
      ) : null}
      {block.type === "image" ? (
        <>
          {onPickImage && !readOnly ? (
            <div>
              <Button onClick={onPickImage}>Choose image…</Button>
            </div>
          ) : null}
          {"url" in block.src ? (
            <TextField
              label="Image address"
              hint="An https address, or a merge tag that gives one."
              value={block.src.url}
              readOnly={readOnly}
              autoComplete="off"
              {...merge("the image address")}
              onChange={(event) => onChange({ ...block, src: { url: event.target.value } })}
            />
          ) : (
            <TextField
              label="Image source"
              hint={onPickImage && !readOnly ? "Choose another image to replace it." : "Set outside this editor; kept as it is."}
              value={"asset" in block.src ? `Uploaded image ${block.src.asset}` : "The theme logo"}
              readOnly
              autoComplete="off"
              onChange={() => undefined}
            />
          )}
          <FieldIssues issues={at("src")} />
          <TextField
            label="Alt text"
            hint="Describes the image to people who cannot see it."
            value={block.alt}
            readOnly={readOnly}
            autoComplete="off"
            {...merge("the alt text")}
            onChange={(event) => onChange({ ...block, alt: event.target.value })}
          />
          <FieldIssues issues={at("alt")} />
          <More tucked={tucked}>
            <TextField
              label="Link address"
              hint="Optional. Where a click on the image goes."
              value={block.href ?? ""}
              readOnly={readOnly}
              autoComplete="off"
              {...merge("the image link")}
              onChange={(event) => onChange(optional(block, "href", event.target.value))}
            />
            <FieldIssues issues={at("href")} />
            {block.href ? <UtmOffField block={block} readOnly={readOnly} onChange={onChange} /> : null}
            <TextField
              label="Width in pixels"
              hint="Optional, up to 800."
              type="number"
              min={1}
              max={800}
              value={block.width === undefined ? "" : String(block.width)}
              readOnly={readOnly}
              onChange={(event) => onChange(optional(block, "width", event.target.value === "" ? undefined : Number(event.target.value)))}
            />
            <FieldIssues issues={at("width")} />
          </More>
        </>
      ) : null}
      {block.type === "button" ? (
        <>
          <TextField label="Label" value={block.label} readOnly={readOnly} autoComplete="off" {...merge("the button label")} onChange={(event) => onChange({ ...block, label: event.target.value })} />
          <FieldIssues issues={at("label")} />
          <TextField
            label="URL"
            hint="An https, http or mailto address."
            value={block.url}
            readOnly={readOnly}
            autoComplete="off"
            {...merge("the button address")}
            onChange={(event) => onChange({ ...block, url: event.target.value })}
          />
          <FieldIssues issues={at("url")} />
          <UtmOffField block={block} readOnly={readOnly} onChange={onChange} />
          <More tucked={tucked}>
            <AlignField label="Alignment" value={block.align} readOnly={readOnly} onChange={(align) => onChange({ ...block, align })} />
          </More>
        </>
      ) : null}
      {block.type === "divider" ? (
        <>
          <TextField
            label="Line colour"
            hint="Optional. Such as #cccccc."
            value={block.color ?? ""}
            readOnly={readOnly}
            autoComplete="off"
            onChange={(event) => onChange(optional(block, "color", event.target.value))}
          />
          <FieldIssues issues={at("color")} />
        </>
      ) : null}
      {block.type === "spacer" ? (
        <>
          <NumberField
            key={block.id}
            label="Height in pixels"
            hint="1 to 200."
            min={1}
            max={200}
            value={block.height}
            readOnly={readOnly}
            onValue={(height) => onChange({ ...block, height })}
          />
          <FieldIssues issues={at("height")} />
        </>
      ) : null}
      {block.type === "raw_html" ? (
        <>
          <TextAreaField
            label="HTML"
            hint="Your own HTML, passed through as it is."
            mono
            rows={8}
            spellCheck={false}
            value={block.html}
            readOnly={readOnly}
            {...merge("the HTML")}
            onChange={(event) => onChange({ ...block, html: event.target.value })}
          />
          <FieldIssues issues={at("html")} />
        </>
      ) : null}
      {block.type === "snippet" ? (
        <div className="stack">
          <p className="muted">
            {snippetInfo ? `This block shows the snippet ${snippetInfo.name}${snippetInfo.version === null ? "" : `, version ${snippetInfo.version}`}.` : `This block shows snippet ${block.snippet_id}.`}{snippetInfo?.archived ? " It is archived." : ""} Its blocks are edited in the snippet.
          </p>
          {snippetInfo ? (
            <Link to={snippetInfo.href} target="_blank" rel="noreferrer">
              Open snippet
            </Link>
          ) : null}
        </div>
      ) : null}
      {workspaceId !== undefined ? (
        <Visibility workspaceId={workspaceId} value={block.visible_if} readOnly={readOnly} issues={own} onChange={(next) => onChange(withVisibleIf(block, next))} />
      ) : null}
      <FieldIssues issues={own.filter((issue) => issue.at === `/blocks/block/${block.id}`)} />
    </div>
  );
}

/** The settings of a section: its own colour and padding, and in a document when it shows. */
export function SectionSettings({
  section,
  onChange,
  readOnly,
  issues,
  workspaceId,
}: {
  section: Section;
  onChange: (next: Section) => void;
  readOnly: boolean;
  issues: readonly TemplateIssue[];
  workspaceId: string;
}) {
  const own = issuesOf(issues, section.id);
  const set = (key: "background_color" | "padding", value: string) => {
    onChange(value === "" ? omit(section, key) : { ...section, [key]: value });
  };
  return (
    <div className="stack" role="group" aria-label="Settings of the section">
      <More tucked>
        <TextField label="Section background" hint="Optional. Such as #f4f4f4." value={section.background_color ?? ""} readOnly={readOnly} autoComplete="off" onChange={(event) => set("background_color", event.target.value)} />
        <TextField label="Section padding" hint="Optional. One to four lengths in pixels, such as 20px 0." value={section.padding ?? ""} readOnly={readOnly} autoComplete="off" onChange={(event) => set("padding", event.target.value)} />
      </More>
      <Visibility workspaceId={workspaceId} value={section.visible_if} readOnly={readOnly} issues={own} onChange={(next) => onChange(withVisibleIf(section, next))} />
      <FieldIssues issues={own.filter((issue) => !issue.at.includes("/visible_if"))} />
    </div>
  );
}
