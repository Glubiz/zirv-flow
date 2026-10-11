import { vi } from "vitest";

type Listener = (event: Event) => void;

/** A stand-in for the browser's EventSource that tests drive by hand. */
export class FakeEventSource {
  static instances: FakeEventSource[] = [];

  readonly url: string;
  readyState = 0;
  onopen: (() => void) | null = null;
  onerror: (() => void) | null = null;
  private readonly listeners = new Map<string, Listener[]>();

  constructor(url: string | URL) {
    this.url = String(url);
    FakeEventSource.instances.push(this);
  }

  addEventListener(type: string, listener: Listener): void {
    this.listeners.set(type, [...(this.listeners.get(type) ?? []), listener]);
  }

  close(): void {
    this.readyState = 2;
  }

  open(): void {
    this.readyState = 1;
    this.onopen?.();
  }

  /** Delivers a frame; `data` is JSON-encoded as the server does. */
  emit(type: string, data: unknown, lastEventId = ""): void {
    const event = new MessageEvent(type, { data: JSON.stringify(data), lastEventId });
    for (const listener of this.listeners.get(type) ?? []) {
      listener(event);
    }
  }

  /** A dropped connection the browser is retrying. */
  drop(): void {
    this.readyState = 0;
    this.onerror?.();
  }

  /** A connection the browser gave up on. */
  fail(): void {
    this.readyState = 2;
    this.onerror?.();
  }

  static install(): void {
    FakeEventSource.instances = [];
    vi.stubGlobal("EventSource", FakeEventSource);
  }

  static get latest(): FakeEventSource {
    const latest = FakeEventSource.instances.at(-1);
    if (latest === undefined) throw new Error("no EventSource was opened");
    return latest;
  }
}
