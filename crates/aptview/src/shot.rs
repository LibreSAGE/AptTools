//! Headless rendering: play a movie into an off-screen target and save a PNG.
//!
//! Useful for eyeballing a conversion without a window, and for regression
//! checks in CI. Uses the same player setup as the windowed viewer, so imported
//! movies load the same way.

use std::any::Any;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context, Result};
use ruffle_core::backend::navigator::{NullExecutor, NullNavigatorBackend};
use ruffle_core::config::Letterbox;
use ruffle_core::tag_utils::SwfMovie;
use ruffle_core::{Player, PlayerBuilder, StageScaleMode};
use ruffle_render_wgpu::backend::{
    create_wgpu_instance, request_adapter_and_device, WgpuRenderBackend,
};
use ruffle_render_wgpu::descriptors::Descriptors;
use ruffle_render_wgpu::target::TextureTarget;
use ruffle_render_wgpu::wgpu;

/// Render `frame` (1-based) of the movie and write it to `output` as a PNG.
/// With `hover`, the mouse moves there (and presses/releases if `click`)
/// halfway through the frames, so button states can be exercised headlessly.
pub fn capture(
    swf_bytes: Vec<u8>,
    url: String,
    base_dir: PathBuf,
    frame: u32,
    output: &Path,
    hover: Option<(f64, f64)>,
    click: bool,
) -> Result<()> {
    let movie = SwfMovie::from_data(&swf_bytes, url, None, None)
        .map_err(|e| anyhow!("parsing SWF: {e}"))?;
    let width = movie.width().to_pixels().max(1.0) as u32;
    let height = movie.height().to_pixels().max(1.0) as u32;

    let instance = create_wgpu_instance(wgpu::Backends::PRIMARY, wgpu::BackendOptions::default());
    let (adapter, device, queue) = futures::executor::block_on(request_adapter_and_device(
        wgpu::Backends::PRIMARY,
        &instance,
        None,
        wgpu::PowerPreference::HighPerformance,
    ))
    .map_err(|e| anyhow!("no usable graphics adapter: {e}"))?;
    let descriptors = Arc::new(Descriptors::new(instance, adapter, device, queue));

    let target = TextureTarget::new(&descriptors.device, (width, height))
        .map_err(|e| anyhow!("render target: {e}"))?;
    let renderer = WgpuRenderBackend::new(descriptors, target).map_err(|e| anyhow!("{e}"))?;

    let mut executor = NullExecutor::new();
    let navigator = NullNavigatorBackend::with_base_path(&base_dir, &executor)
        .map_err(|e| anyhow!("navigator base path {}: {e}", base_dir.display()))?;

    let player = PlayerBuilder::new()
        .with_renderer(renderer)
        .with_navigator(navigator)
        .with_movie(movie)
        .with_autoplay(true)
        .with_letterbox(Letterbox::On)
        .with_scale_mode(StageScaleMode::ShowAll, false)
        .with_viewport_dimensions(width, height, 1.0)
        .build();

    preload_with_imports(&player, &mut executor);

    // Advance to the requested frame. `run_frame` is the exporter-style frame
    // step; `tick` only accumulates wall-clock time and is wrong here.
    let total = frame.max(1);
    for i in 0..total {
        if let Some((x, y)) = hover {
            // Move the mouse halfway through so intro animations have settled
            // but the state change still has frames left to play out.
            if i == total / 2 {
                let mut p = player.lock().unwrap();
                p.set_mouse_in_stage(true);
                p.handle_event(ruffle_core::PlayerEvent::MouseMove { x, y });
                if click {
                    p.handle_event(ruffle_core::PlayerEvent::MouseDown {
                        x,
                        y,
                        button: ruffle_core::events::MouseButton::Left,
                        index: None,
                    });
                    p.handle_event(ruffle_core::PlayerEvent::MouseUp {
                        x,
                        y,
                        button: ruffle_core::events::MouseButton::Left,
                    });
                }
            }
        }
        player.lock().unwrap().run_frame();
        executor.run();
    }

    player.lock().unwrap().render();
    let image = capture_frame(&player).context("capturing frame")?;
    image
        .save(output)
        .with_context(|| format!("writing {}", output.display()))?;
    log::info!(
        "wrote {} ({width}x{height}, frame {frame})",
        output.display()
    );
    Ok(())
}

/// Preload until imported movies have been fetched and registered.
///
/// The EA engine links imports synchronously before the first frame; Ruffle
/// fetches them through the navigator instead. Without pumping the executor
/// here, frame 0 would place characters that aren't in the library yet and
/// imported art would never appear (the game movies re-place nothing later).
pub(crate) fn preload_with_imports(player: &Arc<Mutex<Player>>, executor: &mut NullExecutor) {
    use ruffle_core::limits::ExecutionLimit;
    for _ in 0..64 {
        let done = player.lock().unwrap().preload(&mut ExecutionLimit::none());
        executor.run();
        if done {
            break;
        }
    }
}

fn capture_frame(player: &Arc<Mutex<Player>>) -> Option<image::RgbaImage> {
    let mut player = player.lock().unwrap();
    let renderer =
        <dyn Any>::downcast_mut::<WgpuRenderBackend<TextureTarget>>(player.renderer_mut())?;
    renderer.capture_frame()
}
