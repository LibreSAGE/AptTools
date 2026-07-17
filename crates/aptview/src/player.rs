//! The embedded Ruffle player: a winit window driving `ruffle_core`.
//!
//! Deliberately minimal — window, renderer, input forwarding, and the frame
//! clock. No debug UI.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{anyhow, Result};
use ruffle_core::backend::navigator::{NullExecutor, NullNavigatorBackend};
use ruffle_core::config::Letterbox;
use ruffle_core::events::{
    KeyDescriptor, KeyLocation, LogicalKey, MouseButton as RuffleMouseButton, MouseWheelDelta,
    NamedKey as RuffleNamedKey, PhysicalKey as RufflePhysicalKey,
};
use ruffle_core::tag_utils::SwfMovie;
use ruffle_core::{FloatDuration, Player, PlayerBuilder, PlayerEvent, StageScaleMode};
use ruffle_render::backend::ViewportDimensions;
use ruffle_render_wgpu::backend::WgpuRenderBackend;
use ruffle_render_wgpu::wgpu;
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition};
use winit::event::{ElementState, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, KeyLocation as WinitKeyLocation, NamedKey};
use winit::window::{Window, WindowId};

/// Play `swf_bytes` in a window until it is closed.
///
/// `url` is the movie's `file://` URL and `base_dir` the directory it lives in;
/// together they let the player fetch the sibling SWFs the movie imports.
pub fn run(swf_bytes: Vec<u8>, url: String, base_dir: PathBuf, title: String) -> Result<()> {
    let event_loop = EventLoop::new()?;
    let mut app = App {
        swf_bytes,
        url,
        base_dir,
        title,
        window: None,
        player: None,
        executor: NullExecutor::new(),
        time: Instant::now(),
        next_frame_time: None,
        mouse_pos: PhysicalPosition::new(0.0, 0.0),
    };
    event_loop.run_app(&mut app)?;
    Ok(())
}

struct App {
    swf_bytes: Vec<u8>,
    url: String,
    base_dir: PathBuf,
    title: String,
    // The player's wgpu surface was created from the window with an erased
    // lifetime (`for_window_unsafe`), so the window must outlive it — and, on
    // shutdown, the surface must be torn down *before* the window's Wayland/X
    // connection is. Struct fields drop in declaration order, so `player` is
    // declared before `window` to drop first; `exiting` also drops them in this
    // order explicitly, before winit destroys the window.
    player: Option<Arc<Mutex<Player>>>,
    window: Option<Arc<Window>>,
    /// Drives the navigator's fetches (loading imported movies). Nothing polls
    /// these futures unless we run it, so imports would never arrive.
    executor: NullExecutor,
    time: Instant,
    next_frame_time: Option<Instant>,
    mouse_pos: PhysicalPosition<f64>,
}

