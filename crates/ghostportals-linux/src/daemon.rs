use std::collections::HashMap;
use std::net::SocketAddr;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use ghostportals_core::geometry::proxy_placement;
use ghostportals_core::{
    Direction, Endpoint, HandoffLog, HandoffTracker, Hello, Link, LinkEvent, Message,
    PROTOCOL_VERSION, Rect, ScreenEdge, Side, StreamInfo, SyncPacket, VideoFrame,
};
use ghostportals_media::{Decoder, DecoderSink, Encoder, PixelFormat};
use tokio::sync::mpsc;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use crate::backend::{Backend, BackendEvent, Capture, Kind, WindowInfo};
use crate::proxy::{self, Proxy};

const DEFAULT_PORT: u16 = 47800;

#[derive(Parser)]
#[command(version, about = "Ghost Portals daemon for the Linux (Wayland) side")]
struct Cli {
    /// Compositor backend; detected from the session when omitted.
    #[arg(long, global = true)]
    backend: Option<Kind>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print window geometry as the compositor reports it.
    Watch,
    /// Run the handoff daemon.
    Run {
        /// Address to accept the Windows daemon on.
        #[arg(long, default_value_t = SocketAddr::from(([0, 0, 0, 0], DEFAULT_PORT)), conflicts_with = "connect")]
        listen: SocketAddr,
        /// Dial the Windows daemon instead of listening (host:port).
        #[arg(long)]
        connect: Option<String>,
        /// Shared secret both daemons must agree on.
        #[arg(long, env = "GHOSTPORTALS_TOKEN")]
        token: Option<String>,
    },
    /// Check that this machine can encode and decode window streams.
    Selftest,
    /// Save one captured frame of a window as PNG. Takes a window id as
    /// `watch` prints it, or an app id such as io.ghostportals.proxy.
    Snapshot { id: String, output: std::path::PathBuf },
}

pub async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let cli = Cli::parse();
    if let Command::Selftest = cli.command {
        println!("{}", ghostportals_media::selftest(3)?);
        return Ok(());
    }
    let kind = match cli.backend {
        Some(k) => k,
        None => Kind::detect()?,
    };
    info!("using the {kind} backend");
    let (backend, events) = Backend::connect(kind).await?;

    let result = match cli.command {
        Command::Watch => watch(events).await,
        Command::Run { listen, connect, token } => {
            let endpoint = match connect {
                Some(addr) => Endpoint::Connect(addr),
                None => Endpoint::Listen(listen),
            };
            run(&backend, events, endpoint, token).await
        }
        Command::Selftest => unreachable!(),
        Command::Snapshot { id, output } => snapshot(&backend, events, &id, &output).await,
    };
    backend.shutdown().await?;
    result
}

async fn watch(mut events: mpsc::Receiver<BackendEvent>) -> Result<()> {
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            ev = events.recv() => match ev {
                Some(BackendEvent::Window(w)) => println!(
                    "{:<40} {:>6},{:<6} {:>5}x{:<5} {}{} {} — {}",
                    w.id, w.rect.x, w.rect.y, w.rect.width, w.rect.height,
                    if w.active { "A" } else { "-" },
                    if w.moving { "M" } else { "-" },
                    w.app_id, w.title,
                ),
                Some(BackendEvent::Removed(id)) => println!("{id:<40} removed"),
                Some(BackendEvent::Desktop(r)) => println!("desktop {r:?}"),
                None => return Ok(()),
            },
            _ = &mut shutdown => return Ok(()),
        }
    }
}

/// The local window being streamed to the peer.
async fn snapshot(
    backend: &Backend,
    mut events: mpsc::Receiver<BackendEvent>,
    id: &str,
    output: &std::path::Path,
) -> Result<()> {
    // The backend reports every window first; wait for this one's size.
    let (id, rect) = loop {
        match tokio::time::timeout(std::time::Duration::from_secs(3), events.recv()).await {
            Ok(Some(BackendEvent::Window(w))) if w.id == id || w.app_id == id => {
                break (w.id, w.rect);
            }
            Ok(Some(_)) => {}
            _ => anyhow::bail!("no window with id {id}"),
        }
    };
    let capture = tokio::task::block_in_place(|| backend.capture(&id))?;
    tokio::task::block_in_place(|| {
        ghostportals_media::snapshot(capture.source, rect.width, rect.height, output)
    })?;
    println!("saved {}x{} to {}", rect.width, rect.height, output.display());
    Ok(())
}

