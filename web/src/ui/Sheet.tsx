import { useEffect, useId, useRef, type KeyboardEvent, type ReactNode } from "react";

import { Button } from "./Button";

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
  children: ReactNode;
}

/**
 * A slide-over panel beside the page. Unlike a dialog it does not trap focus or block the page, so
 * the person can keep reading and using what is next to it. Escape closes it, and focus goes back to
 * where it was when the sheet opened.
 */
export function Sheet({ title, eyebrow, closeLabel = "Close", onClose, autoFocus = true, children }: SheetProps) {
  const titleId = useId();
  const panel = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const opener = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    if (autoFocus) panel.current?.focus();
    return () => {
      if (opener?.isConnected && opener !== document.body) opener.focus();
    };
    // Only the moment it opens matters: later changes of the prop must not pull focus back in.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  function onKeyDown(event: KeyboardEvent<HTMLDivElement>) {
    if (event.key === "Escape") {
      event.stopPropagation();
      onClose();
    }
  }

  return (
    <div ref={panel} className="sheet" role="dialog" aria-modal="false" aria-labelledby={titleId} tabIndex={-1} onKeyDown={onKeyDown}>
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
}
