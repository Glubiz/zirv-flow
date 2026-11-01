import { createContext, useContext } from "react";

import type { Catalog, DimensionDef, EventTypes } from "./catalog";
import type { BuilderMode, Node, Path } from "./ast";

/** An issue of the API placed on the node and field its pointer names. */
export interface PlacedIssue {
  pointer: string;
  code: string;
  path: Path;
  field: string;
}

export interface BuilderApi {
  readOnly: boolean;
  /** A segment, or a consent policy rule, which also has the message and consent-for-this-message leaves. */
  mode: BuilderMode;
  /**
   * The workspace dimensions a messages condition can filter by; only a policy builder uses them.
   * Null when they could not be loaded: the filters are then not offered, as an empty catalog would.
   */
  dimensions: readonly DimensionDef[] | null;
  /** The segment being edited, which it cannot reference itself; null for a new one. */
  selfId: string | null;
  catalog: Catalog;
  eventTypes: EventTypes;
  issues: readonly PlacedIssue[];
  /** The JSON pointer of the condition the assistant is explaining, which is marked; null for none. */
  highlight: string | null;
  /** Replaces the node at `path`. */
  edit: (path: Path, next: Node) => void;
  add: (group: Path, child: Node) => void;
  remove: (group: Path, position: number) => void;
  changeKind: (path: Path, kind: string) => void;
}

export const BuilderContext = createContext<BuilderApi | null>(null);

export function useBuilder(): BuilderApi {
  const api = useContext(BuilderContext);
  if (api === null) {
    throw new Error("The builder controls are only available inside a segment builder");
  }
  return api;
}
