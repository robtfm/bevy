//! Running the engine on web workers, started with one call from the page.
//!
//! [`start`] takes the page's canvas and spawns the workers the engine runs on: the render
//! worker, which owns the canvas as an `OffscreenCanvas`, the wgpu device and the render world
//! ([`bevy_render::web_worker`]); the compute workers, which join the
//! [`ComputeTaskPool`](bevy_tasks::ComputeTaskPool); and the engine worker, which runs the app
//! world and winit's event loop, with input forwarded from the page. Every worker runs the same
//! script, shipped with this module, which instantiates the wasm module over the page's shared
//! memory and then calls this module's entry for its role. The app supplies one export, the
//! engine worker's, which builds the app with
//! [`RenderCreation::WebWorker`](bevy_render::settings::RenderCreation::WebWorker) and a compute
//! pool of [`compute_threads`] threads and runs it; `App::run` then has `finish` and `cleanup`
//! executed on the render worker before the app runs. Optionally the app supplies a render
//! worker setup export, called there before the render world is created: the place to patch
//! WebGPU before the device exists, or to create the wgpu resources itself and
//! [provide](bevy_render::web_worker::provide_render_resources) them.
//!
//! Functions the engine reaches on the page (JS hooks looked up on `self`) are declared in
//! `#[wasm_bindgen(js_namespace = self)]` extern blocks marked [`page_functions`]; each worker
//! gets a same-named function that relays the call to the page's global and resolves with the
//! result; a function returning a value must be `async` or return a `js_sys::Promise`, which the
//! attribute checks. Worker console output is mirrored to the page. Once a worker's last entry
//! has returned it posts `{ __bevy_ready: tag }` to the page (compute workers never return), or
//! `{ __bevy_ready: tag, error }` if an entry failed. [`spawn_worker`] spawns further workers of
//! the app's own on the same terms.

use core::sync::atomic::{AtomicU32, Ordering};

use bevy_platform::prelude::{String, Vec, format, vec};
use js_sys::{Array, Object, Reflect};
use wasm_bindgen::prelude::*;
use web_sys::{HtmlCanvasElement, OffscreenCanvas, Worker};

use bevy_render::settings::WgpuSettings;

pub use bevy_derive::page_functions;
pub use bevy_render::web_worker::{
    init_on_render_worker, provide_render_resources, render_worker_main,
};

#[wasm_bindgen(module = "/src/web_worker.js")]
extern "C" {
    #[wasm_bindgen(js_name = spawnWorker, catch)]
    fn spawn_worker_js(init: &Object) -> Result<Worker, JsValue>;
}

/// What every worker is spawned with.
pub struct WebWorkerConfig {
    /// The url of the wasm-bindgen glue module, for the workers to import.
    pub glue_url: String,
    /// The app's engine worker export.
    pub engine_entry: String,
    /// The engine export's argument.
    pub engine_args: JsValue,
    /// The app's render worker setup export, if any, called with the `OffscreenCanvas` before
    /// the render world is created there.
    pub render_setup: Option<String>,
    /// How many compute workers to spawn.
    pub compute_threads: u32,
    /// The render worker's wasm stack size in bytes; `None` takes the module's default.
    pub render_stack_size: Option<u32>,
    /// The compute workers' wasm stack size in bytes; `None` takes the module's default.
    pub compute_stack_size: Option<u32>,
    /// The engine worker's wasm stack size in bytes; `None` takes the module's default.
    pub engine_stack_size: Option<u32>,
}

/// A worker for [`spawn_worker`].
pub struct WorkerSpec {
    /// The app export the worker calls once its wasm instance is up.
    pub entry: String,
    /// The export's arguments.
    pub args: Vec<JsValue>,
    /// Names the worker in the page's console.
    pub tag: String,
    /// The worker's wasm stack size in bytes; `None` takes the module's default.
    pub stack_size: Option<u32>,
    /// Objects in `args` to transfer rather than clone.
    pub transfer: Vec<JsValue>,
}

/// The workers [`start`] spawned. The page may add its own listeners; the engine's relay
/// listeners are already installed.
pub struct WebWorkers {
    /// The render worker.
    pub render: Worker,
    /// The compute workers.
    pub compute: Vec<Worker>,
    /// The engine worker.
    pub engine: Worker,
}

