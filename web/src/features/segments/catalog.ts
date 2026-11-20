import { createContext, useContext, useEffect, useMemo, useRef, useState } from "react";

import { client, unwrap, useResource, type components, type Resource } from "../../api";
import { ENGAGEMENT_PROPERTIES } from "../../ui";
import { asValueType, type ValueType } from "./operators";

export type TraitDef = components["schemas"]["TraitBody"];
export type EventDef = components["schemas"]["EventSchemaBody"];
export type ListDef = components["schemas"]["ListBody"];
export type SegmentRow = components["schemas"]["SegmentBody"];
export type TopicDef = components["schemas"]["TopicBody"];
export type DimensionDef = components["schemas"]["DimensionBody"];

export interface Catalog {
  traits: readonly TraitDef[];
  events: readonly EventDef[];
  lists: readonly ListDef[];
  segments: readonly SegmentRow[];
  topics: readonly TopicDef[];
}

export type EventTypes = ReadonlyMap<string, ReadonlyMap<string, ValueType>>;

export type CatalogState =
  | { status: "loading" }
  | { status: "error"; error: unknown; reload: () => void }
  | { status: "ready"; catalog: Catalog };

function first(resources: readonly Resource<unknown>[]): Resource<unknown> | null {
  return resources.find((resource) => resource.status === "error") ?? null;
}

/** The workspace's lists. */
export function useLists(workspaceId: string): Resource<ListDef[]> {
  return useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/lists", { params: { path: { ws: workspaceId } } })),
    [workspaceId],
  );
}

/** The workspace's segments. */
export function useSegmentRows(workspaceId: string): Resource<SegmentRow[]> {
  return useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/segments", { params: { path: { ws: workspaceId } } })),
    [workspaceId],
  );
}

/** What the builder offers to pick from: the workspace's fields, events, lists, segments and topics. */
export function useCatalog(workspaceId: string): CatalogState {
  const traits = useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/traits", { params: { path: { ws: workspaceId } } })),
    [workspaceId],
  );
  const events = useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/event-schemas", { params: { path: { ws: workspaceId } } })),
    [workspaceId],
  );
  const lists = useLists(workspaceId);
  const segments = useSegmentRows(workspaceId);
  const topics = useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/topics", { params: { path: { ws: workspaceId } } })),
    [workspaceId],
  );

  const failed = first([traits, events, lists, segments, topics]);
  if (failed?.status === "error") {
    return {
      status: "error",
      error: failed.error,
      reload: () => {
        traits.reload();
        events.reload();
        lists.reload();
        segments.reload();
        topics.reload();
      },
    };
  }
  if (
    traits.status !== "ready" ||
    events.status !== "ready" ||
    lists.status !== "ready" ||
    segments.status !== "ready" ||
    topics.status !== "ready"
  ) {
    return { status: "loading" };
  }
  return {
    status: "ready",
    catalog: {
      traits: traits.data,
      events: events.data,
      lists: lists.data,
      segments: segments.data,
      topics: topics.data,
    },
  };
}

/** The type of each trait by key. */
export function traitTypes(traits: readonly TraitDef[]): ReadonlyMap<string, ValueType> {
  const types = new Map<string, ValueType>();
  for (const trait of traits) {
    const type = asValueType(trait.type);
    if (type !== null) types.set(trait.key, type);
  }
  return types;
}

/** A failed event-schema fetch is tried again this many times, after 1, 2 and 4 seconds. */
const MAX_RETRIES = 3;
const RETRY_BASE_MS = 1000;

/**
 * The property types of each named event, loaded as the names appear. An event that cannot be
 * loaded is tried again a few times, then left out, so a definition that uses it is not complete.
 */
export function useEventTypes(workspaceId: string, names: readonly string[]): EventTypes {
  const [types, setTypes] = useState<EventTypes>(new Map());
  const [retry, setRetry] = useState(0);
  const asked = useRef(new Set<string>());
  const failures = useRef(new Map<string, number>());
  // Names that used their retries stay asked-for: another name's retry tick must not ask them again.
  const exhausted = useRef(new Set<string>());
  const timers = useRef(new Set<ReturnType<typeof setTimeout>>());
  const mounted = useRef(true);
  const wanted = names.filter((name) => name !== "").join("\n");

  useEffect(() => {
    mounted.current = true;
    const pending = timers.current;
    return () => {
      mounted.current = false;
      for (const timer of pending) clearTimeout(timer);
      pending.clear();
    };
  }, []);
  const scope = useRef(workspaceId);
  // Another workspace starts clean: no held names, no counts, no pending retry of the old one.
  useEffect(() => {
    scope.current = workspaceId;
    asked.current.clear();
    failures.current.clear();
    exhausted.current.clear();
    for (const timer of timers.current) clearTimeout(timer);
    timers.current.clear();
  }, [workspaceId]);
  // Asking for other events starts the count again.
  useEffect(() => {
    failures.current.clear();
    exhausted.current.clear();
  }, [wanted]);

  useEffect(() => {
    for (const name of wanted === "" ? [] : wanted.split("\n")) {
      if (asked.current.has(name) || exhausted.current.has(name)) continue;
      asked.current.add(name);
      unwrap(
        client.GET("/api/v1/workspaces/{ws}/event-schemas/{name}", {
          params: { path: { ws: workspaceId, name } },
        }),
      ).then(
        (detail) => {
          if (scope.current !== workspaceId) return;
          const properties = detail.versions[0]?.properties ?? detail.draft ?? [];
          const map = new Map<string, ValueType>();
          for (const property of properties) {
            const type = asValueType(property.type);
            if (type !== null) map.set(property.name, type);
          }
          failures.current.delete(name);
          setTypes((current) => new Map(current).set(name, map));
        },
        () => {
          if (scope.current !== workspaceId) return;
          const count = (failures.current.get(name) ?? 0) + 1;
          failures.current.set(name, count);
          if (!mounted.current || count > MAX_RETRIES) {
            asked.current.delete(name);
            exhausted.current.add(name);
            return;
          }
          // Only this name is freed when its own backoff ends, so another name's timer cannot retry it early.
          const timer = setTimeout(() => {
            timers.current.delete(timer);
            asked.current.delete(name);
            setRetry((current) => current + 1);
          }, RETRY_BASE_MS * 2 ** (count - 1));
          timers.current.add(timer);
        },
      );
    }
  }, [workspaceId, wanted, retry]);

  return useMemo(() => {
    const merged = new Map(types);
    for (const [event, properties] of Object.entries(ENGAGEMENT_PROPERTIES)) {
      if (!wanted.split("\n").includes(event)) continue;
      const known = new Map(merged.get(event));
      for (const [name, type] of Object.entries(properties)) if (!known.has(name)) known.set(name, type);
      merged.set(event, known);
    }
    return merged;
  }, [types, wanted]);
}

export const CatalogContext = createContext<Catalog | null>(null);

export function useCatalogContext(): Catalog {
  const catalog = useContext(CatalogContext);
  if (catalog === null) {
    throw new Error("The catalog is only available inside a segment builder");
  }
  return catalog;
}
