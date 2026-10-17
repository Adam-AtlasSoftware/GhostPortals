//! Messages exchanged between the two daemons.
//!
//! Each frame on the wire is a 4-byte big-endian length followed by one JSON
//! encoded [`Message`]. JSON keeps the stream inspectable while the protocol is
//! young; the framing lets the body encoding change later without touching the
//! transport. Unknown fields are ignored so newer peers can add data freely;
//! anything incompatible bumps [`PROTOCOL_VERSION`].

use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u16 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Message {
    /// First frame in each direction after connecting.
    Hello(Hello),
    /// Live geometry of a window crossing the bezel.
    Sync(SyncPacket),
    /// The window has fully retreated to its own side; hide the proxy.
    Release {
        session_id: String,
    },
    Ping {
        seq: u64,
    },
    Pong {
        seq: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hello {
    pub protocol_version: u16,
    pub side: crate::Side,
    pub host: String,
    /// Shared secret; checked when the receiving side was started with one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Direction {
    /// A Linux window is sliding onto the Windows screen.
    #[serde(rename = "L_TO_W")]
    LinuxToWindows,
    /// A Windows window is sliding onto the Linux screen.
    #[serde(rename = "W_TO_L")]
    WindowsToLinux,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SyncPacket {
    pub session_id: String,
    pub direction: Direction,
    pub is_dragging: bool,
    pub window_dim: WindowDim,
    pub edge_delta: EdgeDelta,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowDim {
    pub width: i32,
    pub height: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EdgeDelta {
    /// How many pixels of the window have crossed the bezel.
    pub visible_pixels: i32,
    /// Top of the window relative to the top of the screens.
    pub offset_y: i32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_matches_spec_shape() {
        let msg = Message::Sync(SyncPacket {
            session_id: "active_handoff_01".into(),
            direction: Direction::LinuxToWindows,
            is_dragging: true,
            window_dim: WindowDim { width: 1280, height: 720 },
            edge_delta: EdgeDelta { visible_pixels: 145, offset_y: 320 },
        });
        let json: serde_json::Value = serde_json::to_value(&msg).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "type": "sync",
                "session_id": "active_handoff_01",
                "direction": "L_TO_W",
                "is_dragging": true,
                "window_dim": { "width": 1280, "height": 720 },
                "edge_delta": { "visible_pixels": 145, "offset_y": 320 }
            })
        );
        assert_eq!(serde_json::from_value::<Message>(json).unwrap(), msg);
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let msg: Message =
            serde_json::from_str(r#"{"type":"ping","seq":3,"added_in_v2":true}"#).unwrap();
        assert_eq!(msg, Message::Ping { seq: 3 });
    }
}
