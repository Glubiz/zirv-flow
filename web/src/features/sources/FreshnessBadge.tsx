import { Pill } from "../../ui";
import { freshnessLabel, freshnessTone } from "./labels";

/**
 * The freshness of a stream or source as a pill. The state is in the text, so it does not rest on
 * colour; no state (a disabled stream, a source without enabled streams) renders nothing.
 */
export function FreshnessBadge({ state }: { state: string | null | undefined }) {
  if (!state) return null;
  return <Pill tone={freshnessTone(state)}>{freshnessLabel(state)}</Pill>;
}
