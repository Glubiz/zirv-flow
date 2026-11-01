import { useEffect, useId, useRef, useState, type ClipboardEvent, type DragEvent } from "react";

import { Button } from "../../../ui";
import type { InsertTarget } from "./insertTarget";
import { editorNodes, linkAllowed, richTextFromEditor, richTextToEditor, UTM_ATTRIBUTE } from "./richText";

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
  const [noUtm, setNoUtm] = useState(false);
  const urlId = useId();

  useEffect(() => {
    const element = area.current;
    if (element && value !== emitted.current) {
      // Only escaped, allowlisted HTML goes into the DOM: a merge tag is text there, never markup.
      element.innerHTML = richTextToEditor(value);
      emitted.current = value;
    }
  }, [value]);

  function emit() {
    const element = area.current;
    if (!element) return;
    const cleaned = richTextFromEditor(element);
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

  function run(command: string, argument?: string, after?: () => void) {
    area.current?.focus();
    restore();
    // The browser's own editing command: the one way to edit rich text in place without a library.
    const exec = (document as unknown as { execCommand?: (command: string, ui: boolean, value?: string) => boolean }).execCommand;
    if (typeof exec === "function") exec.call(document, command, false, argument);
    after?.();
    emit();
  }

  /** Puts nodes at the caret (or at the end), in place of any selection. */
  function insertNodes(nodes: Node[]) {
    const element = area.current;
    if (!element) return;
    element.focus();
    restore();
    const selection = window.getSelection();
    const fragment = document.createDocumentFragment();
    fragment.append(...nodes);
    const last = fragment.lastChild;
    if (selection && selection.rangeCount > 0 && element.contains(selection.anchorNode)) {
      const current = selection.getRangeAt(0);
      current.deleteContents();
      current.insertNode(fragment);
      if (last) {
        current.setStartAfter(last);
        current.collapse(true);
      }
    } else {
      element.append(fragment);
    }
    remember();
    emit();
  }

  /** Pasted or dropped content never lands as it came: HTML is cleaned first, plain text becomes text nodes. */
  function take(data: DataTransfer | null) {
    if (readOnly || !data) return;
    const html = data.getData("text/html");
    if (html !== "") {
      insertNodes(editorNodes(html));
      return;
    }
    const nodes: Node[] = [];
    data
      .getData("text/plain")
      .split("\n")
      .forEach((line, index) => {
        if (index > 0) nodes.push(document.createElement("br"));
        if (line !== "") nodes.push(document.createTextNode(line));
      });
    insertNodes(nodes);
  }

  function onPaste(event: ClipboardEvent) {
    event.preventDefault();
    take(event.clipboardData);
  }

  function onDrop(event: DragEvent) {
    event.preventDefault();
    take(event.dataTransfer);
  }

  function target(): InsertTarget {
    return {
      label,
      insert: (tag) => {
        if (area.current?.isConnected) insertNodes([document.createTextNode(tag)]);
      },
    };
  }

  /** Sets or clears the opt-out from UTM tagging on the links the selection touches. */
  function markLinks(off: boolean) {
    const selection = window.getSelection();
    if (!area.current || !selection || selection.rangeCount === 0) return;
    for (const link of Array.from(area.current.querySelectorAll("a"))) {
      if (!selection.getRangeAt(0).intersectsNode(link)) continue;
      if (off) link.setAttribute(UTM_ATTRIBUTE, "off");
      else link.removeAttribute(UTM_ATTRIBUTE);
    }
  }

  /** Opens the dialog with the opt-out of the first link under the remembered selection. */
  function openDialog() {
    const selected = range.current;
    const link = selected && area.current ? Array.from(area.current.querySelectorAll("a")).find((a) => selected.intersectsNode(a)) : undefined;
    setNoUtm(link?.getAttribute(UTM_ATTRIBUTE) === "off");
    setLinking(true);
  }

  function closeDialog() {
    setLinking(false);
    setNoUtm(false);
  }

  function applyLink() {
    if (!linkAllowed(url)) {
      setUrlError("A link starts with https://, http://, mailto: or a merge tag.");
      return;
    }
    setUrlError(null);
    closeDialog();
    run("createLink", url.trim(), () => markLinks(noUtm));
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
            onClick={() => (linking ? closeDialog() : openDialog())}
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
          <label className="check">
            <input type="checkbox" checked={noUtm} onChange={(event) => setNoUtm(event.target.checked)} />
            <span>Do not add UTM parameters</span>
          </label>
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
        onPaste={onPaste}
        onDrop={onDrop}
      />
    </div>
  );
}
