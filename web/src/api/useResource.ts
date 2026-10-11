import { useCallback, useEffect, useRef, useState } from "react";

export type Resource<T> =
  | { status: "loading"; reload: () => void }
  | { status: "error"; error: unknown; reload: () => void }
  | { status: "ready"; data: T; reload: () => void; refreshing: boolean };

type Settled<T> =
  | { status: "error"; error: unknown }
  | { status: "ready"; data: T };

interface Slot<T> {
  inputs: string;
  tick: number;
  result: Settled<T>;
}

export function useResource<T>(load: () => Promise<T>, inputs: readonly (string | number | boolean | null)[]): Resource<T> {
  const [slot, setSlot] = useState<Slot<T> | null>(null);
  const [tick, setTick] = useState(0);
  const loader = useRef(load);
  const key = inputs.map(String).join("|");

  useEffect(() => {
    loader.current = load;
  });

  useEffect(() => {
    let live = true;
    loader.current().then(
      (data) => {
        if (live) setSlot({ inputs: key, tick, result: { status: "ready", data } });
      },
      (error: unknown) => {
        if (live) setSlot({ inputs: key, tick, result: { status: "error", error } });
      },
    );
    return () => {
      live = false;
    };
  }, [key, tick]);

  const reload = useCallback(() => setTick((value) => value + 1), []);

  if (slot?.inputs !== key) {
    return { status: "loading", reload };
  }
  if (slot.result.status === "ready") {
    return { status: "ready", data: slot.result.data, reload, refreshing: slot.tick !== tick };
  }
  return slot.tick === tick ? { status: "error", error: slot.result.error, reload } : { status: "loading", reload };
}
