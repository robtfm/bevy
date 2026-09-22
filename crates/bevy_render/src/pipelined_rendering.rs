use async_channel::{Receiver, Sender};

use bevy_app::{App, AppExit, AppLabel, Plugin, SubApp};
use bevy_diagnostic::{
    Diagnostic, DiagnosticMeasurement, DiagnosticPath, DiagnosticsStore, RegisterDiagnostic,
};
use bevy_ecs::{
    resource::Resource,
    schedule::MainThreadExecutor,
    world::{Mut, World},
};
use bevy_platform::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
    time::Instant,
};
use bevy_tasks::ComputeTaskPool;

use crate::RenderApp;

/// A Label for the sub app that runs the parts of pipelined rendering that need to run on the main thread.
///
/// The Main schedule of this app can be used to run logic after the render schedule starts, but
/// before I/O processing. This can be useful for something like frame pacing.
#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq, AppLabel)]
pub struct RenderExtractApp;

/// Channels used by the main app to send and receive the render app.
#[derive(Resource)]
pub struct RenderAppChannels {
    app_to_render_sender: Sender<SubApp>,
    render_to_app_receiver: Receiver<SubApp>,
    /// Signalled once the render worker's animation-frame task has taken the render app.
    #[cfg(target_arch = "wasm32")]
    handoff_taken_receiver: Receiver<()>,
    render_app_in_render_thread: bool,
}

impl RenderAppChannels {
    /// Create a `RenderAppChannels` from a [`async_channel::Receiver`] and [`async_channel::Sender`]
    pub fn new(
        app_to_render_sender: Sender<SubApp>,
        render_to_app_receiver: Receiver<SubApp>,
        #[cfg(target_arch = "wasm32")] handoff_taken_receiver: Receiver<()>,
    ) -> Self {
        Self {
            app_to_render_sender,
            render_to_app_receiver,
            #[cfg(target_arch = "wasm32")]
            handoff_taken_receiver,
            render_app_in_render_thread: false,
        }
    }

    /// Send the `render_app` to the rendering thread.
    ///
    /// On the web this also waits until the render worker has picked the app up from inside
    /// its animation-frame callback, so the app world is paced by the render worker's frames.
    pub fn send_blocking(&mut self, render_app: SubApp) {
        self.app_to_render_sender.send_blocking(render_app).unwrap();
        self.render_app_in_render_thread = true;
        #[cfg(target_arch = "wasm32")]
        self.handoff_taken_receiver.recv_blocking().ok();
    }

    /// Receive the `render_app` from the rendering thread.
    /// Return `None` if the render thread has panicked.
    pub async fn recv(&mut self) -> Option<SubApp> {
        let render_app = self.render_to_app_receiver.recv().await.ok()?;
        self.render_app_in_render_thread = false;
        Some(render_app)
    }
}

impl Drop for RenderAppChannels {
    fn drop(&mut self) {
        if self.render_app_in_render_thread {
            // Any non-send data in the render world was initialized on the main thread.
            // So on dropping the main world and ending the app, we block and wait for
            // the render world to return to drop it. Which allows the non-send data
            // drop methods to run on the correct thread.
            self.render_to_app_receiver.recv_blocking().ok();
        }
    }
}

