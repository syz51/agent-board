// Test helpers for the live layer.

import type { Timers } from "./refresh.ts";

/** Timers run by hand. */
export function manualTimers() {
  let next = 1;
  const pending = new Map<number, { callback: () => void; ms: number }>();
  const timers: Timers = {
    setTimeout(callback, ms) {
      const handle = next++;
      pending.set(handle, { callback, ms });
      return handle;
    },
    clearTimeout(handle) {
      pending.delete(handle as number);
    },
  };
  return {
    timers,
    pending,
    runAll() {
      const due = [...pending.values()];
      pending.clear();
      for (const timer of due) timer.callback();
    },
  };
}
