import { useState } from "react";

import type { ErrorSummary } from "../../api";
import { ErrorState, LoadingState, previewDocument } from "../../ui";
import type { ThemePreview } from "./model";

export type PreviewState =
  | { status: "loading" }
  | { status: "ready"; data: ThemePreview }
  | { status: "failed"; error: ErrorSummary };

type Width = "desktop" | "mobile";

interface Props {
  state: PreviewState;
  refreshing: boolean;
  onRetry: () => void;
  /** Names the frame; the older-version view says which version it shows. */
  frameTitle?: string;
}

/** The server-rendered sample. The HTML goes into an iframe with every sandbox permission off; the SPA never renders it. */
export function ThemePreviewPane({ state, refreshing, onRetry, frameTitle = "Preview of the theme" }: Props) {
  const [width, setWidth] = useState<Width>("desktop");
  return (
    <section className="tp-preview" aria-label="Preview" aria-busy={refreshing || undefined}>
      <div className="th-preview-head">
        <h2 className="panel-title">Preview</h2>
        <div className="seg-tabs" role="group" aria-label="Preview width">
          {(["desktop", "mobile"] as const).map((entry) => (
            <button key={entry} type="button" className={width === entry ? "seg seg-on" : "seg"} aria-pressed={width === entry} onClick={() => setWidth(entry)}>
              {entry === "desktop" ? "Desktop" : "Mobile"}
            </button>
          ))}
        </div>
      </div>
      {refreshing ? (
        <p className="faint" role="status">
          Updating the preview
        </p>
      ) : null}
      {state.status === "loading" ? <LoadingState label="Rendering the preview" /> : null}
      {state.status === "failed" ? (
        <ErrorState title="The preview could not be rendered" detail={state.error.detail || state.error.title} traceId={state.error.traceId} onRetry={onRetry} />
      ) : null}
      {state.status === "ready" && !state.data.rendered ? (
        <p className="notice" role="status">
          Nothing was rendered because the theme has blocking issues. Fix them and the preview returns.
        </p>
      ) : null}
      {state.status === "ready" && state.data.rendered ? (
        <div className={width === "mobile" ? "tp-frame tp-frame-mobile" : "tp-frame"}>
          <iframe title={frameTitle} sandbox="" srcDoc={previewDocument(state.data.html ?? "")} className="tp-iframe" />
        </div>
      ) : null}
    </section>
  );
}
