import { useSnippets } from "../../snippets";
import { readTokens, useThemes, type ThemeListItem } from "../../themes";
import type { SnippetNames } from "./BlockCanvas";
import type { ThemeRef } from "./document";

/** The theme a document is under, as far as the editor can tell. */
export interface ThemeScope {
  ref: ThemeRef;
  /** The themes a document can follow. */
  choices: readonly ThemeListItem[];
  /** The theme the document names or follows, archived or not, when it can be found. */
  current: ThemeListItem | null;
  /** Whether a theme styles the document: a published named one, or the workspace default when one is set. */
  inEffect: boolean;
  /** What the dock tab and the footer call it. */
  name: string;
  /** Whether the theme the document names is archived. It still renders. */
  archived: boolean;
  /** Whether the theme in effect names a footer snippet: only then does the email end with a theme footer. */
  hasFooter: boolean;
}

/** No theme in play: a snippet, whose look comes from the email that holds it. */
export const NO_THEME: ThemeScope = { ref: "none", choices: [], current: null, inEffect: false, name: "", archived: false, hasFooter: false };

/** The scope of `ref` over the workspace's themes, archived ones included, since the server still renders those; the list loads in the background. */
export function useThemeScope(workspaceId: string, ref: ThemeRef): ThemeScope {
  const themes = useThemes(workspaceId, true);
  const all = themes.status === "ready" ? themes.data : [];
  const published = (theme: ThemeListItem) => theme.current_version !== null && theme.current_version !== undefined;
  const choices = all.filter((theme) => published(theme) && !theme.archived_at);
  const footerOf = (theme: ThemeListItem | null) => theme !== null && Boolean(readTokens(theme.tokens).footer_snippet);
  if (ref === "none") return { ref, choices, current: null, inEffect: false, name: "", archived: false, hasFooter: false };
  if (ref === "default") {
    const current = all.find((theme) => theme.is_default) ?? null;
    return { ref, choices, current, inEffect: current !== null, name: current?.name ?? "Default", archived: false, hasFooter: footerOf(current) };
  }
  const current = all.find((theme) => theme.id === ref.id) ?? null;
  const effective = current !== null && published(current);
  return { ref, choices, current, inEffect: effective, name: current?.name ?? "Theme", archived: Boolean(current?.archived_at), hasFooter: effective && footerOf(current) };
}

/** What the document can say about the snippets it holds, archived ones included: name, live version and whether it is archived, by id. */
export function useSnippetNames(workspaceId: string): SnippetNames {
  const snippets = useSnippets(workspaceId, true);
  const map = new Map<string, { name: string; version: number | null; archived: boolean }>();
  if (snippets.status === "ready") {
    for (const snippet of snippets.data) map.set(snippet.id, { name: snippet.name, version: snippet.current_version ?? null, archived: Boolean(snippet.archived_at) });
  }
  return map;
}
