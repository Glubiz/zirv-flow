import { Button } from "../../ui";

/** The id the dock returns focus to when it closes. */
export const ASSISTANT_TOGGLE_ID = "assistant-toggle";

interface Props {
  open: boolean;
  onToggle: () => void;
}

/** The "Assistant" button of an editor's header or toolbar: it opens and closes the dock. */
export function AssistantToggle({ open, onToggle }: Props) {
  return (
    <Button id={ASSISTANT_TOGGLE_ID} aria-pressed={open} onClick={onToggle}>
      Assistant
    </Button>
  );
}
