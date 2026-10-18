//! Shared pieces of Ghost Portals: the wire protocol, the unified virtual canvas
//! and the handoff tracker, and the peer link that carries messages between the
//! Linux and Windows daemons.

pub mod geometry;
pub mod handoff;
pub mod keys;
pub mod link;
pub mod protocol;

pub use geometry::{Rect, ScreenEdge, Side};
pub use handoff::{HandoffLog, HandoffTracker};
pub use link::{Endpoint, Link, LinkEvent, VideoFrame, VideoSender};
pub use protocol::{
    Direction, EdgeDelta, Hello, InputEvent, Message, PROTOCOL_VERSION, PointerButton, StreamInfo,
    SyncPacket, WindowDim,
};