/// The [`PipelinedRenderingPlugin`] can be added to your application to enable pipelined rendering.
///
/// This moves rendering into a different thread, so that the Nth frame's rendering can
/// be run at the same time as the N + 1 frame's simulation.
///
/// ```text
/// |--------------------|--------------------|--------------------|--------------------|
/// | simulation thread  | frame 1 simulation | frame 2 simulation | frame 3 simulation |
/// |--------------------|--------------------|--------------------|--------------------|
/// | rendering thread   |                    | frame 1 rendering  | frame 2 rendering  |
/// |--------------------|--------------------|--------------------|--------------------|
/// ```
///
/// The plugin is dependent on the [`RenderApp`] added by [`crate::RenderPlugin`] and so must
/// be added after that plugin. If it is not added after, the plugin will do nothing.
///
/// A single frame of execution looks something like below
///
/// ```text
/// |---------------------------------------------------------------------------|
/// |      |         | RenderExtractApp schedule | winit events | main schedule |
/// | sync | extract |----------------------------------------------------------|
/// |      |         | extract commands | rendering schedule                    |
/// |---------------------------------------------------------------------------|
/// ```
///
/// - `sync` is the step where the entity-entity mapping between the main and render world is updated.
///   This is run on the main app's thread. For more information checkout [`SyncWorldPlugin`].
/// - `extract` is the step where data is copied from the main world to the render world.
///   This is run on the main app's thread.
/// - On the render thread, we first apply the `extract commands`. This is not run during extract, so the
///   main schedule can start sooner.
/// - Then the `rendering schedule` is run. See [`RenderSet`](crate::RenderSet) for the standard steps in this process.
/// - In parallel to the rendering thread the [`RenderExtractApp`] schedule runs. By
///   default, this schedule is empty. But it is useful if you need something to run before I/O processing.
/// - Next all the `winit events` are processed.
/// - And finally the `main app schedule` is run.
/// - Once both the `main app schedule` and the `render schedule` are finished running, `extract` is run again.
///
/// [`SyncWorldPlugin`]: crate::sync_world::SyncWorldPlugin
/// Wall-clock timings of the pipeline's stages, per frame, in milliseconds. Published into the
/// main world's [`DiagnosticsStore`] when the render app is handed over.
pub mod diagnostics {
    use bevy_diagnostic::DiagnosticPath;

    /// The main world's update, from the previous handoff to this one.
    pub const MAIN_UPDATE: DiagnosticPath = DiagnosticPath::const_new("pipelined/main_update");
    /// The main world waiting for the render app to come back.
    pub const WAIT_RENDER: DiagnosticPath = DiagnosticPath::const_new("pipelined/wait_render");
    /// The main world blocked handing the render app over (web: until the render worker's
    /// animation-frame task takes it).
    pub const WAIT_HANDOFF: DiagnosticPath = DiagnosticPath::const_new("pipelined/wait_handoff");
    /// The render app's update on the render thread.
    pub const RENDER_UPDATE: DiagnosticPath = DiagnosticPath::const_new("pipelined/render_update");
    /// The render thread waiting for the main world's handoff.
    pub const WAIT_MAIN: DiagnosticPath = DiagnosticPath::const_new("pipelined/wait_main");
    /// Web: the render worker waiting for the GPU backlog to drain.
    pub const WAIT_GPU: DiagnosticPath = DiagnosticPath::const_new("pipelined/wait_gpu");
    /// Web: the render worker waiting for the next animation frame.
    pub const WAIT_FRAME: DiagnosticPath = DiagnosticPath::const_new("pipelined/wait_frame");
    /// Web: estimated GPU time per frame, from when the frame's work could start (its submission
    /// or the previous frame's completion) to its fence completing.
    pub const GPU: DiagnosticPath = DiagnosticPath::const_new("pipelined/gpu");
}

/// The render thread's timings for its last frame, read by the main world at the next handoff.
#[derive(Default, Clone, Copy)]
struct RenderThreadStats {
    render_update: f64,
    wait_main: f64,
    wait_gpu: f64,
    wait_frame: f64,
    gpu: f64,
}

static RENDER_THREAD_STATS: Mutex<RenderThreadStats> = Mutex::new(RenderThreadStats {
    render_update: 0.0,
    wait_main: 0.0,
    wait_gpu: 0.0,
    wait_frame: 0.0,
    gpu: 0.0,
});

/// When the main world last handed the render app over; the next handoff measures its update.
static LAST_HANDOFF: Mutex<Option<Instant>> = Mutex::new(None);

/// Set by [`PipelinedRenderingDiagnosticsPlugin`]; nothing is measured until it is.
static DIAGNOSTICS_ENABLED: AtomicBool = AtomicBool::new(false);

fn enabled() -> bool {
    DIAGNOSTICS_ENABLED.load(Ordering::Relaxed)
}

fn timer() -> Option<Instant> {
    enabled().then(Instant::now)
}

fn elapsed_ms(since: Option<Instant>) -> f64 {
    since.map_or(0.0, |since| since.elapsed().as_secs_f64() * 1000.0)
}

