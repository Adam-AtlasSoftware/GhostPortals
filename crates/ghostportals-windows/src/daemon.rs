use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use ghostportals_core::geometry::{Margins, proxy_clip, proxy_placement};
use ghostportals_core::{
    Direction, Endpoint, HandoffLog, HandoffTracker, Hello, Link, LinkEvent, Message,
    PROTOCOL_VERSION, Rect, ScreenEdge, Side, StreamInfo, SyncPacket, VideoFrame,
};
use ghostportals_media::{Decoder, DecoderSink, Encoder, PixelFormat, Source};
use tokio::sync::mpsc;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use crate::proxy::Proxy;
use crate::win32::{self, Window};

/// Polling the foreground window this often follows a drag at display rate.
const POLL: Duration = Duration::from_millis(16);

#[derive(Parser)]
#[command(version, about = "Ghost Portals daemon for the Windows side")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print the foreground window's geometry relative to the bezel.
    Watch,
    /// Run the handoff daemon.
    Run {
        /// The Linux daemon to dial (host:port).
        #[arg(long, required_unless_present = "listen")]
        connect: Option<String>,
        /// Accept the Linux daemon on this address instead of dialling.
        #[arg(long, conflicts_with = "connect")]
        listen: Option<SocketAddr>,
        /// Shared secret both daemons must agree on.
        #[arg(long, env = "GHOSTPORTALS_TOKEN")]
        token: Option<String>,
    },
    /// Check that this machine can encode and decode window streams.
    Selftest,
}

pub async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    if let Err(e) = win32::enable_per_monitor_dpi() {
        warn!("{e:#}; coordinates may be scaled");
    }
    match Cli::parse().command {
        Command::Watch => watch().await,
        Command::Selftest => {
            println!("{}", ghostportals_media::selftest(3)?);
            Ok(())
        }
        Command::Run { connect, listen, token } => {
            let endpoint = match (connect, listen) {
                (Some(addr), _) => Endpoint::Connect(addr),
                (None, Some(addr)) => Endpoint::Listen(addr),
                (None, None) => bail!("pass --connect or --listen"),
            };
            run(endpoint, token).await
        }
    }
}

fn edge() -> ScreenEdge {
    ScreenEdge::from_desktop(Side::Windows, win32::virtual_desktop())
}

async fn watch() -> Result<()> {
    let mut ticker = tokio::time::interval(POLL);
    let mut last = None;
    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            _ = tokio::signal::ctrl_c() => return Ok(()),
        }
        let Some(w) = Window::foreground() else { continue };
        let rect = match w.rect() {
            Ok(r) => r,
            Err(e) => {
                warn!("{e:#}");
                continue;
            }
        };
        let moving = w.in_move_size();
        let edge = edge();
        let sample = (w, rect, moving);
        if last == Some(sample) {
            continue;
        }
        last = Some(sample);
        let canvas = edge.to_canvas(rect);
        let overhang = edge.overhang(canvas).map_or(0, |d| d.visible_pixels);
        println!(
            "{} canvas {:>6},{:<6} {:>5}x{:<5} {} past bezel {:>5}px — {}",
            w.id(),
            canvas.x,
            canvas.y,
            canvas.width,
            canvas.height,
            if moving { "M" } else { "-" },
            overhang,
            w.title(),
        );
    }
}

/// The local window being streamed to the peer.
struct Outgoing {
    session_id: String,
    window: Window,
    info: StreamInfo,
    encoder: Encoder,
}

/// The peer's window being shown in the proxy.
struct Incoming {
    info: StreamInfo,
    decoder: Decoder,
    keyframe_seen: bool,
}

/// Reports from GStreamer threads back to the main loop.
enum Internal {
    EncoderFailed(u32),
    DecoderFailed(u32),
}

struct State {
    tracker: HandoffTracker,
    /// The local window currently crossing, if any.
    tracked: Option<Window>,
    log: HandoffLog,
    proxy: Option<Proxy>,
    outgoing: Option<Outgoing>,
    /// A session whose window could not be streamed; not retried every frame.
    failed_session: Option<String>,
    incoming: Option<Incoming>,
    /// The session the proxy currently stands in for.
    remote_session: Option<String>,
    /// The user is dragging the proxy; incoming placements would fight them.
    proxy_dragging: bool,
    next_stream: u32,
    internal: mpsc::UnboundedSender<Internal>,
}

