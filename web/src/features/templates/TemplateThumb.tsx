import type { CSSProperties } from "react";

import type { TemplateListItem } from "./library";

/** The tokens of a theme that a card's picture uses. */
export interface ThumbTokens {
  background: string;
  primary: string;
  text: string;
  button: string;
}

const NEUTRAL: ThumbTokens = { background: "#ffffff", primary: "#48505c", text: "#bdc3ca", button: "#626a73" };

function colour(value: unknown, fallback: string): string {
  return typeof value === "string" && /^#[0-9a-f]{3,8}$/i.test(value) ? value : fallback;
}

/** The picture colours for stored tokens; a token the theme lacks falls back to a neutral grey. */
export function thumbTokens(stored: object | null | undefined): ThumbTokens {
  if (!stored) return NEUTRAL;
  const tokens = stored as Record<string, unknown>;
  return {
    background: colour(tokens["background"], NEUTRAL.background),
    primary: colour(tokens["primary"], NEUTRAL.primary),
    text: colour(tokens["text"], NEUTRAL.text),
    button: colour(tokens["button"], NEUTRAL.button),
  };
}

function bar(width: string, background: string, height = 4, opacity = 1): CSSProperties {
  return { width, height, background, opacity, borderRadius: 2 };
}

/**
 * A card's picture of a template, drawn with HTML and CSS from the colours of the theme it follows:
 * nothing is rendered on the server. A webhook, or a template with no theme, is drawn in grey.
 */
export function TemplateThumb({ item, tokens }: { item: TemplateListItem; tokens: ThumbTokens }) {
  if (item.channel === "webhook") {
    return (
      <div className="tl-thumb tl-thumb-hook" aria-hidden="true">
        <div className="tl-thumb-code">
          <span style={bar("46%", "#27e3fd", 5)} />
          <span style={bar("72%", "#8fa3b8", 5)} />
          <span style={bar("58%", "#8fa3b8", 5)} />
          <span style={bar("34%", "#27e3fd", 5)} />
        </div>
      </div>
    );
  }
  return (
    <div className="tl-thumb" aria-hidden="true">
      <div className="tl-thumb-paper" style={{ background: tokens.background }}>
        <span style={bar("38%", tokens.primary, 5)} />
        <span style={bar("82%", tokens.primary, 8)} />
        <span style={bar("100%", tokens.text, 4, 0.35)} />
        <span style={bar("92%", tokens.text, 4, 0.35)} />
        <span style={bar("70%", tokens.text, 4, 0.35)} />
        <span style={{ ...bar("42%", tokens.button, 13), alignSelf: "center", marginTop: 4, borderRadius: 3 }} />
      </div>
    </div>
  );
}
