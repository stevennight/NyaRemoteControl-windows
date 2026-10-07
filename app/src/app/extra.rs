//! Extra session windows: each shows one more host display (physical or
//! virtual) at the same time as the main window, with its own stream
//! (slot), decoder, renderer and a small toolbar. Keyboard input goes to the
//! host from any of them; the mouse maps to the display the window shows.

use std::collections::HashSet;
use std::sync::Arc;

use nya_proto::pb::{self, input_msg::Ev};
use nya_ui::Gui;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::ActiveEventLoop;
use winit::window::{Fullscreen, Window, WindowId};

use super::{hwnd, App};
use crate::render::{fit, Renderer};
use crate::ui::{self, ExtraAction};
use crate::input;

pub(super) struct ExtraWindow {
    pub slot: u32,
    pub window: Arc<Window>,
    renderer: Renderer,
    gui: Gui,
    remote_buttons: u8,
    pub fullscreen: bool,
    focused: bool,
}

/// Displays the "one window per display" setting already opened this session
/// (a window the user closed is not opened again).
#[derive(Default)]
pub(super) struct AutoOpened(HashSet<u32>);

fn button_of(b: &MouseButton) -> Option<(pb::MouseButton, u8)> {
    Some(match b {
        MouseButton::Left => (pb::MouseButton::Left, 1),
        MouseButton::Right => (pb::MouseButton::Right, 2),
        MouseButton::Middle => (pb::MouseButton::Middle, 4),
        MouseButton::Back => (pb::MouseButton::X1, 8),
        MouseButton::Forward => (pb::MouseButton::X2, 16),
        MouseButton::Other(_) => return None,
    })
}

impl ExtraWindow {
    pub fn focused(&self) -> bool {
        self.focused
    }

    /// Follow the window's monitor into or out of HDR mode.
    pub fn refresh_display(&mut self) {
        if self.renderer.refresh_display() {
            self.window.request_redraw();
        }
    }
}