fn record(store: &mut DiagnosticsStore, path: &DiagnosticPath, value: f64) {
    if let Some(diagnostic) = store.get_mut(path) {
        diagnostic.add_measurement(DiagnosticMeasurement {
            time: Instant::now(),
            value,
        });
    }
}

/// Publishes the pipeline's per-frame timings ([`diagnostics`]) into the main world's
/// [`DiagnosticsStore`]. Without it nothing is measured.
#[derive(Default)]
pub struct PipelinedRenderingDiagnosticsPlugin;

impl Plugin for PipelinedRenderingDiagnosticsPlugin {
    fn build(&self, app: &mut App) {
        #[cfg(target_arch = "wasm32")]
        let web = [
            diagnostics::WAIT_GPU,
            diagnostics::WAIT_FRAME,
            diagnostics::GPU,
        ];
        #[cfg(not(target_arch = "wasm32"))]
        let web: [DiagnosticPath; 0] = [];
        for path in [
            diagnostics::MAIN_UPDATE,
            diagnostics::WAIT_RENDER,
            diagnostics::WAIT_HANDOFF,
            diagnostics::RENDER_UPDATE,
            diagnostics::WAIT_MAIN,
        ]
        .into_iter()
        .chain(web)
        {
            app.register_diagnostic(Diagnostic::new(path).with_suffix("ms"));
        }
        DIAGNOSTICS_ENABLED.store(true, Ordering::Relaxed);
    }
}

#[derive(Default)]
pub struct PipelinedRenderingPlugin;

impl Plugin for PipelinedRenderingPlugin {
    fn build(&self, app: &mut App) {
        // Don't add RenderExtractApp if RenderApp isn't initialized.
        if app.get_sub_app(RenderApp).is_none() {
            return;
        }
        app.insert_resource(MainThreadExecutor::new());

        let mut sub_app = SubApp::new();
        sub_app.set_extract(renderer_extract);
        app.insert_sub_app(RenderExtractApp, sub_app);
    }