impl App {
    fn init(&mut self, event_loop: &ActiveEventLoop) -> Result<()> {
        let movie = SwfMovie::from_data(&self.swf_bytes, self.url.clone(), None, None)
            .map_err(|e| anyhow!("parsing SWF: {e}"))?;
        let (movie_w, movie_h) = (movie.width().to_pixels(), movie.height().to_pixels());

        let window = Arc::new(
            event_loop.create_window(
                Window::default_attributes()
                    .with_title(self.title.clone())
                    .with_inner_size(LogicalSize::new(movie_w.max(1.0), movie_h.max(1.0))),
            )?,
        );

        let size = window.inner_size();
        let renderer = unsafe {
            WgpuRenderBackend::for_window_unsafe(
                wgpu::SurfaceTargetUnsafe::from_window(window.as_ref())
                    .map_err(|e| anyhow!("surface target: {e}"))?,
                (size.width.max(1), size.height.max(1)),
                wgpu::Backends::PRIMARY,
                wgpu::PowerPreference::HighPerformance,
            )
        }
        .map_err(|e| anyhow!("creating renderer: {e}"))?;

        // The default navigator can't fetch anything; this one serves the
        // movie's own directory, which is where its imported movies live.
        let navigator = NullNavigatorBackend::with_base_path(&self.base_dir, &self.executor)
            .map_err(|e| anyhow!("navigator base path {}: {e}", self.base_dir.display()))?;

        let player = PlayerBuilder::new()
            .with_renderer(renderer)
            .with_navigator(navigator)
            .with_movie(movie)
            // Defaults to false, which would make `tick` silently do nothing.
            .with_autoplay(true)
            .with_letterbox(Letterbox::On)
            .with_scale_mode(StageScaleMode::ShowAll, false)
            .with_viewport_dimensions(size.width, size.height, window.scale_factor())
            .build();

        // Fetch and register imported movies before the first frame runs, the
        // way the game engine links imports (see shot::preload_with_imports).
        crate::shot::preload_with_imports(&player, &mut self.executor);

        self.window = Some(window);
        self.player = Some(player);
        self.time = Instant::now();
        Ok(())
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return; // resumed() can fire more than once
        }
        if let Err(e) = self.init(event_loop) {
            log::error!("{e:#}");
            event_loop.exit();
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let (Some(window), Some(player)) = (self.window.as_ref(), self.player.as_ref()) else {
            return;
        };
        match event {
            WindowEvent::CloseRequested => {
                event_loop.exit();
                return;
            }
            WindowEvent::RedrawRequested => player.lock().unwrap().render(),
            WindowEvent::Resized(size) => {
                player
                    .lock()
                    .unwrap()
                    .set_viewport_dimensions(ViewportDimensions {
                        width: size.width.max(1),
                        height: size.height.max(1),
                        scale_factor: window.scale_factor(),
                    });
                window.request_redraw();
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.mouse_pos = position;
                player.lock().unwrap().handle_event(PlayerEvent::MouseMove {
                    x: position.x,
                    y: position.y,
                });
            }
            WindowEvent::MouseInput { button, state, .. } => {
                let button = match button {
                    MouseButton::Left => RuffleMouseButton::Left,
                    MouseButton::Right => RuffleMouseButton::Right,
                    MouseButton::Middle => RuffleMouseButton::Middle,
                    _ => RuffleMouseButton::Unknown,
                };
                let (x, y) = (self.mouse_pos.x, self.mouse_pos.y);
                let event = match state {
                    ElementState::Pressed => PlayerEvent::MouseDown {
                        x,
                        y,
                        button,
                        index: None,
                    },
                    ElementState::Released => PlayerEvent::MouseUp { x, y, button },
                };
                player.lock().unwrap().handle_event(event);
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let delta = match delta {
                    MouseScrollDelta::LineDelta(_, dy) => MouseWheelDelta::Lines(dy.into()),
                    MouseScrollDelta::PixelDelta(pos) => MouseWheelDelta::Pixels(pos.y),
                };
                player
                    .lock()
                    .unwrap()
                    .handle_event(PlayerEvent::MouseWheel { delta });
            }
            WindowEvent::CursorLeft { .. } => {
                let mut p = player.lock().unwrap();
                p.set_mouse_in_stage(false);
                p.handle_event(PlayerEvent::MouseLeave);
            }
            WindowEvent::CursorEntered { .. } => player.lock().unwrap().set_mouse_in_stage(true),
            WindowEvent::KeyboardInput { event, .. } => {
                let key = map_key(&event);
                let mut p = player.lock().unwrap();
                match event.state {
                    ElementState::Pressed => {
                        p.handle_event(PlayerEvent::KeyDown { key });
                        // Text input is a separate event from KeyDown.
                        if let Some(text) = &event.text {
                            for codepoint in text.chars() {
                                p.handle_event(PlayerEvent::TextInput { codepoint });
                            }
                        }
                    }
                    ElementState::Released => {
                        p.handle_event(PlayerEvent::KeyUp { key });
                    }
                }
            }
            _ => {}
        }

        if player.lock().unwrap().needs_render() {
            window.request_redraw();
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let (Some(window), Some(player)) = (self.window.as_ref(), self.player.as_ref()) else {
            return;
        };
        let now = Instant::now();
        let dt = FloatDuration::from_std(now.duration_since(self.time));
        if dt.as_millis() > 0.0 {
            self.time = now;
            let mut p = player.lock().unwrap();
            p.tick(dt);
            self.next_frame_time = Some(now + p.time_til_next_frame());
            if p.needs_render() {
                window.request_redraw();
            }
            drop(p);
            // Let pending fetches (imported movies) make progress.
            self.executor.run();
        }
        if let Some(t) = self.next_frame_time {
            event_loop.set_control_flow(ControlFlow::WaitUntil(t));
        }
    }

    /// Tear down while the window is still alive. The player owns the wgpu
    /// surface built from this window; dropping the window (and its Wayland/X
    /// connection) first would make the surface's unconfigure-on-drop
    /// dereference freed objects and segfault (in `libwayland-client`). Drop the
    /// player first, then the window — winit only destroys the window after
    /// `exiting` returns.
    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        self.player = None;
        self.window = None;
    }
}

