import { vi } from "vitest";

/**
 * What @xyflow/react needs from a browser and jsdom does not have, as its testing guide lists:
 * a ResizeObserver, DOMMatrixReadOnly, element sizes and SVG measuring. Call it in `beforeEach`;
 * the shared setup unstubs the globals after each test.
 */
export function stubReactFlow() {
  class ResizeObserverStub {
    callback: ResizeObserverCallback;
    constructor(callback: ResizeObserverCallback) {
      this.callback = callback;
    }
    observe(target: Element) {
      const entry = { target, contentRect: { width: 1000, height: 800 } } as ResizeObserverEntry;
      this.callback([entry], this);
    }
    unobserve() {}
    disconnect() {}
  }
  class DOMMatrixReadOnlyStub {
    m22: number;
    constructor(transform?: string) {
      const scale = transform?.match(/scale\(([\d.]+)\)/)?.[1];
      this.m22 = scale === undefined ? 1 : Number(scale);
    }
  }
  vi.stubGlobal("ResizeObserver", ResizeObserverStub);
  vi.stubGlobal("DOMMatrixReadOnly", DOMMatrixReadOnlyStub);
  vi.stubGlobal("matchMedia", vi.fn().mockImplementation((media: string) => ({
    matches: false, media, onchange: null,
    addListener: vi.fn(), removeListener: vi.fn(),
    addEventListener: vi.fn(), removeEventListener: vi.fn(), dispatchEvent: vi.fn(),
  })));
  Object.defineProperties(HTMLElement.prototype, {
    offsetHeight: {
      configurable: true,
      get() {
        return Number.parseFloat((this as HTMLElement).style.height) || 1;
      },
    },
    offsetWidth: {
      configurable: true,
      get() {
        return Number.parseFloat((this as HTMLElement).style.width) || 1;
      },
    },
  });
  Object.defineProperty(SVGElement.prototype, "getBBox", {
    configurable: true,
    value: () => ({ x: 0, y: 0, width: 0, height: 0 }),
  });
  Object.defineProperty(Element.prototype, "scrollIntoView", { configurable: true, value: () => undefined });
}
