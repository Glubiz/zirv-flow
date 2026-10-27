import { useEffect, useId, useRef, type KeyboardEvent, type ReactNode } from "react";
import { createPortal } from "react-dom";

import { Button } from "./Button";

const FOCUSABLE = [
  "a[href]",
  "button:not([disabled])",
  "input:not([disabled])",
  "select:not([disabled])",
  "textarea:not([disabled])",
  '[tabindex]:not([tabindex="-1"])',
].join(",");

interface SheetProps {
  /** The heading that names the sheet. */
  title: string;
  /** A line above the heading, such as the kind of thing the sheet is about. */
  eyebrow?: ReactNode;
  /** The accessible name of the close button. */
  closeLabel?: string;
  onClose: () => void;
  /** Move focus into the sheet when it opens. Off when the caller has put focus where it should stay. */
  autoFocus?: boolean;
  /**
   * Show it over the page as a modal: a backdrop that closes it on click, Tab kept inside, and the rest of
   * the page inert while it is open. Off by default, which keeps the non-modal slide-over.
   */
  modal?: boolean;
  children: ReactNode;
}

/**
 * A slide-over panel beside the page. Unlike a dialog it does not trap focus or block the page, so
 * the person can keep reading and using what is next to it. Escape closes it, and focus goes back to
 * where it was when the sheet opened.
 */
export function Sheet({ title, eyebrow, closeLabel = "Close", onClose, autoFocus = true, modal = false, children }: SheetProps) {
  const titleId = useId();
  const panel = useRef<HTMLDivElement>(null);
  const layer = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const opener = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    if (autoFocus) panel.current?.focus();
    const held: HTMLElement[] = [];
    if (modal) {
      for (const sibling of Array.from(document.body.children)) {
        if (sibling instanceof HTMLElement && sibling !== layer.current && !sibling.hasAttribute("inert")) {
          sibling.setAttribute("inert", "");
          held.push(sibling);
        }
      }
    }
    return () => {
      for (const sibling of held) sibling.removeAttribute("inert");
      if (opener?.isConnected && opener !== document.body) opener.focus();
    };
    // Only the moment it opens matters: later changes of the prop must not pull focus back in.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  function onKeyDown(event: KeyboardEvent<HTMLDivElement>) {
    if (event.key === "Escape") {
      event.stopPropagation();
      onClose();
      return;
    }
    if (!modal || event.key !== "Tab" || !panel.current) return;
    const items = Array.from(panel.current.querySelectorAll<HTMLElement>(FOCUSABLE));
    const head = items[0];
    const tail = items[items.length - 1];
    if (!head || !tail) {
      event.preventDefault();
      return;
    }
    if (event.shiftKey && (document.activeElement === head || document.activeElement === panel.current)) {
      event.preventDefault();
      tail.focus();
    } else if (!event.shiftKey && document.activeElement === tail) {
      event.preventDefault();
      head.focus();
    }
  }

  const sheet = (
    <div ref={panel} className={modal ? "sheet sheet-modal" : "sheet"} role="dialog" aria-modal={modal ? "true" : "false"} aria-labelledby={titleId} tabIndex={-1} onKeyDown={onKeyDown}>
      <div className="sheet-head">
        <div className="sheet-heading">
          {eyebrow ? <div className="sheet-eyebrow">{eyebrow}</div> : null}
          <h2 className="sheet-title" id={titleId}>
            {title}
          </h2>
        </div>
        <Button onClick={onClose}>{closeLabel}</Button>
      </div>
      <div className="sheet-body">{children}</div>
    </div>
  );
  if (!modal) return sheet;
  return createPortal(
    <div
      ref={layer}
      className="sheet-backdrop"
      onClick={(event) => {
        if (event.target === event.currentTarget) onClose();
      }}
    >
      {sheet}
    </div>,
    document.body,
  );
}