    // Sets up the render thread and inserts resources into the main app used for controlling the render thread.
    fn cleanup(&self, app: &mut App) {
        // skip setting up when headless
        if app.get_sub_app(RenderExtractApp).is_none() {
            return;
        }

        let (app_to_render_sender, app_to_render_receiver) = async_channel::bounded::<SubApp>(1);
        let (render_to_app_sender, render_to_app_receiver) = async_channel::bounded::<SubApp>(1);
        #[cfg(target_arch = "wasm32")]
        let (handoff_taken_sender, handoff_taken_receiver) = async_channel::bounded::<()>(1);

        let mut render_app = app
            .remove_sub_app(RenderApp)
            .expect("Unable to get RenderApp. Another plugin may have removed the RenderApp before PipelinedRenderingPlugin");

        // clone main thread executor to render world
        let executor = app.world().get_resource::<MainThreadExecutor>().unwrap();
        render_app.world_mut().insert_resource(executor.clone());

        render_to_app_sender.send_blocking(render_app).unwrap();

        app.insert_resource(RenderAppChannels::new(
            app_to_render_sender,
            render_to_app_receiver,
            #[cfg(target_arch = "wasm32")]
            handoff_taken_receiver,
        ));

        // On the web the render loop stays on the worker that calls `cleanup` (the one that
        // owns the wgpu device and the OffscreenCanvas). It has to return to the JS event loop
        // after every frame for the browser to present, so it is an async task, not a thread.
        // Each frame is collected and rendered from inside an animation-frame callback, so the
        // canvas commits on the display's cadence and the app world (blocked in `send_blocking`
        // until the handoff is taken) runs one frame ahead of it.
        #[cfg(target_arch = "wasm32")]
        wasm_bindgen_futures::spawn_local(async move {
            // Submitted frames the GPU may still be working on. One idles the GPU between a
            // completion and the next animation frame; each extra frame adds a refresh of
            // latency for a smaller throughput gain.
            const FRAMES_IN_FLIGHT: usize = 2;
            // 4-byte mappable buffers used as GPU fences, one per frame in flight: mapping is
            // queued behind everything submitted before it, and wgpu's WebGPU backend has no
            // `on_submitted_work_done`.
            let mut fences: Vec<crate::render_resource::Buffer> = Vec::new();
            // Per frame in flight: its fence, and when the fence completed (sent by the map
            // callback). With the frame's submission time that estimates the GPU time.
            let mut in_flight: std::collections::VecDeque<(usize, Receiver<Instant>, Instant)> =
                std::collections::VecDeque::new();
            let mut last_completed: Option<Instant> = None;
            let mut frame: usize = 0;
            let mut stats = RenderThreadStats::default();
            loop {
                // Take the handoff first, then render inside the next animation-frame callback:
                // frames committed outside the callback are not paced by the compositor, and
                // when two land in one refresh the earlier one is dropped.
                let started = timer();
                let Ok(mut render_app) = app_to_render_receiver.recv().await else {
                    break;
                };
                stats.wait_main = elapsed_ms(started);
                let started = timer();
                // A worker's animation frames are not throttled by GPU backlog the way a page's
                // are, so bound the backlog here before waiting for the next frame.
                while in_flight.len() >= FRAMES_IN_FLIGHT {
                    let (index, done, submitted) = in_flight.pop_front().unwrap();
                    if let Ok(completed) = done.recv().await {
                        let could_start = last_completed.map_or(submitted, |c| c.max(submitted));
                        stats.gpu = completed
                            .saturating_duration_since(could_start)
                            .as_secs_f64()
                            * 1000.0;
                        last_completed = Some(completed);
                    }
                    fences[index].unmap();
                }
                stats.wait_gpu = elapsed_ms(started);
                let started = timer();
                if web::animation_frame().await.is_err() {
                    break;
                }
                stats.wait_frame = elapsed_ms(started);
                if handoff_taken_sender.send(()).await.is_err() {
                    break;
                }
                let started = timer();
                render_app.update();
                stats.render_update = elapsed_ms(started);
                if enabled() {
                    *RENDER_THREAD_STATS.lock().unwrap() = stats;
                }
                if fences.is_empty() {
                    if let Some(device) = render_app
                        .world()
                        .get_resource::<crate::renderer::RenderDevice>()
                    {
                        fences = (0..FRAMES_IN_FLIGHT)
                            .map(|_| {
                                device.create_buffer(&wgpu::BufferDescriptor {
                                    label: Some("pipelined rendering frame fence"),
                                    size: 4,
                                    usage: wgpu::BufferUsages::MAP_READ
                                        | wgpu::BufferUsages::COPY_DST,
                                    mapped_at_creation: false,
                                })
                            })
                            .collect();
                    }
                }
                if !fences.is_empty() {
                    let index = frame % FRAMES_IN_FLIGHT;
                    let (sender, receiver) = async_channel::bounded(1);
                    wgpu::Buffer::slice(&fences[index], ..).map_async(
                        wgpu::MapMode::Read,
                        move |_| {
                            sender.try_send(Instant::now()).ok();
                        },
                    );
                    in_flight.push_back((index, receiver, Instant::now()));
                    frame += 1;
                }
                if render_to_app_sender.send(render_app).await.is_err() {
                    break;
                }
            }
            tracing::debug!("exiting pipelined rendering task");
        });

        #[cfg(not(target_arch = "wasm32"))]
        std::thread::spawn(move || {
            #[cfg(feature = "trace")]
            let _span = tracing::info_span!("render thread").entered();

            let compute_task_pool = ComputeTaskPool::get();
            let mut stats = RenderThreadStats::default();
            loop {
                // run a scope here to allow main world to use this thread while it's waiting for the render app
                let started = timer();
                let sent_app = compute_task_pool
                    .scope(|s| {
                        s.spawn(async { app_to_render_receiver.recv().await });
                    })
                    .pop();
                let Some(Ok(mut render_app)) = sent_app else {
                    break;
                };
                stats.wait_main = elapsed_ms(started);

                {
                    #[cfg(feature = "trace")]
                    let _sub_app_span = tracing::info_span!("sub app", name = ?RenderApp).entered();
                    let started = timer();
                    render_app.update();
                    stats.render_update = elapsed_ms(started);
                    if enabled() {
                        *RENDER_THREAD_STATS.lock().unwrap() = stats;
                    }
                }

                if render_to_app_sender.send_blocking(render_app).is_err() {
                    break;
                }
            }

            tracing::debug!("exiting pipelined rendering thread");
        });
    }
}