/// Map a winit key event to Ruffle's key descriptor.
///
/// Ruffle's exhaustive version is `winit_input_to_ruffle_key_descriptor` in
/// `desktop/src/util.rs`; this covers the keys a UI movie actually reacts to.
fn map_key(event: &KeyEvent) -> KeyDescriptor {
    let logical_key = match event.logical_key.as_ref() {
        Key::Character(c) => c
            .chars()
            .next()
            .map(LogicalKey::Character)
            .unwrap_or(LogicalKey::Unknown),
        Key::Named(NamedKey::Space) => LogicalKey::Character(' '),
        Key::Named(NamedKey::Enter) => LogicalKey::Named(RuffleNamedKey::Enter),
        Key::Named(NamedKey::Backspace) => LogicalKey::Named(RuffleNamedKey::Backspace),
        Key::Named(NamedKey::Tab) => LogicalKey::Named(RuffleNamedKey::Tab),
        Key::Named(NamedKey::Escape) => LogicalKey::Named(RuffleNamedKey::Escape),
        Key::Named(NamedKey::Delete) => LogicalKey::Named(RuffleNamedKey::Delete),
        Key::Named(NamedKey::Home) => LogicalKey::Named(RuffleNamedKey::Home),
        Key::Named(NamedKey::End) => LogicalKey::Named(RuffleNamedKey::End),
        Key::Named(NamedKey::Shift) => LogicalKey::Named(RuffleNamedKey::Shift),
        Key::Named(NamedKey::Control) => LogicalKey::Named(RuffleNamedKey::Control),
        Key::Named(NamedKey::Alt) => LogicalKey::Named(RuffleNamedKey::Alt),
        Key::Named(NamedKey::ArrowUp) => LogicalKey::Named(RuffleNamedKey::ArrowUp),
        Key::Named(NamedKey::ArrowDown) => LogicalKey::Named(RuffleNamedKey::ArrowDown),
        Key::Named(NamedKey::ArrowLeft) => LogicalKey::Named(RuffleNamedKey::ArrowLeft),
        Key::Named(NamedKey::ArrowRight) => LogicalKey::Named(RuffleNamedKey::ArrowRight),
        _ => LogicalKey::Unknown,
    };
    let key_location = match event.location {
        WinitKeyLocation::Standard => KeyLocation::Standard,
        WinitKeyLocation::Left => KeyLocation::Left,
        WinitKeyLocation::Right => KeyLocation::Right,
        WinitKeyLocation::Numpad => KeyLocation::Numpad,
    };
    KeyDescriptor {
        physical_key: RufflePhysicalKey::Unknown,
        logical_key,
        key_location,
    }
}
