import "@testing-library/jest-dom/vitest";

class ResizeObserverMock {
  callback: ResizeObserverCallback;

  constructor(callback: ResizeObserverCallback) {
    this.callback = callback;
  }

  observe(target: Element) {
    this.callback(
      [
        {
          target,
          contentRect: {
            width: (target as HTMLElement).clientWidth ?? 0,
            height: (target as HTMLElement).clientHeight ?? 0,
            x: 0,
            y: 0,
            top: 0,
            left: 0,
            bottom: 0,
            right: 0,
          },
        } as ResizeObserverEntry,
      ],
      this
    );
  }

  unobserve() {
    // noop
  }

  disconnect() {
    // noop
  }
}

Object.defineProperty(globalThis, "ResizeObserver", {
  writable: true,
  configurable: true,
  value: ResizeObserverMock,
});
