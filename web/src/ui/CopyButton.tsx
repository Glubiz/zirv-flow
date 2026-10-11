import { useState } from "react";

import { Button } from "./Button";

const MESSAGES = {
  idle: "",
  copied: "Copied to the clipboard",
  failed: "Could not copy. Select the text and copy it by hand.",
} as const;

export function CopyButton({ value, label }: { value: string; label: string }) {
  const [state, setState] = useState<keyof typeof MESSAGES>("idle");

  async function copy() {
    try {
      await navigator.clipboard.writeText(value);
      setState("copied");
    } catch {
      setState("failed");
    }
  }

  return (
    <>
      <Button onClick={() => void copy()} aria-label={label}>
        {state === "copied" ? "Copied" : "Copy"}
      </Button>
      <span className="visually-hidden" role="status">
        {MESSAGES[state]}
      </span>
    </>
  );
}