async fn run(endpoint: Endpoint, token: Option<String>) -> Result<()> {
    ghostportals_media::init()?;
    let hello = Hello {
        protocol_version: PROTOCOL_VERSION,
        side: Side::Windows,
        host: std::env::var("COMPUTERNAME").unwrap_or_else(|_| "windows".into()),
        token: token.clone(),
    };
    let mut link = Link::spawn(endpoint, hello, token)
        .await
        .context("cannot listen for the peer (is another daemon already running?)")?;
    let proxy = Proxy::spawn()
        .map_err(|e| warn!("no proxy window, windows from Linux will not be shown: {e:#}"))
        .ok();
    let (internal_tx, mut internal_rx) = mpsc::unbounded_channel();
    let mut state = State {
        tracker: HandoffTracker::new(edge()),
        tracked: None,
        log: HandoffLog::default(),
        proxy,
        outgoing: None,
        failed_session: None,
        incoming: None,
        remote_session: None,
        proxy_dragging: false,
        next_stream: 1,
        internal: internal_tx,
    };
    let mut ticker = tokio::time::interval(POLL);

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                state.tracker.set_edge(edge());
                watch_proxy(&link, &mut state);
                if let Some(msg) = poll(&mut state) {
                    state.log.outgoing(&msg);
                    link.send(msg.clone());
                    update_outgoing(&link, &mut state, &msg);
                }
            }
            ev = link.recv() => match ev {
                Some(LinkEvent::Connected(peer)) => {
                    info!("handoff link up with {}", peer.host);
                    if let Some(out) = &state.outgoing {
                        link.send(Message::StreamStart(out.info.clone()));
                        out.encoder.force_keyframe();
                    }
                }
                Some(LinkEvent::Disconnected) => {
                    info!("handoff link down");
                    end_incoming(&mut state);
                }
                Some(LinkEvent::Message(msg)) => remote_message(&link, &mut state, msg),
                Some(LinkEvent::Video(frame)) => remote_video(&mut state, frame),
                None => return Ok(()),
            },
            Some(report) = internal_rx.recv() => match report {
                Internal::EncoderFailed(stream) => {
                    if state.outgoing.as_ref().is_some_and(|o| o.info.stream == stream) {
                        warn!("capture of stream {stream} stopped");
                        stop_outgoing(&link, &mut state);
                    }
                }
                Internal::DecoderFailed(stream) => {
                    if let Some(inc) = &mut state.incoming {
                        inc.keyframe_seen = false;
                    }
                    link.send(Message::KeyframeRequest { stream });
                }
            },
            _ = tokio::signal::ctrl_c() => {
                end_incoming(&mut state);
                stop_outgoing(&link, &mut state);
                return Ok(());
            }
        }
    }
}

/// When the user drags the proxy, drag the real window on Linux along.
fn watch_proxy(link: &Link, state: &mut State) {
    let Some(proxy) = &state.proxy else { return };
    let w = proxy.window();
    state.proxy_dragging = w.in_move_size();
    if !state.proxy_dragging {
        return;
    }
    let (Some(session_id), Ok(rect)) = (state.remote_session.clone(), w.rect()) else { return };
    let canvas = state.tracker.edge().to_canvas(rect);
    link.send(Message::ProxyMoved { session_id, x: canvas.x, y: canvas.y });
}

fn poll(state: &mut State) -> Option<Message> {
    // A window that was crossing and has since closed ends its session.
    if let Some(w) = state.tracked
        && w.rect().is_err()
    {
        state.tracked = None;
        return state.tracker.removed(&w.id());
    }
    let w = Window::foreground()?;
    // The proxy hangs off the edge on purpose; never track it.
    if state.proxy.as_ref().is_some_and(|p| p.window() == w) {
        return None;
    }
    let rect = w.rect().ok()?;
    let msg = state.tracker.sample(&w.id(), rect, w.in_move_size());
    if matches!(msg, Some(Message::Sync(_))) {
        state.tracked = Some(w);
    }
    msg
}

/// Starts, restarts or stops streaming to follow the handoff.
fn update_outgoing(link: &Link, state: &mut State, msg: &Message) {
    match msg {
        Message::Sync(p) => {
            let size = (p.window_dim.width, p.window_dim.height);
            let current = state
                .outgoing
                .as_ref()
                .map(|o| (o.session_id.as_str(), (o.info.width, o.info.height)));
            if current == Some((p.session_id.as_str(), size))
                || state.failed_session.as_deref() == Some(p.session_id.as_str())
            {
                return;
            }
            let Some(window) = state.tracked else { return };
            // Same session, new size: keep the stream id so the receiver
            // reuses its proxy.
            let stream = match &state.outgoing {
                Some(o) if o.session_id == p.session_id => o.info.stream,
                _ => {
                    stop_outgoing(link, state);
                    state.next_stream += 1;
                    state.next_stream
                }
            };
            state.outgoing = None;
            let info = StreamInfo {
                stream,
                session_id: p.session_id.clone(),
                codec: "h264".into(),
                width: size.0,
                height: size.1,
                title: window.title(),
            };
            link.send(Message::StreamStart(info.clone()));
            let internal = state.internal.clone();
            match Encoder::start(
                Source::Win32Window { hwnd: window.raw() },
                info.width,
                info.height,
                stream,
                link.video_sender(),
                move || {
                    let _ = internal.send(Internal::EncoderFailed(stream));
                },
            ) {
                Ok(encoder) => {
                    state.failed_session = None;
                    state.outgoing =
                        Some(Outgoing { session_id: p.session_id.clone(), window, info, encoder });
                }
                Err(e) => {
                    warn!("cannot stream \"{}\": {e:#}", info.title);
                    link.send(Message::StreamEnd { stream });
                    state.failed_session = Some(p.session_id.clone());
                }
            }
        }
        Message::Release { .. } => {
            stop_outgoing(link, state);
            state.failed_session = None;
        }
        _ => {}
    }
}

