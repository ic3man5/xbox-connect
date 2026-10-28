//! Minimal window host: winit + glutin (WGL) + egui_glow, instead of eframe.
//! Saves ~400 KB (no clipboard/links/PNG/image code) and gives us direct control over
//! visibility: the window is created hidden, hides on close, and is shown via an event
//! sent to the UI thread (so winit's own state always matches).

use std::num::NonZeroU32;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use egui_glow::glow;
use glutin::config::ConfigTemplateBuilder;
use glutin::context::{ContextApi, ContextAttributesBuilder, NotCurrentGlContext, PossiblyCurrentContext};
use glutin::display::{Display, DisplayApiPreference, GlDisplay};
use glutin::surface::{GlSurface, Surface, SurfaceAttributesBuilder, SwapInterval, WindowSurface};
use winit::application::ApplicationHandler;
use winit::event::{StartCause, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawWindowHandle};
use winit::window::{Icon, Window, WindowAttributes, WindowId};

use crate::bt::Shared;

#[derive(Debug)]
pub enum UserEvent {
    Redraw(Duration),
    Show,
}

static PROXY: OnceLock<Mutex<EventLoopProxy<UserEvent>>> = OnceLock::new();

/// Asks the UI thread to show and focus the window. Returns false if there is no UI.
pub fn request_show() -> bool {
    PROXY.get().is_some_and(|p| p.lock().unwrap_or_else(|e| e.into_inner()).send_event(UserEvent::Show).is_ok())
}

/// Field order matters: fields drop top to bottom, and glutin needs the window to outlive
/// both the surface and the context.
struct Gl {
    surface: Surface<WindowSurface>,
    context: PossiblyCurrentContext,
    window: Window,
}

impl Gl {
    /// Creates the window and a WGL context. Every failure is returned as an error (never a
    /// panic) so the app can fall back to running tray-only.
    fn new(event_loop: &ActiveEventLoop, attrs: WindowAttributes) -> Result<(Self, glow::Context), String> {
        let window = event_loop.create_window(attrs).map_err(|e| format!("cannot create window: {e}"))?;
        let raw_window = window.window_handle().map_err(|e| e.to_string())?.as_raw();
        let raw_display = event_loop.display_handle().map_err(|e| e.to_string())?.as_raw();
        let display = unsafe { Display::new(raw_display, DisplayApiPreference::Wgl(Some(raw_window))) }
            .map_err(|e| format!("no OpenGL display: {e}"))?;
        let template = ConfigTemplateBuilder::new()
            .compatible_with_native_window(raw_window)
            .prefer_hardware_accelerated(None)
            .with_depth_size(0)
            .with_stencil_size(0)
            .build();
        let config = unsafe { display.find_configs(template) }
            .map_err(|e| format!("no OpenGL pixel format: {e}"))?
            .next()
            .ok_or("no OpenGL pixel format")?;
        let core = ContextAttributesBuilder::new().build(Some(raw_window));
        let gles = ContextAttributesBuilder::new().with_context_api(ContextApi::Gles(None)).build(Some(raw_window));
        let not_current =
            unsafe { display.create_context(&config, &core).or_else(|_| display.create_context(&config, &gles)) }
                .map_err(|e| format!("cannot create OpenGL context: {e}"))?;
        let size = window.inner_size();
        let surface_attrs = SurfaceAttributesBuilder::<WindowSurface>::new().build(
            raw_window,
            NonZeroU32::new(size.width).unwrap_or(NonZeroU32::MIN),
            NonZeroU32::new(size.height).unwrap_or(NonZeroU32::MIN),
        );
        let surface = unsafe { display.create_window_surface(&config, &surface_attrs) }
            .map_err(|e| format!("cannot create surface: {e}"))?;
        let context = not_current.make_current(&surface).map_err(|e| format!("cannot make context current: {e}"))?;
        let _ = surface.set_swap_interval(&context, SwapInterval::Wait(NonZeroU32::MIN));
        let gl = unsafe {
            glow::Context::from_loader_function(|s| {
                let s = std::ffi::CString::new(s).unwrap_or_default();
                display.get_proc_address(&s)
            })
        };
        // egui_glow panics on OpenGL < 2.0 (e.g. Windows' "GDI Generic" 1.1 renderer in some
        // RDP sessions/VMs); check first so we can fall back gracefully.
        let v = {
            use glow::HasContext as _;
            gl.version().clone()
        };
        if v.major < 2 {
            return Err(format!("OpenGL {}.{} is too old (2.0 or newer is needed)", v.major, v.minor));
        }
        Ok((Self { surface, context, window }, gl))
    }
}
struct Host {
    shared: Arc<Shared>,
    start_hidden: bool,
    gl: Option<Gl>,
    egui: Option<egui_glow::EguiGlow>,
    app: Option<crate::App>,
    /// Show the window once the first frame is painted (avoids a white flash).
    show_after_paint: bool,
    /// Whether at least one frame has been presented.
    painted: bool,
    repaint_at: Option<Instant>,
    error: Option<String>,
}

