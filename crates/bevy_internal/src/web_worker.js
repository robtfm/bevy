// The worker script of `bevy::web_worker`, and its page-side spawner. The same file runs on the
// page, where the wasm-bindgen glue imports it, and as the script of every worker it spawns.
//
// A worker is told, in its init message, the url of the glue and the exports to call, in order,
// once its wasm instance is up (over the shared module and memory the page passes). The page functions
// the engine calls (extern blocks marked `#[page_functions]`, listed by the glue exports that
// attribute generates) each become a function on the worker's global that posts the call to the
// page and resolves with the page function's result. Console output is sent to the page to
// print. When the last export returns (or its promise resolves) the worker posts
// `{ __bevy_ready: tag }`; if importing the glue or an export fails it posts
// `{ __bevy_ready: tag, error }` instead.

/**
 * Page side: spawns a worker and posts it its init message.
 * @param {{ glueUrl: string, calls: { entry: string, args: any[] }[], tag: string,
 *           module: WebAssembly.Module, memory: WebAssembly.Memory, stackSize?: number,
 *           transfer: Transferable[] }} init
 * @returns {Worker}
 */
export function spawnWorker(init) {
  const worker = new Worker(new URL(import.meta.url), { type: "module" });
  worker.addEventListener("message", async ({ data }) => {
    if (!data) return;
    if (data.__bevy_log) {
      console[data.__bevy_log](...data.args);
      return;
    }
    if (data.__bevy_call !== undefined) {
      const fn = globalThis[data.__bevy_call];
      let value, error;
      try {
        if (typeof fn !== "function") throw new Error(`${data.__bevy_call} is not defined on the page`);
        value = await fn(...data.args);
      } catch (err) {
        error = err?.message ?? String(err);
        console.error(`[bevy] ${data.tag} worker: ${data.__bevy_call} failed`, err);
      }
      worker.postMessage({ __bevy_result: data.__bevy_id, value, error });
    }
  });
  const { transfer = [], ...message } = init;
  worker.postMessage({ __bevy_init: message }, transfer);
  return worker;
}

// Worker side.
if (typeof WorkerGlobalScope !== "undefined" && self instanceof WorkerGlobalScope) {
  const pending = new Map();
  let nextId = 1;
  self.addEventListener("message", ({ data }) => {
    if (!data || data.__bevy_result === undefined) return;
    const call = pending.get(data.__bevy_result);
    if (!call) return;
    pending.delete(data.__bevy_result);
    if (data.error !== undefined) call.reject(new Error(String(data.error)));
    else call.resolve(data.value);
  });
  self.addEventListener("message", async ({ data }) => {
    const init = data?.__bevy_init;
    if (!init) return;
    const { glueUrl, calls, tag, module, memory, stackSize } = init;
    forwardConsole(tag);
    try {
      const glue = await import(glueUrl);
      await glue.default({ module_or_path: module, memory, thread_stack_size: stackSize });
      for (const key of Object.keys(glue)) {
        if (!key.startsWith("__bevy_page_functions_")) continue;
        for (const name of glue[key]()) {
          self[name] = (...args) =>
            new Promise((resolve, reject) => {
              const id = nextId++;
              pending.set(id, { resolve, reject });
              postMessage({ __bevy_call: name, __bevy_id: id, args, tag });
            });
        }
      }
      for (const { entry, args = [] } of calls) {
        const entryFn = glue[entry];
        if (typeof entryFn !== "function") throw new Error(`${entry} is not exported by ${glueUrl}`);
        await entryFn(...args);
      }
      postMessage({ __bevy_ready: tag });
    } catch (e) {
      console.error(`[bevy] ${tag} worker failed to start`, e);
      postMessage({ __bevy_ready: tag, error: String(e?.stack ?? e) });
    }
  });
}

// Sends this worker's console to the page, which prints it (and whose console hooks see it).
// It is not printed here as well: DevTools shows a worker's console alongside the page's, so
// every line would appear twice. Arguments are passed through as-is where they can be cloned, so
// styled (`%c`) lines render the same on the page.
function forwardConsole(tag) {
  for (const level of ["log", "info", "warn", "error", "debug"]) {
    const original = console[level].bind(console);
    console[level] = (...args) => {
      try {
        postMessage({ __bevy_log: level, tag, args: args.map(cloneable) });
      } catch (_) {
        // not cloneable: print it here instead
        original(...args);
      }
    };
  }
  self.addEventListener("error", (e) => console.error(`[${tag} worker] uncaught: ${e.message}`));
  self.addEventListener("unhandledrejection", (e) =>
    console.error(`[${tag} worker] unhandled rejection: ${e.reason?.stack ?? e.reason}`)
  );
}

function cloneable(value) {
  if (value instanceof Error) return value.stack || value.message;
  const t = typeof value;
  if (t === "string" || t === "number" || t === "boolean" || t === "bigint" || value == null) return value;
  try {
    return JSON.stringify(value);
  } catch (_) {
    return String(value);
  }
}
