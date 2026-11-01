import { useId, type KeyboardEvent, type ReactNode } from "react";

import { Button, TabNav, useTabParam, type TabEntry } from "../../ui";
import { ASSISTANT_TOGGLE_ID } from "./AssistantToggle";

interface Props {
  /** What the editor edits, for the explain tab: "flow" gives "Explain this flow". */
  thing: string;
  /** The "Draft a change" tab; absent in a read-only editor, which can only explain. */
  draft?: ReactNode;
  /** The "Explain this ..." tab; absent where there is nothing to explain. */
  explain?: ReactNode;
  /** Hides the dock without closing it, such as while another panel of the editor takes its place. */
  hidden?: boolean;
  onClose: () => void;
}

/**
 * The assistant of every editor: one aside with the same header, tabs and footer. Both tabs stay
 * mounted so a draft in progress survives a look at the explanation. Escape closes it and focus goes
 * back to the toggle that opened it.
 */
export function AssistantDock({ thing, draft, explain, hidden, onClose }: Props) {
  const heading = useId();
  const tabs: TabEntry[] = [
    ...(draft ? [{ id: "draft", label: "Draft a change" }] : []),
    ...(explain ? [{ id: "explain", label: `Explain this ${thing}` }] : []),
  ];
  const { active } = useTabParam(tabs, "dock");

  function close() {
    onClose();
    document.getElementById(ASSISTANT_TOGGLE_ID)?.focus();
  }

  function onKeyDown(event: KeyboardEvent<HTMLElement>) {
    // A dialog opened from inside the dock bubbles through React but sits elsewhere in the page.
    if (event.key !== "Escape" || event.defaultPrevented || !event.currentTarget.contains(event.target as Node)) return;
    event.stopPropagation();
    close();
  }

  return (
    <aside className="dock" aria-labelledby={heading} hidden={hidden} onKeyDown={onKeyDown}>
      <div className="dock-head">
        <h2 id={heading}>Assistant</h2>
        <Button variant="quiet" aria-label="Close the assistant" onClick={close}>
          ×
        </Button>
      </div>
      <TabNav label="Assistant" tabs={tabs} param="dock" />
      {draft ? (
        <div className="dock-pane" hidden={active !== "draft"}>
          {draft}
        </div>
      ) : null}
      {explain ? (
        <div className="dock-pane" hidden={active !== "explain"}>
          {explain}
        </div>
      ) : null}
    </aside>
  );
}