impl App {
    /// Open (or bring forward) a window for host display `display_id`.
    pub(super) fn open_extra(&mut self, el: &ActiveEventLoop, display_id: u32) {
        let Some(s) = self.session.as_mut() else { return };
        if !s.multi_supported {
            s.notice("被控端版本太旧，不支持多窗口".into(), std::time::Duration::from_secs(5));
            return;
        }
        if let Some(slot) = s.view_of(display_id) {
            if let Some(w) = self.extras.values().find(|w| w.slot == slot) {
                w.window.focus_window();
            }
            return;
        }
        let Some(dev) = self.renderer.as_ref().map(|r| r.dev.clone()) else { return };
        let title = format!("{} · {} — NyaRemoteControl", s.label, s.display_title(display_id));
        use winit::platform::windows::WindowAttributesExtWindows;
        let attrs = Window::default_attributes().with_class_name(input::SESSION_CLASS).with_window_icon(super::app_icon()).with_title(title).with_inner_size(LogicalSize::new(960.0, 600.0)).with_min_inner_size(LogicalSize::new(320.0, 200.0));
        let window = match el.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => return tracing::warn!("extra window: {e}"),
        };
        let Some(h) = hwnd(&window) else { return };
        let size = window.inner_size();
        let (renderer, gui) = match (Renderer::new(dev.clone(), h, size.width, size.height), Gui::new(&window, &dev)) {
            (Ok(r), Ok(g)) => (r, g),
            (Err(e), _) | (_, Err(e)) => return tracing::warn!("extra window renderer: {e:#}"),
        };
        input::set_extra_window(h, true);
        let slot = s.open_view(display_id);
        self.extras.insert(window.id(), ExtraWindow { slot, window: window.clone(), renderer, gui, remote_buttons: 0, fullscreen: false, focused: true });
        window.request_redraw();
        // A virtual screen shown here takes this window's size.
        self.schedule_vd_fit();
    }

    pub(super) fn close_extra(&mut self, id: WindowId) {
        let Some(w) = self.extras.remove(&id) else { return };
        if let Some(h) = hwnd(&w.window) {
            input::set_extra_window(h, false);
        }
        if let Some(s) = self.session.as_mut() {
            s.close_view(w.slot);
        }
    }

    /// Session over: every extra window goes.
    pub(super) fn close_all_extras(&mut self) {
        let ids: Vec<WindowId> = self.extras.keys().copied().collect();
        for id in ids {
            self.close_extra(id);
        }
        self.auto_opened = AutoOpened::default();
    }

    pub(super) fn extra_of_slot(&self, slot: u32) -> Option<WindowId> {
        self.extras.iter().find(|(_, w)| w.slot == slot).map(|(id, _)| *id)
    }

    /// The main window's GPU changed: the extra windows follow (decoders
    /// produce frames on the new device).
    pub(super) fn rebuild_extras(&mut self) {
        let Some(dev) = self.renderer.as_ref().map(|r| r.dev.clone()) else { return };
        for w in self.extras.values_mut() {
            let Some(h) = hwnd(&w.window) else { continue };
            let size = w.window.inner_size();
            match Renderer::new(dev.clone(), h, size.width, size.height) {
                Ok(r) => w.renderer = r,
                Err(e) => tracing::warn!("extra window renderer: {e:#}"),
            }
            if let Err(e) = w.gui.set_device(&dev) {
                tracing::warn!("extra window gui: {e:#}");
            }
            if let Some(v) = self.session.as_mut().and_then(|s| s.views.get_mut(&w.slot)) {
                v.current = None;
            }
        }
    }

    /// A window showing a virtual screen changed size: resize the screens soon
    /// (after the drag ends: an unusual size restarts the virtual display driver).
    pub(super) fn schedule_vd_fit(&mut self) {
        if self.session.as_ref().is_some_and(|s| s.vd_follow_window) {
            self.vd_resize_at = Some(std::time::Instant::now() + std::time::Duration::from_millis(800));
        }
    }

    /// Add a virtual screen to the host and show it in a new window.
    pub(super) fn new_virtual_window(&mut self) {
        let Some(s) = self.session.as_mut() else { return };
        let mut c = s.display_choice();
        if c.count >= 4 {
            s.notice("最多 4 个虚拟显示器".into(), std::time::Duration::from_secs(4));
            return;
        }
        c.count += 1;
        self.pending_virtual = Some(c.count);
        let setup = self.setup_request(c, self.fullscreen);
        if let Some(s) = self.session.as_mut() {
            s.set_display_setup(setup);
        }
    }

    /// Host displays changed: with "one window per display", open windows for
    /// new displays; windows whose display is gone close.
    pub(super) fn sync_extras(&mut self) {
        let Some(s) = self.session.as_ref() else { return };
        let Some(info) = &s.info else { return };
        let ids: Vec<u32> = info.displays.iter().map(|d| d.id).collect();
        let gone: Vec<WindowId> = self
            .extras
            .iter()
            .filter(|(_, w)| s.views.get(&w.slot).is_some_and(|v| !ids.contains(&v.display_id)))
            .map(|(id, _)| *id)
            .collect();
        for id in gone {
            self.close_extra(id);
        }
        let Some(s) = self.session.as_ref() else { return };
        // A virtual screen created for a new window has appeared: open it.
        if let Some(idx) = self.pending_virtual {
            if let Some(d) = s.info.as_ref().and_then(|i| i.displays.iter().find(|d| d.virtual_index == idx)) {
                self.pending_virtual = None;
                self.auto_opened.0.insert(d.id);
                self.open_requests.push(d.id);
            }
        }
        if !self.sd.multi_window || !s.multi_supported {
            return;
        }
        let main = s.stream.as_ref().map(|x| x.display_id);
        let Some(main) = main else { return }; // wait until the main window knows its display
        for id in ids {
            if id != main && s.view_of(id).is_none() && self.auto_opened.0.insert(id) {
                self.open_requests.push(id);
            }
        }
    }

    pub(super) fn extra_event(&mut self, id: WindowId, event: WindowEvent) {
        let Some(w) = self.extras.get_mut(&id) else { return };
        let window = w.window.clone();
        let keyboard = matches!(event, WindowEvent::KeyboardInput { .. } | WindowEvent::ModifiersChanged(_) | WindowEvent::Ime(_));
        if !keyboard && w.gui.on_event(&window, &event).repaint {
            window.request_redraw();
        }
        let over_ui = w.gui.ctx.is_pointer_over_area() || w.gui.ctx.is_using_pointer();
        match &event {
            WindowEvent::CloseRequested => self.close_extra(id),
            WindowEvent::Resized(size) => {
                if let Err(e) = w.renderer.resize(size.width, size.height) {
                    tracing::warn!("extra window resize: {e:#}");
                }
                window.request_redraw();
                self.schedule_vd_fit();
            }
            WindowEvent::RedrawRequested => self.draw_extra(id),
            WindowEvent::Focused(f) => {
                w.focused = *f;
                if *f && self.session.is_some() {
                    input::reinstall_hook();
                }
                if !*f {
                    w.remote_buttons = 0;
                    input::reset_modifiers();
                    if let Some(s) = &self.session {
                        s.release_all();
                    }
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                if !over_ui && w.focused {
                    let slot = w.slot;
                    let size = self.session.as_ref().and_then(|s| s.views.get(&slot)).and_then(|v| v.current.as_ref().map(|c| (c.width, c.height)));
                    if let (Some((vw, vh)), Some(s)) = (size, &self.session) {
                        let r = fit(w.renderer.width, w.renderer.height, vw, vh);
                        let nx = ((position.x - r.x) / r.w).clamp(0.0, 1.0);
                        let ny = ((position.y - r.y) / r.h).clamp(0.0, 1.0);
                        let (x, y) = ((nx * 65535.0).round() as u32, (ny * 65535.0).round() as u32);
                        s.send_input(Ev::MouseAbs(pb::MouseAbs { x, y, slot }));
                    }
                }
                if position.y < 60.0 || over_ui {
                    window.request_redraw();
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let Some((b, bit)) = button_of(button) else { return };
                let down = *state == ElementState::Pressed;
                let forward = if down { !over_ui } else { w.remote_buttons & bit != 0 };
                if forward {
                    if down {
                        w.remote_buttons |= bit;
                    } else {
                        w.remote_buttons &= !bit;
                    }
                    if let Some(s) = &self.session {
                        s.send_input(Ev::MouseButton(pb::MouseButtonEv { button: b as i32, down }));
                    }
                }
            }
            WindowEvent::MouseWheel { delta, .. } if !over_ui => {
                let (dx, dy) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => ((x * 120.0) as i32, (y * 120.0) as i32),
                    MouseScrollDelta::PixelDelta(p) => (p.x as i32, p.y as i32),
                };
                if dx != 0 || dy != 0 {
                    if let Some(s) = &self.session {
                        s.send_input(Ev::Wheel(pb::Wheel { dx, dy }));
                    }
                }
            }
            WindowEvent::KeyboardInput { event, is_synthetic, .. } => {
                // Same keys as the main window (hotkeys, keys the hook didn't take).
                if let Some(h) = self.hotkey_from_key(event) {
                    return self.hotkey(h);
                }
                if input::grabbed() {
                    if !is_synthetic {
                        input::key_missed_hook();
                    }
                    use winit::platform::scancode::PhysicalKeyExtScancode;
                    if let (Some(sc), Some(s)) = (event.physical_key.to_scancode(), &self.session) {
                        let (scancode, extended) = (sc & 0xff, sc & 0xff00 == 0xe000);
                        if scancode != 0 {
                            s.send_input(Ev::Key(pb::Key { scancode, extended, down: event.state == ElementState::Pressed }));
                        }
                    }
                }
            }
            _ => {}
        }
    }

    pub(super) fn draw_extra(&mut self, id: WindowId) {
        let Some(w) = self.extras.get_mut(&id) else { return };
        let Some(s) = self.session.as_mut() else { return };
        let title = s.display_title(s.views.get(&w.slot).map(|v| v.display_id).unwrap_or(0));
        let Some(v) = s.views.get_mut(&w.slot) else { return };
        let (slot, fresh) = v.store.take();
        if slot.is_some() {
            v.current = slot;
        }
        if fresh {
            // Latency needs the clock offset, which the main statistics keep.
            let lat = v.current.as_ref().map(|c| s.stats.latency_ms(c.capture_ts));
            v.stats.with(|st| {
                st.frames_rendered += 1;
                st.total_rendered += 1;
                if let Some(l) = lat {
                    st.latency_ms.push(l);
                }
            });
        }
        let status = v.status.clone();
        let current = v.current.clone();
        let window = w.window.clone();
        let fullscreen = w.fullscreen;
        let mut action = ExtraAction::None;
        let frame = w.gui.run(&window, |ctx| action = ui::extra_overlay(ctx, &title, &status, fullscreen));
        // egui set its own cursor (pointer came back, or left a button): the remote one again.
        if frame.cursor_set && !(w.gui.ctx.is_pointer_over_area() || w.gui.ctx.is_using_pointer()) {
            if let Some(v) = s.views.get(&w.slot) {
                if let Some(c) = s.cursors.get(&v.cursor_shape) {
                    window.set_cursor(c.clone());
                }
                window.set_cursor_visible(v.cursor_visible);
            }
        }
        let res = (|| -> anyhow::Result<()> {
            let rtv = w.renderer.begin()?;
            if let Some(c) = &current {
                w.renderer.draw_video(c);
            }
            w.gui.paint(&rtv, (w.renderer.width, w.renderer.height), &frame)?;
            w.renderer.present(false)
        })();
        if let Err(e) = res {
            tracing::warn!("extra window render: {e:#}");
        }
        if frame.repaint_after < std::time::Duration::from_secs(1) {
            window.request_redraw();
        }
        match action {
            ExtraAction::None => {}
            ExtraAction::ToggleFullscreen => {
                w.fullscreen = !w.fullscreen;
                window.set_fullscreen(w.fullscreen.then_some(Fullscreen::Borderless(None)));
            }
            ExtraAction::Close => self.close_extra(id),
        }
    }
}
