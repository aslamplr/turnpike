/// Test doubles for the two `@tauri-apps/api` modules the window imports.
///
/// `api.ts` and `stores.ts` are the *only* two files that import
/// `@tauri-apps` — every panel goes through one or the other. `vitest.config.ts`
/// aliases both specifiers here, so that pair of imports is the whole seam and
/// no production file carries a test branch.
///
/// This is injection by construction rather than a global `vi.mock`, matching
/// the Rust side's `MemoryStore` / `ScriptedPrompt`: a test registers exactly
/// the commands it expects, and a command with no fake **rejects loudly** rather
/// than resolving `undefined` — a typo in a command name is a broken test, not
/// a silently empty panel.

export interface InvokeCall {
  cmd: string;
  args: Record<string, unknown>;
}

type Fake = (args: Record<string, unknown>) => unknown;

const fakes = new Map<string, Fake>();
const once = new Map<string, Fake>();

/** Every `invoke` this process has seen, in order. Cleared by `reset`. */
export const calls: InvokeCall[] = [];

/** Event handlers `stores.attach` registered, by event name. */
const listeners = new Map<string, Array<(e: { payload: unknown }) => void>>();

/** Answer `cmd` with `fn`'s return value (or its rejection) for the rest of the test. */
export function fake(cmd: string, fn: Fake): void {
  fakes.set(cmd, fn);
}

/** Answer `cmd` with `fn` once, then fall through to whatever `fake` registered. */
export function fakeOnce(cmd: string, fn: Fake): void {
  once.set(cmd, fn);
}

/** The arguments of the last call to `cmd`, or `null` if it was never called. */
export function lastCall(cmd: string): Record<string, unknown> | null {
  for (let i = calls.length - 1; i >= 0; i--) {
    if (calls[i].cmd === cmd) return calls[i].args;
  }
  return null;
}

/** How many times `cmd` was invoked. */
export function callCount(cmd: string): number {
  return calls.filter((c) => c.cmd === cmd).length;
}

/** Deliver an event to whatever `stores.attach` subscribed to it. */
export function emit(event: string, payload: unknown): void {
  for (const handler of listeners.get(event) ?? []) handler({ payload });
}

/** Clear every fake, call log and listener. The setup file calls this per test. */
export function reset(): void {
  fakes.clear();
  once.clear();
  calls.length = 0;
  listeners.clear();
}

/// Stands in for `@tauri-apps/api/core`'s `invoke`.
export function invoke<T>(cmd: string, args: Record<string, unknown> = {}): Promise<T> {
  calls.push({ cmd, args });
  const fn = once.get(cmd) ?? fakes.get(cmd);
  if (fn === undefined) {
    return Promise.reject(new Error(`no fake registered for command "${cmd}"`));
  }
  if (once.has(cmd)) once.delete(cmd);
  try {
    // `Promise.resolve` is what keeps a synchronous throw asynchronous, like
    // the real `invoke` — otherwise a caller's `finally` could run before the
    // `catch` it belongs to, an ordering production never has.
    return Promise.resolve(fn(args) as T);
  } catch (e) {
    return Promise.reject(e);
  }
}

/// Stands in for `@tauri-apps/api/event`'s `listen`. Returns the unlisten
/// function the real one does, so `attach`'s `await` shape is exercised.
export function listen<T>(
  event: string,
  handler: (e: { payload: T }) => void,
): Promise<() => void> {
  const forEvent = listeners.get(event) ?? [];
  const h = handler as (e: { payload: unknown }) => void;
  forEvent.push(h);
  listeners.set(event, forEvent);
  return Promise.resolve(() => {
    listeners.set(
      event,
      (listeners.get(event) ?? []).filter((x) => x !== h),
    );
  });
}
