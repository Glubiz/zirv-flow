import type { ReactNode } from "react";

import type { NodeType } from "./model";

interface IconProps {
  size?: number;
  children: ReactNode;
}

function Icon({ size = 14, children }: IconProps) {
  return (
    <svg
      aria-hidden="true"
      focusable="false"
      width={size}
      height={size}
      viewBox="0 0 16 16"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.6"
      strokeLinecap="round"
      strokeLinejoin="round"
    >
      {children}
    </svg>
  );
}

export type IconName =
  | NodeType
  | "trigger"
  | "plus"
  | "fit"
  | "zoom-in"
  | "zoom-out"
  | "undo"
  | "assistant";

/** The small drawings of the canvas: one per kind of step, and the tools of the rail. */
export function FlowIcon({ name, size }: { name: IconName; size?: number }) {
  switch (name) {
    case "trigger":
      return (
        <Icon size={size}>
          <path d="M9 1.5L3.5 9h4L7 14.5 12.5 7h-4z" />
        </Icon>
      );
    case "wait.duration":
      return (
        <Icon size={size}>
          <circle cx="8" cy="8" r="6" />
          <path d="M8 4.5V8l2.5 1.5" />
        </Icon>
      );
    case "wait.until_time":
      return (
        <Icon size={size}>
          <rect x="2.5" y="3.5" width="11" height="10" rx="1.5" />
          <path d="M2.5 6.5h11M5.5 2v3M10.5 2v3" />
        </Icon>
      );
    case "wait.until_event":
      return (
        <Icon size={size}>
          <path d="M4 6.5a4 4 0 0 1 8 0v2.5l1.5 2h-11L4 9z" />
          <path d="M6.5 13a1.5 1.5 0 0 0 3 0" />
        </Icon>
      );
    case "branch":
      return (
        <Icon size={size}>
          <circle cx="4" cy="3.5" r="1.5" />
          <circle cx="4" cy="12.5" r="1.5" />
          <circle cx="12" cy="6" r="1.5" />
          <path d="M4 5v6M4 9.5c0-2 2-3.5 6.5-3.5" />
        </Icon>
      );
    case "split.random":
      return (
        <Icon size={size}>
          <path d="M2.5 8H6l3-4.5h4M6 8l3 4.5h4" />
          <path d="M11.5 2l2 1.5-2 1.5M11.5 11l2 1.5-2 1.5" />
        </Icon>
      );
    case "action.webhook":
      return (
        <Icon size={size}>
          <path d="M9 2.5h4.5V7M13.5 2.5l-6 6" />
          <path d="M11.5 9.5v4h-9v-9h4" />
        </Icon>
      );
    case "action.send":
      return (
        <Icon size={size}>
          <rect x="2" y="3.5" width="12" height="9" rx="1.5" />
          <path d="M2.5 5l5.5 4 5.5-4" />
        </Icon>
      );
    case "action.sync":
      return (
        <Icon size={size}>
          <path d="M13 6.5A5 5 0 0 0 3.7 5M3 2.5V5h2.5" />
          <path d="M3 9.5A5 5 0 0 0 12.3 11M13 13.5V11h-2.5" />
        </Icon>
      );
    case "action.update_trait":
      return (
        <Icon size={size}>
          <path d="M10.5 2.5l3 3L6 13H3v-3z" />
        </Icon>
      );
    case "action.list":
      return (
        <Icon size={size}>
          <path d="M6 4h7.5M6 8h7.5M6 12h7.5M2.5 4h.5M2.5 8h.5M2.5 12h.5" />
        </Icon>
      );
    case "goal":
      return (
        <Icon size={size}>
          <path d="M3.5 14V2.5M3.5 3h8l-1.5 2.5L11.5 8h-8" />
        </Icon>
      );
    case "exit":
      return (
        <Icon size={size}>
          <path d="M6 2.5H3v11h3" />
          <path d="M9 5l3 3-3 3M12 8H6" />
        </Icon>
      );
    case "plus":
      return (
        <Icon size={size}>
          <path d="M8 3v10M3 8h10" />
        </Icon>
      );
    case "fit":
      return (
        <Icon size={size}>
          <path d="M2.5 6V2.5H6M10 2.5h3.5V6M13.5 10v3.5H10M6 13.5H2.5V10" />
        </Icon>
      );
    case "zoom-in":
      return (
        <Icon size={size}>
          <circle cx="7" cy="7" r="4.5" />
          <path d="M10.5 10.5l3 3M5 7h4M7 5v4" />
        </Icon>
      );
    case "zoom-out":
      return (
        <Icon size={size}>
          <circle cx="7" cy="7" r="4.5" />
          <path d="M10.5 10.5l3 3M5 7h4" />
        </Icon>
      );
    case "assistant":
      return (
        <Icon size={size}>
          <path d="M8 1.5l1.4 3.6 3.6 1.4-3.6 1.4L8 11.5 6.6 7.9 3 6.5l3.6-1.4z" />
          <path d="M12.5 10.5l.6 1.4 1.4.6-1.4.6-.6 1.4-.6-1.4-1.4-.6 1.4-.6z" />
        </Icon>
      );
    case "undo":
      return (
        <Icon size={size}>
          <path d="M5.5 3L2.5 6l3 3" />
          <path d="M2.5 6H10a3.5 3.5 0 0 1 0 7H7" />
        </Icon>
      );
  }
}
