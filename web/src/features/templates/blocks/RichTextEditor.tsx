import { useEffect, useId, useRef, useState } from "react";

import { Button } from "../../../ui";
import type { InsertTarget } from "./insertTarget";
import { cleanRichText, linkAllowed } from "./richText";

interface Props {
  label: string;
  /** The rich text source, in the subset the server allows. */
  value: string;
  onChange: (next: string) => void;
  readOnly: boolean;
  onTarget: (target: InsertTarget) => void;
}

const COMMANDS: readonly { command: string; label: string; show: string }[] = [
  { command: "bold", label: "Bold", show: "B" },
  { command: "italic", label: "Italic", show: "I" },
  { command: "underline", label: "Underline", show: "U" },
  { command: "insertUnorderedList", label: "Bulleted list", show: "•" },
  { command: "insertOrderedList", label: "Numbered list", show: "1." },
];

/** A small `contenteditable` editor. Every change goes through the clean pass, so it hands on only what the server allows. */
export function RichTextEditor({ label, value, onChange, readOnly, onTarget }: Props) {
  const area = useRef<HTMLDivElement>(null);
  const emitted = useRef<string | null>(null);
  const range = useRef<Range | null>(null);
  const [linking, setLinking] = useState(false);
  const [url, setUrl] = useState("https://");
  const [urlError, setUrlError] = useState<string | null>(null);
  const urlId = useId();

  useEffect(() => {
    const element = area.current;
    if (element && value !== emitted.current) {
      element.innerHTML = cleanRichText(value);
      emitted.current = value;
    }
  }, [value]);

  function emit() {
    const element = area.current;
    if (!element) return;
    const cleaned = cleanRichText(element.innerHTML);
    emitted.current = cleaned;
    onChange(cleaned);
  }

  function remember() {
    const element = area.current;
    const selection = window.getSelection();
    if (element && selection && selection.rangeCount > 0 && element.contains(selection.anchorNode)) {
      range.current = selection.getRangeAt(0).cloneRange();
    }
  }

  function restore() {
    const selection = window.getSelection();
    if (selection && range.current) {
      selection.removeAllRanges();
      selection.addRange(range.current);
    }
  }

  function run(command: string, argument?: string) {
    area.current?.focus();
    restore();
    // The browser's own editing command: the one way to edit rich text in place without a library.
    const exec = (document as unknown as { execCommand?: (command: string, ui: boolean, value?: string) => boolean }).execCommand;
    if (typeof exec === "function") exec.call(document, command, false, argument);
    emit();
  }

  function target(): InsertTarget {
    return {
      label,
      insert(tag) {
        const element = area.current;
        if (!element?.isConnected) return;
        element.focus();
        restore();
        const selection = window.getSelection();
        const text = document.createTextNode(tag);
        if (selection && selection.rangeCount > 0 && element.contains(selection.anchorNode)) {
          const current = selection.getRangeAt(0);
          current.deleteContents();
          current.insertNode(text);
          current.setStartAfter(text);
          current.collapse(true);
        } else {
          element.append(text);
        }
        remember();
        emit();
      },
    };
  }

  function applyLink() {
    if (!linkAllowed(url)) {
      setUrlError("A link starts with https://, http://, mailto: or a merge tag.");
      return;
    }
    setUrlError(null);
    setLinking(false);
    run("createLink", url.trim());
  }

  return (
    <div className="field be-rich">
      <span className="field-label" id={`${urlId}-label`}>
        {label}
      </span>
      {readOnly ? null : (
        <div className="be-toolbar" role="toolbar" aria-label={`${label} formatting`}>
          {COMMANDS.map((entry) => (
            <button
              key={entry.command}
              type="button"
              className="btn be-tool"
              aria-label={entry.label}
              onMouseDown={(event) => event.preventDefault()}
              onClick={() => run(entry.command)}
            >
              {entry.show}
            </button>
          ))}
          <button
            type="button"
            className="btn be-tool"
            aria-label="Add a link"
            onMouseDown={(event) => {
              event.preventDefault();
              remember();
            }}
            onClick={() => setLinking((open) => !open)}
          >
            Link
          </button>
          <button type="button" className="btn be-tool" aria-label="Remove link" onMouseDown={(event) => event.preventDefault()} onClick={() => run("unlink")}>
            Unlink
          </button>
        </div>
      )}
      {linking ? (
        <div className="row">
          <label className="field-label" htmlFor={urlId}>
            Link address
          </label>
          <input id={urlId} className="input" value={url} onChange={(event) => setUrl(event.target.value)} autoComplete="off" aria-invalid={urlError ? true : undefined} />
          <Button onClick={applyLink}>Apply link</Button>
          {urlError ? <span className="field-error">{urlError}</span> : null}
        </div>
      ) : null}
      <div
        ref={area}
        className="input be-editable"
        role="textbox"
        aria-multiline="true"
        aria-labelledby={`${urlId}-label`}
        aria-readonly={readOnly || undefined}
        contentEditable={!readOnly}
        suppressContentEditableWarning
        tabIndex={0}
        onFocus={() => onTarget(target())}
        onBlur={remember}
        onKeyUp={remember}
        onMouseUp={remember}
        onInput={emit}
      />
    </div>
  );
}
