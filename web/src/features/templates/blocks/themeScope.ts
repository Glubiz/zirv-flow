import { useSnippets } from "../../snippets";
import { useThemes, type ThemeListItem } from "../../themes";
import type { ThemeRef } from "./document";

/** The theme a document is under, as far as the editor can tell. */
export interface ThemeScope {
  ref: ThemeRef;
  /** The themes a document can follow. */
  choices: readonly ThemeListItem[];
  /** The theme the document looks like, when there is one. */
  current: ThemeListItem | null;
  /** Whether a theme styles the document: a named one, or the workspace default when one is set. */
  inEffect: boolean;
  /** What the dock tab and the footer call it. */
  name: string;
}

/** No theme in play: a snippet, whose look comes from the email that holds it. */
export const NO_THEME: ThemeScope = { ref: "none", choices: [], current: null, inEffect: false, name: "" };

/** The scope of `ref` over the workspace's themes; the list loads in the background. */
export function useThemeScope(workspaceId: string, ref: ThemeRef): ThemeScope {
  const themes = useThemes(workspaceId);
  const all = themes.status === "ready" ? themes.data : [];
  const choices = all.filter((theme) => theme.current_version !== null && theme.current_version !== undefined);
  if (ref === "none") return { ref, choices, current: null, inEffect: false, name: "" };
  if (ref === "default") {
    const current = all.find((theme) => theme.is_default) ?? null;
    return { ref, choices, current, inEffect: current !== null, name: current?.name ?? "Default" };
  }
  const current = all.find((theme) => theme.id === ref.id) ?? null;
  return { ref, choices, current, inEffect: true, name: current?.name ?? "Theme" };
}

/** What the document can say about the snippets it holds: name and live version by id. */
export function useSnippetNames(workspaceId: string): ReadonlyMap<string, { name: string; version: number | null }> {
  const snippets = useSnippets(workspaceId);
  const map = new Map<string, { name: string; version: number | null }>();
  if (snippets.status === "ready") {
    for (const snippet of snippets.data) map.set(snippet.id, { name: snippet.name, version: snippet.current_version ?? null });
  }
  return map;
}