/// Page side: prepares `canvas` for the engine and spawns its workers.
pub fn start(canvas: HtmlCanvasElement, config: &WebWorkerConfig) -> Result<WebWorkers, JsValue> {
    let (worker_id, offscreen) = bevy_winit::prepare_worker(canvas)?;
    let canvas_arg: JsValue = offscreen.clone().into();
    let mut render_calls = Vec::new();
    if let Some(setup) = &config.render_setup {
        render_calls.push((setup.as_str(), vec![canvas_arg.clone()]));
    }
    render_calls.push((RENDER_ENTRY, vec![canvas_arg]));
    let render = spawn(
        &config.glue_url,
        &render_calls,
        "render",
        config.render_stack_size,
        &[offscreen.into()],
    )?;
    let compute = (0..config.compute_threads)
        .map(|i| {
            spawn(
                &config.glue_url,
                &[(COMPUTE_ENTRY, Vec::new())],
                &format!("compute-{i}"),
                config.compute_stack_size,
                &[],
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    let engine = spawn(
        &config.glue_url,
        &[
            (
                ATTACH_ENTRY,
                vec![worker_id.into(), config.compute_threads.into()],
            ),
            (config.engine_entry.as_str(), vec![config.engine_args.clone()]),
        ],
        "engine",
        config.engine_stack_size,
        &[],
    )?;
    Ok(WebWorkers {
        render,
        compute,
        engine,
    })
}

/// Page side: spawns one worker on the engine's terms (see the module docs).
pub fn spawn_worker(glue_url: &str, spec: &WorkerSpec) -> Result<Worker, JsValue> {
    spawn(
        glue_url,
        &[(spec.entry.as_str(), spec.args.clone())],
        &spec.tag,
        spec.stack_size,
        &spec.transfer,
    )
}

/// Spawns a worker that calls `calls` in order (each an export and its arguments) once its wasm
/// instance is up.
fn spawn(
    glue_url: &str,
    calls: &[(&str, Vec<JsValue>)],
    tag: &str,
    stack_size: Option<u32>,
    transfer: &[JsValue],
) -> Result<Worker, JsValue> {
    let init = Object::new();
    let set = |key: &str, value: &JsValue| Reflect::set(&init, &key.into(), value).map(|_| ());
    set("glueUrl", &glue_url.into())?;
    let calls_array = Array::new();
    for (entry, args) in calls {
        let call = Object::new();
        Reflect::set(&call, &"entry".into(), &(*entry).into())?;
        Reflect::set(&call, &"args".into(), &args.iter().collect::<Array>())?;
        calls_array.push(&call);
    }
    set("calls", &calls_array)?;
    set("tag", &tag.into())?;
    set("module", &wasm_bindgen::module())?;
    set("memory", &wasm_bindgen::memory())?;
    if let Some(stack_size) = stack_size {
        set("stackSize", &stack_size.into())?;
    }
    set("transfer", &transfer.iter().collect::<Array>())?;
    spawn_worker_js(&init)
}

const RENDER_ENTRY: &str = "__bevy_render_worker_main";
const COMPUTE_ENTRY: &str = "__bevy_compute_worker_main";
const ATTACH_ENTRY: &str = "__bevy_attach_engine_worker";

static COMPUTE_THREADS: AtomicU32 = AtomicU32::new(0);

/// Engine worker side: how many compute workers [`start`] spawned, for sizing the app's
/// [`ComputeTaskPool`](bevy_tasks::ComputeTaskPool).
pub fn compute_threads() -> u32 {
    COMPUTE_THREADS.load(Ordering::Relaxed)
}

// The worker entries, called by the worker script.

#[doc(hidden)]
#[wasm_bindgen(js_name = "__bevy_render_worker_main")]
pub async fn render_worker_entry(canvas: OffscreenCanvas) -> Result<(), JsValue> {
    render_worker_main(canvas, WgpuSettings::default()).await
}

#[doc(hidden)]
#[wasm_bindgen(js_name = "__bevy_compute_worker_main")]
pub fn compute_worker_entry() {
    compute_worker_main();
}

#[doc(hidden)]
#[wasm_bindgen(js_name = "__bevy_attach_engine_worker")]
pub fn attach_engine_worker_entry(worker_id: u32, compute_threads: u32) -> Result<(), JsValue> {
    COMPUTE_THREADS.store(compute_threads, Ordering::Relaxed);
    attach_engine_worker(worker_id)
}

/// Engine worker side, before the app is built: attaches winit's event loop to the page's
/// canvas.
fn attach_engine_worker(worker_id: u32) -> Result<(), JsValue> {
    // The real canvas is on the render worker and the surface comes from there, so the window
    // handle winit needs is a placeholder.
    bevy_winit::attach_worker(worker_id, OffscreenCanvas::new(1, 1)?)
}

/// Compute worker side: joins the engine's [`ComputeTaskPool`](bevy_tasks::ComputeTaskPool)
/// once the engine worker has built it, and never returns.
fn compute_worker_main() {
    let pool = loop {
        if let Some(pool) = bevy_tasks::ComputeTaskPool::try_get() {
            break pool;
        }
        bevy_platform::thread::sleep(core::time::Duration::from_millis(5));
    };
    pool.run_worker();
}
