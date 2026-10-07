//! Cursor forwarding: shapes are sent once (by content hash id), positions
//! and visibility as small state updates.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};

use nya_proto::pb::{self, cursor_msg::Msg};
use nya_win::duplication::{CursorShape, PointerUpdate};

#[derive(Default)]
pub struct CursorTracker {
    sent_shapes: HashSet<u32>,
    shape: Option<(u32, i32, i32)>, // id, hot_x, hot_y
    last_state: Option<pb::CursorState>,
    raw_pos: Option<(i32, i32, bool)>,
    log: CursorLog,
}

/// Cursor events in the helper log: the first ones one by one, then a
/// summary now and then (the pointer vanishing on clients is chased here).
#[derive(Default)]
struct CursorLog {
    lines: u32,
    hides: u32,
    shows: u32,
    shapes: u32,
    since: Option<Instant>,
    no_shape_logged: bool,
}

const LOG_LINES: u32 = 200;

impl CursorLog {
    fn line(&mut self) -> bool {
        self.lines += 1;
        self.lines <= LOG_LINES
    }

    fn summary(&mut self, state: Option<&pb::CursorState>) {
        let since = *self.since.get_or_insert_with(Instant::now);
        if self.lines < LOG_LINES || since.elapsed() < Duration::from_secs(10) {
            return;
        }
        if self.hides + self.shows + self.shapes > 0 {
            tracing::info!(
                "cursor (10 s): {} hidden, {} shown, {} new shapes; now {}",
                self.hides,
                self.shows,
                self.shapes,
                state.map_or("no state".to_string(), |s| format!("visible={} shape {:08x}", s.visible, s.shape_id))
            );
        }
        (self.hides, self.shows, self.shapes, self.since) = (0, 0, 0, Some(Instant::now()));
    }
}

/// Opaque pixels of a straight-alpha RGBA image (a cursor with none is invisible).
pub fn opaque_pixels(rgba: &[u8]) -> usize {
    rgba.chunks_exact(4).filter(|p| p[3] != 0).count()
}

fn shape_id(s: &CursorShape) -> u32 {
    let mut h = DefaultHasher::new();
    (s.width, s.height, s.hot_x, s.hot_y).hash(&mut h);
    s.rgba.hash(&mut h);
    (h.finish() as u32).max(1)
}

impl CursorTracker {
    /// Turn a DXGI pointer update into messages for the client.
    pub fn update(&mut self, p: PointerUpdate, out: &mut Vec<pb::CursorMsg>) {
        if let Some(s) = p.shape {
            let id = shape_id(&s);
            if self.shape.map(|(i, ..)| i) != Some(id) {
                self.log.shapes += 1;
                if self.log.line() {
                    tracing::info!(
                        "cursor shape {id:08x}: {}x{} type {} hot ({},{}) opaque {}",
                        s.width,
                        s.height,
                        s.kind,
                        s.hot_x,
                        s.hot_y,
                        opaque_pixels(&s.rgba)
                    );
                }
            }
            if self.sent_shapes.insert(id) {
                if self.sent_shapes.len() > 256 {
                    self.sent_shapes.clear();
                    self.sent_shapes.insert(id);
                }
                out.push(pb::CursorMsg {
                    msg: Some(Msg::Shape(pb::CursorShape {
                        id,
                        width: s.width,
                        height: s.height,
                        hot_x: s.hot_x,
                        hot_y: s.hot_y,
                        rgba: s.rgba,
                    })),
                });
            }
            self.shape = Some((id, s.hot_x, s.hot_y));
        }
        if let Some(pos) = p.position {
            self.raw_pos = Some(pos);
        }
        let (Some((id, hx, hy)), Some((x, y, visible))) = (self.shape, self.raw_pos) else {
            if self.raw_pos.is_some() && !self.log.no_shape_logged {
                self.log.no_shape_logged = true;
                tracing::info!("cursor: position without a shape yet; nothing sent");
            }
            return;
        };
        let state = pb::CursorState { shape_id: id, visible, x: x + hx, y: y + hy, slot: 0 };
        let was_visible = self.last_state.as_ref().map(|s| s.visible);
        if was_visible != Some(visible) {
            if visible {
                self.log.shows += 1;
            } else {
                self.log.hides += 1;
            }
            if self.log.line() {
                tracing::info!("cursor {} at ({x},{y}) shape {id:08x}", if visible { "shown" } else { "hidden" });
            }
        }
        self.log.summary(Some(&state));
        if self.last_state.as_ref() != Some(&state) {
            self.last_state = Some(state.clone());
            out.push(pb::CursorMsg { msg: Some(Msg::State(state)) });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape() -> CursorShape {
        CursorShape { width: 2, height: 2, hot_x: 1, hot_y: 1, kind: 2, rgba: vec![255; 16] }
    }

    #[test]
    fn shape_sent_once_state_on_change() {
        let mut t = CursorTracker::default();
        let mut out = Vec::new();
        t.update(PointerUpdate { position: Some((10, 20, true)), shape: Some(shape()) }, &mut out);
        assert_eq!(out.len(), 2);
        match &out[1].msg {
            Some(Msg::State(s)) => assert_eq!((s.x, s.y, s.visible), (11, 21, true)),
            _ => panic!(),
        }
        out.clear();
        t.update(PointerUpdate { position: Some((10, 20, true)), shape: Some(shape()) }, &mut out);
        assert!(out.is_empty(), "nothing changed");
        t.update(PointerUpdate { position: Some((12, 20, true)), shape: None }, &mut out);
        assert_eq!(out.len(), 1);
    }
}
