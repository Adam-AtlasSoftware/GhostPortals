//! Stands in for the Windows daemon while developing the Linux side (or the
//! other way round).
//!
//!   cargo run -p ghostportals-core --example fake_peer -- listen 0.0.0.0:47800
//!   cargo run -p ghostportals-core --example fake_peer -- sweep 127.0.0.1:47800 800x600
//!
//! `listen` prints every message the peer sends. `sweep` connects and slides
//! a pretend Windows window of the given size across the bezel and back.

use std::time::Duration;

use ghostportals_core::protocol::{Direction, EdgeDelta, SyncPacket, WindowDim};
use ghostportals_core::{Endpoint, Hello, Link, LinkEvent, Message, PROTOCOL_VERSION, Side};

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let usage = "usage: fake_peer listen <addr> | sweep <host:port> <W>x<H>";
    let (mode, addr) = match args.as_slice() {
        [m, a, ..] => (m.as_str(), a.clone()),
        _ => panic!("{usage}"),
    };
    let hello = Hello {
        protocol_version: PROTOCOL_VERSION,
        side: Side::Windows,
        host: "fake-peer".into(),
        token: std::env::var("GHOSTPORTALS_TOKEN").ok(),
    };

    match mode {
        "listen" => {
            let mut link =
                Link::spawn(Endpoint::Listen(addr.parse().expect("socket address")), hello, None);
            while let Some(ev) = link.recv().await {
                match ev {
                    LinkEvent::Message(m) => println!("{}", serde_json::to_string(&m).unwrap()),
                    other => println!("# {other:?}"),
                }
            }
        }
        "sweep" => {
            let (w, h) = args
                .get(2)
                .and_then(|s| s.split_once('x'))
                .map(|(w, h)| (w.parse().unwrap(), h.parse().unwrap()))
                .unwrap_or((800, 600));
            let mut link = Link::spawn(Endpoint::Connect(addr), hello, None);
            while !matches!(link.recv().await, Some(LinkEvent::Connected(_))) {}
            let steps: Vec<i32> = (0..=w).step_by(20).chain((0..=w).rev().step_by(20)).collect();
            for visible in steps {
                link.send(Message::Sync(SyncPacket {
                    session_id: "fake-1".into(),
                    direction: Direction::WindowsToLinux,
                    is_dragging: true,
                    window_dim: WindowDim { width: w, height: h },
                    edge_delta: EdgeDelta { visible_pixels: visible, offset_y: 200 },
                }));
                tokio::time::sleep(Duration::from_millis(16)).await;
            }
            link.send(Message::Release { session_id: "fake-1".into() });
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        _ => panic!("{usage}"),
    }
}