struct Outgoing {
    session_id: String,
    window_id: String,
    info: StreamInfo,
    encoder: Encoder,
    _capture: Capture,
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
    windows: HashMap<String, WindowInfo>,
    tracker: HandoffTracker,
    desktop_known: bool,
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

impl State {
    fn edge(&self) -> ScreenEdge {
        self.tracker.edge()
    }

    fn proxy_window(&self) -> Option<&WindowInfo> {
        self.windows.values().find(|w| w.app_id == proxy::APP_ID)
    }
}

async fn run(
    backend: &Backend,
    mut events: mpsc::Receiver<BackendEvent>,
    endpoint: Endpoint,
    token: Option<String>,
) -> Result<()> {
    ghostportals_media::init()?;
    let hello = Hello {
        protocol_version: PROTOCOL_VERSION,
        side: Side::Linux,
        host: hostname(),
        token: token.clone(),
    };
    let mut link = Link::spawn(endpoint, hello, token)
        .await
        .context("cannot listen for the peer (is another daemon already running?)")?;
    let proxy = Proxy::spawn()
        .map_err(|e| warn!("no proxy window, windows from Windows will not be shown: {e:#}"))
        .ok();
    let (internal_tx, mut internal_rx) = mpsc::unbounded_channel();
    let mut state = State {
        windows: HashMap::new(),
        tracker: HandoffTracker::new(ScreenEdge::from_desktop(Side::Linux, Rect::default())),
        desktop_known: false,
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

    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            ev = events.recv() => {
                let Some(ev) = ev else {
                    warn!("compositor backend stopped");
                    return Ok(());
                };
                local_event(backend, &link, &mut state, ev).await;
            }
            ev = link.recv() => match ev {
                Some(LinkEvent::Connected(peer)) => {
                    info!("handoff link up with {}", peer.host);
                    // A new peer needs the stream header and a keyframe.
                    if let Some(out) = &state.outgoing {
                        link.send(Message::StreamStart(out.info.clone()));
                        out.encoder.force_keyframe();
                    }
                }
                Some(LinkEvent::Disconnected) => {
                    info!("handoff link down");
                    end_incoming(backend, &mut state).await;
                }
                Some(LinkEvent::Message(msg)) => remote_message(backend, &link, &mut state, msg).await,
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
            _ = &mut shutdown => {
                end_incoming(backend, &mut state).await;
                stop_outgoing(&link, &mut state);
                return Ok(());
            }
        }
    }
}

async fn local_event(backend: &Backend, link: &Link, state: &mut State, ev: BackendEvent) {
    let msg = match ev {
        BackendEvent::Desktop(r) => {
            info!("desktop bounds {r:?}");
            state.tracker.set_edge(ScreenEdge::from_desktop(Side::Linux, r));
            state.desktop_known = true;
            None
        }
        BackendEvent::Removed(id) => {
            state.windows.remove(&id);
            state.tracker.removed(&id)
        }
        BackendEvent::Window(w) if w.app_id == proxy::APP_ID => {
            // The proxy hangs off the edge on purpose; never track it. When
            // the user drags it, move the real window on the other side.
            tracing::debug!("proxy now at {:?}", w.rect);
            state.proxy_dragging = w.moving;
            if w.moving
                && let Some(session_id) = state.remote_session.clone()
            {
                let canvas = state.edge().to_canvas(w.rect);
                link.send(Message::ProxyMoved { session_id, x: canvas.x, y: canvas.y });
            }
            state.windows.insert(w.id.clone(), w);
            None
        }
        BackendEvent::Window(w) => {
            let feed = state.desktop_known && (w.active || w.moving);
            let msg = feed.then(|| state.tracker.sample(&w.id, w.rect, w.moving)).flatten();
            state.windows.insert(w.id.clone(), w);
            msg
        }
    };
    if let Some(msg) = msg {
        state.log.outgoing(&msg);
        link.send(msg.clone());
        update_outgoing(backend, link, state, &msg);
    }
}

/// Starts, restarts or stops streaming to follow the handoff.
fn update_outgoing(backend: &Backend, link: &Link, state: &mut State, msg: &Message) {
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
            let Some(window_id) = state.tracker.window().map(str::to_owned) else { return };
            let title = state.windows.get(&window_id).map(|w| w.title.clone()).unwrap_or_default();
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
                title,
            };
            match start_stream(backend, link, state, &window_id, &info) {
                Ok(out) => {
                    state.failed_session = None;
                    state.outgoing = Some(out);
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

fn start_stream(
    backend: &Backend,
    link: &Link,
    state: &State,
    window_id: &str,
    info: &StreamInfo,
) -> Result<Outgoing> {
    // Capture setup waits on the compositor for a few milliseconds.
    let capture = tokio::task::block_in_place(|| backend.capture(window_id))?;
    link.send(Message::StreamStart(info.clone()));
    let stream = info.stream;
    let internal = state.internal.clone();
    let encoder = Encoder::start(
        capture.source,
        info.width,
        info.height,
        stream,
        link.video_sender(),
        move || {
            let _ = internal.send(Internal::EncoderFailed(stream));
        },
    )?;
    Ok(Outgoing {
        session_id: info.session_id.clone(),
        window_id: window_id.to_owned(),
        info: info.clone(),
        encoder,
        _capture: capture,
    })
}

fn stop_outgoing(link: &Link, state: &mut State) {
    if let Some(out) = state.outgoing.take() {
        link.send(Message::StreamEnd { stream: out.info.stream });
    }
}

async fn remote_message(backend: &Backend, link: &Link, state: &mut State, msg: Message) {
    state.log.incoming(&msg);
    match msg {
        Message::Sync(p) if p.direction == Direction::WindowsToLinux => {
            state.remote_session = Some(p.session_id.clone());
            if !state.proxy_dragging {
                place_proxy(backend, state, &p).await;
            }
        }
        Message::Sync(p) => warn!("ignoring sync heading the wrong way: {p:?}"),
        Message::Release { .. } => end_incoming(backend, state).await,
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
            let Some(w) = state.windows.get(&out.window_id) else { return };
            let target = state.edge().to_local(Rect { x, y, ..state.edge().to_canvas(w.rect) });
            if let Err(e) = backend.place_window(&out.window_id, target).await {
                warn!("following the proxy: {e}");
            }
        }
        _ => {}
    }
}

fn start_incoming(link: &Link, state: &mut State, info: StreamInfo) {
    let Some(proxy) = state.proxy.clone() else { return };
    proxy.show(info.width, info.height, &info.title);
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
        DecoderSink::Frames(PixelFormat::Rgba, Box::new(move |f| sink_proxy.frame(f))),
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

async fn place_proxy(backend: &Backend, state: &State, p: &SyncPacket) {
    let Some(proxy) = state.proxy_window() else { return };
    let target = state.edge().to_local(proxy_placement(p.direction, p.window_dim, p.edge_delta));
    if let Err(e) = backend.place_window(&proxy.id, target).await {
        warn!("placing proxy: {e}");
    }
}

/// The remote window went home: slide the proxy off the left edge and hide it.
async fn end_incoming(backend: &Backend, state: &mut State) {
    state.remote_session = None;
    state.incoming = None;
    if let Some(proxy) = state.proxy_window() {
        let x = state.edge().bezel_x - proxy.rect.width;
        if let Err(e) = backend.place_window(&proxy.id, Rect { x, ..proxy.rect }).await {
            warn!("parking proxy: {e}");
        }
    }
    if let Some(p) = &state.proxy {
        p.hide();
    }
}

/// Resolves on Ctrl+C or SIGTERM (killall, systemd), so either way the
/// daemon unloads what it installed in the compositor before exiting.
async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    match signal(SignalKind::terminate()) {
        Ok(mut term) => tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        },
        Err(e) => {
            warn!("cannot watch for SIGTERM: {e}");
            let _ = tokio::signal::ctrl_c().await;
        }
    }
    info!("shutting down");
}

fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|h| h.trim().to_owned())
        .unwrap_or_else(|_| "linux".into())
}
