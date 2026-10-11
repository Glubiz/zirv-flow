import { useState } from "react";

import { describeError, type ErrorSummary } from "../../api";

/** Runs one request at a time and keeps whether it is running and how it failed. */
export function useAction() {
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);

  async function perform(task: () => Promise<unknown>): Promise<boolean> {
    setBusy(true);
    setFailure(null);
    try {
      await task();
      return true;
    } catch (error) {
      setFailure(describeError(error));
      return false;
    } finally {
      setBusy(false);
    }
  }

  return { busy, failure, perform };
}
