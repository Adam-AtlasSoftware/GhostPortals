//! Turns a stream of local window samples into the messages the other side
//! needs to mirror the crossing.

use crate::geometry::{Rect, ScreenEdge};
use crate::protocol::{EdgeDelta, Message, SyncPacket, WindowDim};

#[derive(Debug, Clone, PartialEq)]
struct Active {
    session_id: String,
    window_id: String,
    last: SyncPacket,
}

/// Tracks the one window currently being handed across the bezel.
#[derive(Debug)]
pub struct HandoffTracker {
    edge: ScreenEdge,
    next_session: u64,
    active: Option<Active>,
}

impl HandoffTracker {
    pub fn new(edge: ScreenEdge) -> Self {
        Self { edge, next_session: 1, active: None }
    }

    pub fn edge(&self) -> ScreenEdge {
        self.edge
    }

    pub fn set_edge(&mut self, edge: ScreenEdge) {
        self.edge = edge;
    }

    /// Feeds the latest local geometry of `window_id`. Returns the message to
    /// send, if anything changed.
    pub fn sample(&mut self, window_id: &str, local: Rect, dragging: bool) -> Option<Message> {
        let canvas = self.edge.to_canvas(local);
        let overhang = self.edge.overhang(canvas);

        // Another window crossing replaces the session; the receiver sees the
        // new session id and moves its proxy over.
        if self.active.as_ref().is_some_and(|a| a.window_id != window_id) {
            overhang?;
            self.active = None;
        }

        let Some(delta) = overhang else {
            let a = self.active.take()?;
            return Some(Message::Release { session_id: a.session_id });
        };
        let session_id = match &self.active {
            Some(a) => a.session_id.clone(),
            None => {
                let id = format!("handoff-{}", self.next_session);
                self.next_session += 1;
                id
            }
        };
        let packet = self.packet(&session_id, canvas, delta, dragging);
        if self.active.as_ref().is_some_and(|a| a.last == packet) {
            return None;
        }
        self.active =
            Some(Active { session_id, window_id: window_id.to_owned(), last: packet.clone() });
        Some(Message::Sync(packet))
    }

    /// The tracked window went away.
    pub fn removed(&mut self, window_id: &str) -> Option<Message> {
        if self.active.as_ref()?.window_id != window_id {
            return None;
        }
        let a = self.active.take()?;
        Some(Message::Release { session_id: a.session_id })
    }

    fn packet(
        &self,
        session_id: &str,
        canvas: Rect,
        delta: EdgeDelta,
        dragging: bool,
    ) -> SyncPacket {
        SyncPacket {
            session_id: session_id.to_owned(),
            direction: self.edge.outgoing(),
            is_dragging: dragging,
            window_dim: WindowDim { width: canvas.width, height: canvas.height },
            edge_delta: delta,
        }
    }
}

/// Logs handoffs at a readable rate: one line when a session starts or ends,
/// with the per-frame packets left to debug level.
#[derive(Debug, Default)]
pub struct HandoffLog {
    outgoing: Option<String>,
    incoming: Option<String>,
}

impl HandoffLog {
    pub fn outgoing(&mut self, msg: &Message) {
        Self::log(&mut self.outgoing, "sent", msg);
    }

    pub fn incoming(&mut self, msg: &Message) {
        Self::log(&mut self.incoming, "received", msg);
    }

    fn log(current: &mut Option<String>, verb: &str, msg: &Message) {
        match msg {
            Message::Sync(p) if current.as_deref() != Some(&p.session_id) => {
                tracing::info!(
                    "{verb} handoff {} {:?}: {}x{} window, {} px across",
                    p.session_id,
                    p.direction,
                    p.window_dim.width,
                    p.window_dim.height,
                    p.edge_delta.visible_pixels
                );
                *current = Some(p.session_id.clone());
            }
            Message::Sync(p) => tracing::debug!(
                "{verb} {} px across, dragging={}",
                p.edge_delta.visible_pixels,
                p.is_dragging
            ),
            Message::Release { session_id } => {
                tracing::info!("{verb} release of handoff {session_id}");
                *current = None;
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Direction, Side};

    fn tracker() -> HandoffTracker {
        HandoffTracker::new(ScreenEdge::from_desktop(Side::Linux, Rect::new(0, 0, 1920, 1080)))
    }

    fn visible(msg: &Message) -> i32 {
        match msg {
            Message::Sync(p) => p.edge_delta.visible_pixels,
            other => panic!("expected sync, got {other:?}"),
        }
    }

    #[test]
    fn drag_across_and_back() {
        let mut t = tracker();
        assert_eq!(t.sample("a", Rect::new(10, 0, 800, 600), true), None);

        let first = t.sample("a", Rect::new(-100, 0, 800, 600), true).unwrap();
        assert_eq!(visible(&first), 100);
        let Message::Sync(p) = &first else { unreachable!() };
        assert_eq!(p.direction, Direction::LinuxToWindows);
        assert_eq!(p.session_id, "handoff-1");

        // Same geometry again: nothing to say.
        assert_eq!(t.sample("a", Rect::new(-100, 0, 800, 600), true), None);

        assert_eq!(visible(&t.sample("a", Rect::new(-300, 0, 800, 600), true).unwrap()), 300);

        // Drag ends while still crossing: the receiver needs to know.
        let Some(Message::Sync(p)) = t.sample("a", Rect::new(-300, 0, 800, 600), false) else {
            panic!()
        };
        assert!(!p.is_dragging);

        assert_eq!(
            t.sample("a", Rect::new(5, 0, 800, 600), true),
            Some(Message::Release { session_id: "handoff-1".into() })
        );

        let Some(Message::Sync(p)) = t.sample("a", Rect::new(-1, 0, 800, 600), true) else {
            panic!()
        };
        assert_eq!(p.session_id, "handoff-2");
    }

    #[test]
    fn other_windows_do_not_steal_the_session() {
        let mut t = tracker();
        t.sample("a", Rect::new(-50, 0, 800, 600), true).unwrap();
        assert_eq!(t.sample("b", Rect::new(100, 0, 800, 600), false), None);
        assert_eq!(t.removed("b"), None);
        assert_eq!(t.removed("a"), Some(Message::Release { session_id: "handoff-1".into() }));
    }
}
