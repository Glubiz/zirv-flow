import { SelectField, TextField } from "../../../ui";
import { FieldIssues } from "../IssueList";
import { issuesAt, type TemplateIssue } from "../model";
import { SenderPicker } from "../SenderPicker";
import { FONT_STACKS, omit, type BlocksContent, type DocumentStyle } from "./document";
import { fieldTarget, type InsertTarget } from "./insertTarget";

const COLOURS: readonly { key: keyof DocumentStyle; label: string }[] = [
  { key: "background_color", label: "Background colour" },
  { key: "text_color", label: "Text colour" },
  { key: "link_color", label: "Link colour" },
  { key: "button_color", label: "Button colour" },
  { key: "button_text_color", label: "Button label colour" },
];

interface Props {
  content: BlocksContent;
  onChange: (next: BlocksContent) => void;
  readOnly: boolean;
  issues: readonly TemplateIssue[];
}

/** The per-document style: background, content width, font stack, and the text, link and button colours. */
export function DocumentStyleForm({ content, onChange, readOnly, issues }: Props) {
  const style = content.settings;
  const set = (patch: Partial<DocumentStyle>) => onChange({ ...content, settings: { ...style, ...patch } });
  return (
    <div className="stack" role="group" aria-label="Document style">
      {COLOURS.map((entry) => (
        <div key={entry.key} className="stack">
          <TextField
            label={entry.label}
            hint="Such as #1a5fb4."
            value={String(style[entry.key])}
            readOnly={readOnly}
            autoComplete="off"
            onChange={(event) => set({ [entry.key]: event.target.value })}
          />
          <FieldIssues issues={issuesAt(issues, `/blocks/settings/${entry.key}`)} />
        </div>
      ))}
      <TextField
        label="Content width in pixels"
        hint="480 to 800."
        type="number"
        min={480}
        max={800}
        value={String(style.content_width)}
        readOnly={readOnly}
        onChange={(event) => set({ content_width: Number(event.target.value) })}
      />
      <FieldIssues issues={issuesAt(issues, "/blocks/settings/content_width")} />
      <SelectField label="Font" value={style.font_family} disabled={readOnly} onChange={(event) => set({ font_family: event.target.value })}>
        {FONT_STACKS.map((font) => (
          <option key={font.id} value={font.id}>
            {font.label}
          </option>
        ))}
      </SelectField>
      <FieldIssues issues={issuesAt(issues, "/blocks/settings/font_family")} />
    </div>
  );
}

/** The envelope of the message: subject, preheader, sender and language. */
export function MessageForm({
  workspaceId,
  content,
  onChange,
  readOnly,
  canAdmin,
  issues,
  onTarget,
}: Props & { workspaceId: string; canAdmin: boolean; onTarget: (target: InsertTarget) => void }) {
  const set = (patch: Partial<BlocksContent>) => onChange({ ...content, ...patch });
  const merge = (label: string) => ({
    onFocus: (event: { currentTarget: HTMLInputElement }) => onTarget(fieldTarget(label, event.currentTarget)),
  });
  return (
    <div className="stack" role="group" aria-label="Message">
      <TextField label="Subject" value={content.subject} readOnly={readOnly} autoComplete="off" {...merge("the subject")} onChange={(event) => set({ subject: event.target.value })} />
      <FieldIssues issues={issuesAt(issues, "/blocks/subject")} />
      <TextField
        label="Preheader"
        hint="The line an inbox shows after the subject."
        value={content.preheader}
        readOnly={readOnly}
        autoComplete="off"
        {...merge("the preheader")}
        onChange={(event) => set({ preheader: event.target.value })}
      />
      <FieldIssues issues={issuesAt(issues, "/blocks/preheader")} />
      {canAdmin ? (
        <SenderPicker workspaceId={workspaceId} value={content.sender ?? ""} disabled={readOnly} onChange={(sender) => set(withOptional(content, "sender", sender))} />
      ) : (
        <TextField label="Sender identity" value={content.sender ?? ""} readOnly mono hint="Workspace admins choose the sender." />
      )}
      <FieldIssues issues={issuesAt(issues, "/blocks/sender")} />
      <TextField
        label="Language"
        hint="A language code such as en or da."
        value={content.lang ?? ""}
        readOnly={readOnly}
        autoComplete="off"
        onChange={(event) => set(withOptional(content, "lang", event.target.value))}
      />
      <FieldIssues issues={issuesAt(issues, "/blocks/lang")} />
    </div>
  );
}

/** The patch that sets an optional text field, or leaves it out when empty. */
function withOptional(content: BlocksContent, key: "sender" | "lang", value: string): BlocksContent {
  return value === "" ? omit(content, key) : { ...content, [key]: value };
}