// `async_channel` only offers its blocking helpers off-wasm; on a web worker parking the
// thread via `block_on` is fine (it is never the page's main thread).
#[cfg(target_arch = "wasm32")]
trait SendBlocking<T> {
    fn send_blocking(&self, value: T) -> Result<(), async_channel::SendError<T>>;
}

#[cfg(target_arch = "wasm32")]
impl<T> SendBlocking<T> for Sender<T> {
    fn send_blocking(&self, value: T) -> Result<(), async_channel::SendError<T>> {
        futures_lite::future::block_on(self.send(value))
    }
}

#[cfg(target_arch = "wasm32")]
trait RecvBlocking<T> {
    fn recv_blocking(&self) -> Result<T, async_channel::RecvError>;
}

#[cfg(target_arch = "wasm32")]
impl<T> RecvBlocking<T> for Receiver<T> {
    fn recv_blocking(&self) -> Result<T, async_channel::RecvError> {
        futures_lite::future::block_on(self.recv())
    }
}

#[cfg(target_arch = "wasm32")]
mod web {
    use wasm_bindgen::{JsCast, JsValue};

    /// Resolves inside the worker's next animation-frame callback.
    pub async fn animation_frame() -> Result<(), JsValue> {
        let promise = js_sys::Promise::new(&mut |resolve, reject| {
            let scope: web_sys::DedicatedWorkerGlobalScope = js_sys::global().unchecked_into();
            if let Err(err) = scope.request_animation_frame(&resolve) {
                reject.call1(&JsValue::UNDEFINED, &err).ok();
            }
        });
        wasm_bindgen_futures::JsFuture::from(promise)
            .await
            .map(|_| ())
    }
}

// This function waits for the rendering world to be received,
// runs extract, and then sends the rendering world back to the render thread.
fn renderer_extract(app_world: &mut World, _world: &mut World) {
    let handoff = timer();
    let main_update = handoff.and_then(|handoff| {
        LAST_HANDOFF
            .lock()
            .unwrap()
            .take()
            .map(|last| handoff.saturating_duration_since(last).as_secs_f64() * 1000.0)
    });
    app_world.resource_scope(|world, main_thread_executor: Mut<MainThreadExecutor>| {
        world.resource_scope(|world, mut render_channels: Mut<RenderAppChannels>| {
            // we use a scope here to run any main thread tasks that the render world still needs to run
            // while we wait for the render world to be received.
            let received = ComputeTaskPool::get()
                .scope_with_executor(true, Some(&*main_thread_executor.0), |s| {
                    s.spawn(async { render_channels.recv().await });
                })
                .pop()
                .unwrap();
            let wait_render = elapsed_ms(handoff);
            if let Some(mut render_app) = received {
                render_app.extract(world);

                let sending = timer();
                render_channels.send_blocking(render_app);
                let wait_handoff = elapsed_ms(sending);
                let store = timer().and_then(|now| {
                    *LAST_HANDOFF.lock().unwrap() = Some(now);
                    world.get_resource_mut::<DiagnosticsStore>()
                });
                if let Some(mut store) = store {
                    if let Some(main_update) = main_update {
                        record(&mut store, &diagnostics::MAIN_UPDATE, main_update);
                    }
                    record(&mut store, &diagnostics::WAIT_RENDER, wait_render);
                    record(&mut store, &diagnostics::WAIT_HANDOFF, wait_handoff);
                    let stats = *RENDER_THREAD_STATS.lock().unwrap();
                    record(&mut store, &diagnostics::RENDER_UPDATE, stats.render_update);
                    record(&mut store, &diagnostics::WAIT_MAIN, stats.wait_main);
                    record(&mut store, &diagnostics::WAIT_GPU, stats.wait_gpu);
                    record(&mut store, &diagnostics::WAIT_FRAME, stats.wait_frame);
                    record(&mut store, &diagnostics::GPU, stats.gpu);
                }
            } else {
                // Renderer thread panicked
                world.send_event(AppExit::error());
            }
        });
    });
}
