import { useEffect, useId, useRef, useState, type KeyboardEvent, type ReactNode } from "react";

export interface MenuEntry {
  key: string;
  label: string;
  onSelect: () => void;
  checked?: boolean;
  /** Shown but not selectable; `reason` says why, under the label. */
  disabled?: boolean;
  reason?: string;
}

interface MenuButtonProps {
  label: string;
  buttonClassName: string;
  buttonContent: ReactNode;
  heading?: string;
  entries: MenuEntry[];
  emptyText?: string;
  /** Where the menu opens: under the button with its right edge on it (the default), or beside it, to its right, or under it with its left edge on it (`below-start`, for a trigger at the left edge). */
  placement?: "below-end" | "below-start" | "beside";
}

export function MenuButton({
  label,
  buttonClassName,
  buttonContent,
  heading,
  entries,
  emptyText,
  placement = "below-end",
}: MenuButtonProps) {
  const [open, setOpen] = useState(false);
  const button = useRef<HTMLButtonElement>(null);
  const wrap = useRef<HTMLDivElement>(null);
  const menu = useRef<HTMLDivElement>(null);
  const menuId = useId();

  function items(): HTMLElement[] {
    return Array.from(menu.current?.querySelectorAll<HTMLElement>("[role^='menuitem']") ?? []);
  }

  function closeToButton() {
    setOpen(false);
    button.current?.focus();
  }

  useEffect(() => {
    if (!open) {
      return;
    }
    const list = items();
    (list.find((item) => item.getAttribute("aria-checked") === "true") ?? list[0])?.focus();
    function onPointer(event: MouseEvent) {
      if (wrap.current && event.target instanceof Node && !wrap.current.contains(event.target)) {
        setOpen(false);
      }
    }
    document.addEventListener("mousedown", onPointer);
    return () => document.removeEventListener("mousedown", onPointer);
  }, [open]);

  function onButtonKey(event: KeyboardEvent<HTMLButtonElement>) {
    if (event.key === "ArrowDown" && !open) {
      event.preventDefault();
      setOpen(true);
    }
  }

  function onMenuKey(event: KeyboardEvent<HTMLDivElement>) {
    const list = items();
    const index = list.findIndex((item) => item === document.activeElement);
    const move = (target: number) => {
      event.preventDefault();
      list[(target + list.length) % list.length]?.focus();
    };
    switch (event.key) {
      case "ArrowDown":
        move(index + 1);
        break;
      case "ArrowUp":
        move(index - 1);
        break;
      case "Home":
        move(0);
        break;
      case "End":
        move(list.length - 1);
        break;
      case "Escape":
        event.preventDefault();
        event.stopPropagation();
        closeToButton();
        break;
      case "Tab":
        setOpen(false);
        break;
      default:
    }
  }

  return (
    <div className="menu-wrap" ref={wrap}>
      <button
        ref={button}
        type="button"
        className={buttonClassName}
        aria-label={label}
        aria-haspopup="menu"
        aria-expanded={open}
        aria-controls={open ? menuId : undefined}
        onClick={() => setOpen((value) => !value)}
        onKeyDown={onButtonKey}
      >
        {buttonContent}
      </button>
      {open ? (
        <div id={menuId} ref={menu} className={placement === "beside" ? "menu menu-beside" : placement === "below-start" ? "menu menu-start" : "menu"} role="menu" aria-label={label} onKeyDown={onMenuKey}>
          {heading ? (
            <div className="menu-heading" role="presentation">
              {heading}
            </div>
          ) : null}
          {entries.length === 0 && emptyText ? (
            <div className="menu-heading" role="presentation">
              {emptyText}
            </div>
          ) : null}
          {entries.map((entry) => (
            <button
              key={entry.key}
              type="button"
              className="menu-item"
              role={entry.checked === undefined ? "menuitem" : "menuitemradio"}
              aria-checked={entry.checked}
              aria-disabled={entry.disabled || undefined}
              tabIndex={-1}
              onClick={() => {
                if (entry.disabled) return;
                entry.onSelect();
                closeToButton();
              }}
            >
              {entry.checked === undefined ? null : (
                <span className="menu-check" aria-hidden="true">
                  {entry.checked ? "✓" : ""}
                </span>
              )}
              {entry.reason ? (
                <span className="menu-label">
                  {entry.label}
                  <span className="menu-reason">{entry.reason}</span>
                </span>
              ) : (
                entry.label
              )}
            </button>
          ))}
        </div>
      ) : null}
    </div>
  );
}
