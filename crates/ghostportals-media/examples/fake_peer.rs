//! Stands in for the Windows daemon while developing the Linux side (or the
//! other way round).
//!
//!   cargo run -p ghostportals-media --example fake_peer -- listen 0.0.0.0:47800 [dump.h264]
//!   cargo run -p ghostportals-media --example fake_peer -- sweep 127.0.0.1:47800 800x600
//!
//! `listen` prints every message the peer sends, counts its video frames and
//! optionally writes the first stream to a raw H.264 file. With
//! `FAKE_PROXY_MOVE=x,y` set it also pretends the user dragged its proxy to
//! canvas `(x, y)` once the first handoff arrives. `sweep` connects
//! and slides a pretend Windows window of the given size across the bezel and
//! back (pausing a few seconds halfway), streaming a test pattern as its
//! contents.

use std::io::Write;
use std::time::Duration;

use ghostportals_core::protocol::{Direction, EdgeDelta, SyncPacket, WindowDim};
use ghostportals_core::{
    Endpoint, Hello, Link, LinkEvent, Message, PROTOCOL_VERSION, Side, StreamInfo,
};
use ghostportals_media::{Encoder, Source};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_env_filter("info").init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let usage = "usage: fake_peer listen <addr> [dump.h264] | sweep <host:port> <W>x<H>";
    let (mode, addr) = match args.as_slice() {
        [m, a, ..] => (m.as_str(), a.clone()),
        _ => anyhow::bail!("{usage}"),
    };
    let hello = Hello {
        protocol_version: PROTOCOL_VERSION,
        side: Side::Windows,
        host: "fake-peer".into(),
        token: std::env::var("GHOSTPORTALS_TOKEN").ok(),
    };

    match mode {
        "listen" => {
            let mut dump = args.get(2).map(std::fs::File::create).transpose()?;
            let mut link = Link::spawn(Endpoint::Listen(addr.parse()?), hello, None).await?;
            let mut proxy_move = std::env::var("FAKE_PROXY_MOVE").ok().and_then(|v| {
                let (x, y) = v.split_once(',')?;
                Some((x.parse::<i32>().ok()?, y.parse::<i32>().ok()?))
            });
            let (mut frames, mut keyframes, mut bytes) = (0u64, 0u64, 0usize);
            while let Some(ev) = link.recv().await {
                match ev {
                    LinkEvent::Video(f) => {
                        frames += 1;
                        keyframes += f.keyframe as u64;
                        bytes += f.data.len();
                        if let Some(d) = &mut dump {
                            d.write_all(&f.data)?;
                        }
                        if frames % 60 == 0 {
                            println!(
                                "# video: {frames} frames, {keyframes} keyframes, {bytes} bytes"
                            );
                        }
                    }
                    LinkEvent::Message(m) => {
                        println!("{}", serde_json::to_string(&m)?);
                        if let (Message::Sync(p), Some((x, y))) = (&m, proxy_move) {
                            println!("# moving the proxy of {} to {x},{y}", p.session_id);
                            link.send(Message::ProxyMoved {
                                session_id: p.session_id.clone(),
                                x,
                                y,
                            });
                            proxy_move = None;
                        }
                    }
                    other => println!("# {other:?}"),
                }
            }
        }
        "sweep" => {
            ghostportals_media::init()?;
            let (w, h) = args
                .get(2)
                .and_then(|s| s.split_once('x'))
                .map(|(w, h)| (w.parse().unwrap(), h.parse().unwrap()))
                .unwrap_or((800, 600));
            let mut link = Link::spawn(Endpoint::Connect(addr), hello, None).await?;
            while !matches!(link.recv().await, Some(LinkEvent::Connected(_))) {}
            link.send(Message::StreamStart(StreamInfo {
                stream: 1,
                session_id: "fake-1".into(),
                codec: "h264".into(),
                width: w,
                height: h,
                title: "Fake peer test pattern".into(),
            }));
            let encoder = Encoder::start(Source::Test, w, h, 1, link.video_sender(), || {})?;
            let steps: Vec<i32> = (0..=w).step_by(10).chain((0..=w).rev().step_by(10)).collect();
            for (i, visible) in steps.iter().copied().enumerate() {
                // Hold halfway across for a few seconds, for inspection.
                let hold = i == steps.len() / 4;
                link.send(Message::Sync(SyncPacket {
                    session_id: "fake-1".into(),
                    direction: Direction::WindowsToLinux,
                    is_dragging: true,
                    window_dim: WindowDim { width: w, height: h },
                    edge_delta: EdgeDelta { visible_pixels: visible, offset_y: 200 },
                }));
                let wait = Duration::from_millis(if hold { 4000 } else { 16 });
                let until = tokio::time::Instant::now() + wait;
                while let Ok(Some(ev)) = tokio::time::timeout_at(until, link.recv()).await {
                    if let LinkEvent::Message(Message::KeyframeRequest { .. }) = ev {
                        encoder.force_keyframe();
                    }
                }
            }
            link.send(Message::StreamEnd { stream: 1 });
            link.send(Message::Release { session_id: "fake-1".into() });
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        _ => anyhow::bail!("{usage}"),
    }
    Ok(())
}