fn stop_outgoing(link: &Link, state: &mut State) {
    if let Some(out) = state.outgoing.take() {
        link.send(Message::StreamEnd { stream: out.info.stream });
    }
}

fn remote_message(link: &Link, state: &mut State, msg: Message) {
    state.log.incoming(&msg);
    match msg {
        Message::Sync(p) if p.direction == Direction::LinuxToWindows => {
            state.remote_session = Some(p.session_id.clone());
            if !state.proxy_dragging {
                place_proxy(state, &p);
            }
        }
        Message::Sync(p) => warn!("ignoring sync heading the wrong way: {p:?}"),
        Message::Release { .. } => end_incoming(state),
        Message::StreamStart(info) => start_incoming(link, state, info),
        Message::StreamEnd { stream } => {
            if state.incoming.as_ref().is_some_and(|i| i.info.stream == stream) {
                state.incoming = None;
                if let Some(p) = &state.proxy {
                    p.hide();
                }
            }
        }
        Message::KeyframeRequest { stream } => {
            if let Some(out) = state.outgoing.as_ref().filter(|o| o.info.stream == stream) {
                out.encoder.force_keyframe();
            }
        }
        Message::ProxyMoved { session_id, x, y } => {
            let Some(out) = state.outgoing.as_ref().filter(|o| o.session_id == session_id) else {
                return;
            };
            let w = out.window;
            let edge = state.tracker.edge();
            let result = w.rect().and_then(|r| {
                let target = edge.to_local(Rect { x, y, ..edge.to_canvas(r) });
                w.place(target, w.margins().unwrap_or_default())
            });
            if let Err(e) = result {
                warn!("following the proxy: {e:#}");
            }
        }
        _ => {}
    }
}

fn start_incoming(link: &Link, state: &mut State, info: StreamInfo) {
    let Some(proxy) = state.proxy.clone() else { return };
    proxy.show(&info.title);
    // A resize of the same stream keeps its decoder; the decoder adapts.
    if let Some(inc) = &mut state.incoming
        && inc.info.stream == info.stream
    {
        inc.info = info;
        return;
    }
    let stream = info.stream;
    let internal = state.internal.clone();
    let sink_proxy = proxy.clone();
    match Decoder::start(
        DecoderSink::Frames(PixelFormat::Bgra, Box::new(move |f| sink_proxy.frame(f))),
        move || {
            let _ = internal.send(Internal::DecoderFailed(stream));
        },
    ) {
        Ok(decoder) => {
            state.incoming = Some(Incoming { info, decoder, keyframe_seen: false });
            link.send(Message::KeyframeRequest { stream });
        }
        Err(e) => warn!("cannot decode the stream: {e:#}"),
    }
}

fn remote_video(state: &mut State, frame: VideoFrame) {
    let Some(inc) = state.incoming.as_mut().filter(|i| i.info.stream == frame.stream) else {
        return;
    };
    // Deltas before the first keyframe cannot be decoded.
    if !inc.keyframe_seen {
        if !frame.keyframe {
            return;
        }
        inc.keyframe_seen = true;
    }
    inc.decoder.push(&frame);
}

fn place_proxy(state: &State, p: &SyncPacket) {
    let Some(proxy) = &state.proxy else { return };
    let w = proxy.window();
    let edge = state.tracker.edge();
    // The popup has no invisible borders; the clip keeps it from spilling
    // onto a monitor beyond the bezel.
    let target = edge.to_local(proxy_placement(p.direction, p.window_dim, p.edge_delta));
    let result = w.place(target, Margins::default()).and_then(|()| {
        w.clip(Some(proxy_clip(p.direction, p.window_dim, p.edge_delta)), Margins::default())
    });
    if let Err(e) = result {
        warn!("placing proxy: {e:#}");
    }
}

/// The remote window went home: slide the proxy past the right edge and hide it.
fn end_incoming(state: &mut State) {
    state.remote_session = None;
    state.incoming = None;
    let Some(proxy) = &state.proxy else { return };
    let w = proxy.window();
    let edge = state.tracker.edge();
    let result = w
        .rect()
        .and_then(|r| w.place(Rect { x: edge.bezel_x, ..r }, Margins::default()))
        .and_then(|()| w.clip(None, Margins::default()));
    if let Err(e) = result {
        warn!("parking proxy: {e:#}");
    }
    proxy.hide();
}
