import { describeError } from "../../api";
import { Button, InlineError } from "../../ui";
import type { Paged } from "./usePaged";

/** The footer of a paged list: shown while another page exists. */
export function ShowMore<T>({ paged }: { paged: Paged<T> }) {
  if (paged.resource.status !== "ready" || paged.next === null) return null;
  return (
    <div className="panel-footer">
      {paged.moreError ? <InlineError {...describeError(paged.moreError)} /> : null}
      <Button onClick={paged.loadMore} disabled={paged.loadingMore}>
        {paged.loadingMore ? "Loading" : "Show more"}
      </Button>
    </div>
  );
}
