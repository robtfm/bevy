//! The render world on its own web worker.
//!
//! WebGPU objects are bound to the worker that created them, so the wgpu device, the surface
//! and the render world all live on one worker, the render worker, while the app world runs on
//! another. The page transfers the canvas to the render worker as an `OffscreenCanvas` (see
//! `bevy::web_worker` for the page side); the worker calls [`render_worker_main`], which creates
//! the wgpu resources and then hosts the render world for the life of the page. The app worker
//! builds the app with [`RenderCreation::WebWorker`](crate::settings::RenderCreation::WebWorker)
//! and `App::run` passes it through [`init_on_render_worker`], which runs `finish` and `cleanup`
//! (device setup and the start of the render loop) on the render worker before the app runs.
//! An app that creates the wgpu resources itself hands them over with
//! [`provide_render_resources`] on the render worker before [`render_worker_main`] runs.

use alloc::sync::Arc;
use std::sync::{Mutex, OnceLock};

use async_channel::{Receiver, Sender};
use bevy_app::App;
use wasm_bindgen::JsValue;
use web_sys::OffscreenCanvas;

use crate::{
    renderer::{initialize_renderer, RenderInstance, WgpuWrapper},
    settings::{RenderResources, WgpuSettings},
};

/// The wgpu resources [`render_worker_main`] created, taken by the render plugin's `finish`.
static RESOURCES: OnceLock<Arc<Mutex<Option<RenderResources>>>> = OnceLock::new();
/// The surface on the transferred canvas, taken by `finish` for the primary window.
static SURFACE: Mutex<Option<WgpuWrapper<wgpu::Surface<'static>>>> = Mutex::new(None);

pub(crate) fn resources_slot() -> Arc<Mutex<Option<RenderResources>>> {
    RESOURCES.get_or_init(Default::default).clone()
}

pub(crate) fn take_surface() -> Option<WgpuWrapper<wgpu::Surface<'static>>> {
    SURFACE.lock().unwrap().take()
}

/// Render worker side: wgpu resources the app created itself, for the render plugin to use in
/// place of the ones [`render_worker_main`] would create. `surface` is the one on the
/// transferred canvas, for the primary window.
pub fn provide_render_resources(resources: RenderResources, surface: wgpu::Surface<'static>) {
    *resources_slot().lock().unwrap() = Some(resources);
    *SURFACE.lock().unwrap() = Some(WgpuWrapper::new(surface));
}

/// The app crosses workers twice at startup (for `finish`/`cleanup` on the render worker, then
/// back to run); nothing runs on it in between.
struct SendApp(App);
// SAFETY: the app is only used by one worker at a time; it is sent, not shared.
#[expect(unsafe_code, reason = "see the safety comment above")]
unsafe impl Send for SendApp {}

/// The one-off handoffs between the two workers; in the shared wasm memory so every instance
/// sees the same ones.
struct Channels {
    to_render: (Sender<SendApp>, Receiver<SendApp>),
    to_app: (Sender<SendApp>, Receiver<SendApp>),
}

static CHANNELS: OnceLock<Channels> = OnceLock::new();

fn channels() -> &'static Channels {
    CHANNELS.get_or_init(|| Channels {
        to_render: async_channel::bounded(1),
        to_app: async_channel::bounded(1),
    })
}

fn js_err(e: impl core::fmt::Debug) -> JsValue {
    JsValue::from_str(&alloc::format!("{e:?}"))
}

/// Render worker side: creates the wgpu device and a surface on the transferred canvas (unless
/// the app [provided](provide_render_resources) them), then hosts the render world for the life
/// of the page. Resolves once the app has been initialised here and handed back to the app
/// worker; the render loop keeps running as a task on this worker.
pub async fn render_worker_main(
    canvas: OffscreenCanvas,
    settings: WgpuSettings,
) -> Result<(), JsValue> {
    if resources_slot().lock().unwrap().is_none() {
        create_render_resources(canvas, settings).await?;
    }

    // `finish` runs the device-touching plugin setup and `cleanup` spawns the render loop task,
    // both of which must happen on this worker.
    let SendApp(mut app) = channels().to_render.1.recv().await.map_err(js_err)?;
    app.finish();
    app.cleanup();
    channels().to_app.0.send(SendApp(app)).await.map_err(js_err)
}

async fn create_render_resources(
    canvas: OffscreenCanvas,
    settings: WgpuSettings,
) -> Result<(), JsValue> {
    let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
        backends: settings.backends.unwrap_or(wgpu::Backends::BROWSER_WEBGPU),
        flags: settings.instance_flags,
        ..Default::default()
    });
    let surface = instance
        .create_surface(wgpu::SurfaceTarget::OffscreenCanvas(canvas))
        .map_err(js_err)?;
    let (device, queue, adapter_info, adapter) = initialize_renderer(
        &instance,
        &settings,
        &wgpu::RequestAdapterOptions {
            power_preference: settings.power_preference,
            compatible_surface: Some(&surface),
            ..Default::default()
        },
    )
    .await;
    // wgpu treats uncaptured errors as fatal by default; report them instead.
    device
        .wgpu_device()
        .on_uncaptured_error(Box::new(|e: wgpu::Error| {
            tracing::error!("uncaptured wgpu error: {e:?}");
        }));
    *resources_slot().lock().unwrap() = Some(RenderResources(
        device,
        queue,
        adapter_info,
        adapter,
        RenderInstance(Arc::new(WgpuWrapper::new(instance))),
    ));
    *SURFACE.lock().unwrap() = Some(WgpuWrapper::new(surface));
    Ok(())
}

/// App worker side: has the app's `finish` and `cleanup` run on the render worker and returns
/// it ready to run; `App::run` does this for a [`RenderCreation::WebWorker`] app. Blocks until
/// the render worker hands the app back, so it must be called off the page's main thread.
///
/// [`RenderCreation::WebWorker`]: crate::settings::RenderCreation::WebWorker
pub fn init_on_render_worker(app: App) -> Result<App, JsValue> {
    futures_lite::future::block_on(channels().to_render.0.send(SendApp(app)))
        .map_err(|_| js_err("render worker gone"))?;
    let SendApp(app) =
        futures_lite::future::block_on(channels().to_app.1.recv()).map_err(js_err)?;
    Ok(app)
}