impl Host {
    fn redraw(&mut self) {
        let (Some(gl), Some(egui), Some(app)) = (&self.gl, &mut self.egui, &mut self.app) else { return };
        // Never render while hidden; showing triggers a fresh redraw.
        if !self.show_after_paint && !gl.window.is_visible().unwrap_or(true) {
            return;
        }
        egui.run(&gl.window, |ctx| app.ui(ctx));
        unsafe {
            use glow::HasContext as _;
            let gl_ctx = egui.painter.gl();
            let bg = egui::Rgba::from(crate::theme::BG);
            gl_ctx.clear_color(bg.r(), bg.g(), bg.b(), 1.0);
            gl_ctx.clear(glow::COLOR_BUFFER_BIT);
        }
        egui.paint(&gl.window);
        let _ = gl.surface.swap_buffers(&gl.context);
        self.painted = true;
        if self.show_after_paint {
            self.show_after_paint = false;
            gl.window.set_visible(true);
            gl.window.focus_window();
        }
    }
}

impl ApplicationHandler<UserEvent> for Host {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.gl.is_some() || self.error.is_some() {
            return;
        }
        let icon = Icon::from_rgba(crate::icon::rgba(64, crate::icon::ACTIVE), 64, 64).ok();
        let attrs = Window::default_attributes()
            .with_title(crate::APP_NAME)
            .with_inner_size(winit::dpi::LogicalSize::new(920.0, 640.0))
            .with_min_inner_size(winit::dpi::LogicalSize::new(820.0, 560.0))
            .with_window_icon(icon)
            .with_visible(false);
        let (gl, glow_ctx) = match Gl::new(event_loop, attrs) {
            Ok(v) => v,
            Err(e) => {
                // No OpenGL (e.g. some RDP sessions): keep running tray-only.
                crate::report_ui_failure(&self.shared, &e);
                self.error = Some(e);
                return;
            }
        };
        if let Ok(h) = gl.window.window_handle()
            && let RawWindowHandle::Win32(w) = h.as_raw()
        {
            self.shared.hwnd.store(w.hwnd.get(), std::sync::atomic::Ordering::Relaxed);
            crate::dark_title_bar(w.hwnd.get());
        }
        let egui =
            egui_glow::EguiGlow::new(event_loop, Arc::new(glow_ctx), None, Some(gl.window.scale_factor() as f32), true);
        crate::theme::apply(&egui.egui_ctx);
        if let Some(p) = PROXY.get() {
            let p = Mutex::new(p.lock().unwrap_or_else(|e| e.into_inner()).clone());
            egui.egui_ctx.set_request_repaint_callback(move |info| {
                let _ = p.lock().unwrap_or_else(|e| e.into_inner()).send_event(UserEvent::Redraw(info.delay));
            });
        }
        self.app = Some(crate::App::new(self.shared.clone(), egui.egui_ctx.clone()));
        self.gl = Some(gl);
        self.egui = Some(egui);
        if !self.start_hidden {
            // Windows sends no paint messages to a hidden window, so render the first frame
            // directly, then show it (no blank/black flash).
            self.show_after_paint = true;
            self.redraw();
        }
    }

    fn window_event(&mut self, _event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let (Some(gl), Some(egui)) = (&self.gl, &mut self.egui) else { return };
        match &event {
            // Closing hides to the tray; Quit lives in the tray menu.
            WindowEvent::CloseRequested => {
                gl.window.set_visible(false);
                return;
            }
            WindowEvent::RedrawRequested => {
                self.redraw();
                return;
            }
            WindowEvent::Resized(size) => {
                if let (Some(w), Some(h)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) {
                    gl.surface.resize(&gl.context, w, h);
                }
            }
            _ => {}
        }
        if egui.on_window_event(&gl.window, &event).repaint {
            gl.window.request_redraw();
        }
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: UserEvent) {
        let Some(gl) = &self.gl else { return };
        match event {
            UserEvent::Redraw(delay) if delay.is_zero() => gl.window.request_redraw(),
            UserEvent::Redraw(delay) => {
                let at = Instant::now().checked_add(delay);
                if let Some(at) = at {
                    self.repaint_at = Some(self.repaint_at.map_or(at, |cur| cur.min(at)));
                }
            }
            UserEvent::Show => {
                self.shared.rescan.store(true, std::sync::atomic::Ordering::Relaxed);
                gl.window.set_minimized(false);
                if self.painted {
                    gl.window.set_visible(true);
                    gl.window.focus_window();
                    gl.window.request_redraw();
                } else {
                    // Never shown yet (started with --minimized): render directly, then show.
                    self.show_after_paint = true;
                    self.redraw();
                }
            }
        }
    }

    fn new_events(&mut self, _event_loop: &ActiveEventLoop, cause: StartCause) {
        if let StartCause::ResumeTimeReached { .. } = cause {
            self.repaint_at = None;
            if let Some(gl) = &self.gl {
                gl.window.request_redraw();
            }
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        event_loop.set_control_flow(match self.repaint_at {
            Some(at) => ControlFlow::WaitUntil(at),
            None => ControlFlow::Wait,
        });
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        if let Some(egui) = &mut self.egui {
            egui.destroy();
        }
    }
}

/// Runs the UI on this thread until the process exits. If OpenGL is unavailable the loop
/// keeps running without a window (the tray still works) and the failure is reported once.
pub fn run(shared: Arc<Shared>, start_hidden: bool) -> Result<(), String> {
    let event_loop = EventLoop::<UserEvent>::with_user_event().build().map_err(|e| e.to_string())?;
    let _ = PROXY.set(Mutex::new(event_loop.create_proxy()));
    let mut host = Host {
        shared,
        start_hidden,
        gl: None,
        egui: None,
        app: None,
        show_after_paint: false,
        painted: false,
        repaint_at: None,
        error: None,
    };
    event_loop.run_app(&mut host).map_err(|e| e.to_string())
}
