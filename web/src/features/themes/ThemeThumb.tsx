import { fontCss, type ThemeTokens } from "./model";

/** A card's picture of a theme, drawn from its tokens with HTML and CSS: nothing is rendered on the server. */
export function ThemeThumb({ tokens, name }: { tokens: ThemeTokens; name: string }) {
  return (
    <div className="th-thumb" role="img" aria-label={`Sample of ${name}`}>
      <div className="th-thumb-page" style={{ background: tokens.background, color: tokens.text, fontFamily: fontCss(tokens.font) }}>
        <div className="th-thumb-heading" style={{ color: tokens.primary }}>
          A heading in this theme
        </div>
        <div className="th-thumb-text">Body text sits on the background colour.</div>
        <span className="th-thumb-button" style={{ background: tokens.button, color: tokens.button_text, borderRadius: tokens.button_radius }}>
          Button
        </span>
      </div>
    </div>
  );
}
