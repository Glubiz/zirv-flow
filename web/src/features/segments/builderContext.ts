import { createContext, useContext } from "react";

import type { Catalog, EventTypes } from "./catalog";
import type { Node, Path } from "./ast";

/** An issue of the API placed on the node and field its pointer names. */
export interface PlacedIssue {
  pointer: string;
  code: string;
  path: Path;
  field: string;
}

export interface BuilderApi {
  readOnly: boolean;
  /** The segment being edited, which it cannot reference itself; null for a new one. */
  selfId: string | null;
  catalog: Catalog;
  eventTypes: EventTypes;
  issues: readonly PlacedIssue[];
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
