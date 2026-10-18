use std::collections::HashMap;
use std::net::SocketAddr;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use ghostportals_core::geometry::proxy_placement;
use ghostportals_core::{
    Direction, Endpoint, HandoffLog, HandoffTracker, Hello, Link, LinkEvent, Message,
    PROTOCOL_VERSION, Rect, ScreenEdge, Side, SyncPacket,
};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use crate::backend::{Backend, BackendEvent, Kind, WindowInfo};

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
        /// Title or app id substring of the local window that mirrors remote
        /// windows (the FreeRDP RemoteApp window).
        #[arg(long)]
        proxy: Option<String>,
    },
}

pub async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let cli = Cli::parse();
    let kind = match cli.backend {
        Some(k) => k,
        None => Kind::detect()?,
    };
    info!("using the {kind} backend");
    let (backend, events) = Backend::connect(kind).await?;

    let result = match cli.command {
        Command::Watch => watch(events).await,
        Command::Run { listen, connect, token, proxy } => {
            let endpoint = match connect {
                Some(addr) => Endpoint::Connect(addr),
                None => Endpoint::Listen(listen),
            };
            run(&backend, events, endpoint, token, proxy).await
        }
    };
    backend.shutdown().await?;
    result
}

async fn watch(mut events: tokio::sync::mpsc::Receiver<BackendEvent>) -> Result<()> {
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

struct State {
    windows: HashMap<String, WindowInfo>,
    tracker: HandoffTracker,
    proxy: Option<String>,
    desktop_known: bool,
    log: HandoffLog,
    /// One warning per drag of the proxy itself, one per missing proxy.
    warned_proxy_drag: bool,
    warned_missing_proxy: bool,
}

impl State {
    fn edge(&self) -> ScreenEdge {
        self.tracker.edge()
    }

    fn is_proxy(&self, w: &WindowInfo) -> bool {
        self.proxy.as_deref().is_some_and(|p| w.title.contains(p) || w.app_id.contains(p))
    }

    fn proxy_window(&self) -> Option<&WindowInfo> {
        self.windows.values().find(|w| self.is_proxy(w))
    }
}

async fn run(
    backend: &Backend,
    mut events: tokio::sync::mpsc::Receiver<BackendEvent>,
    endpoint: Endpoint,
    token: Option<String>,
    proxy: Option<String>,
) -> Result<()> {
    let hello = Hello {
        protocol_version: PROTOCOL_VERSION,
        side: Side::Linux,
        host: hostname(),
        token: token.clone(),
    };
    let mut link = Link::spawn(endpoint, hello, token)
        .await
        .context("cannot listen for the peer (is another daemon already running?)")?;
    let mut state = State {
        windows: HashMap::new(),
        tracker: HandoffTracker::new(ScreenEdge::from_desktop(Side::Linux, Rect::default())),
        proxy,
        desktop_known: false,
        log: HandoffLog::default(),
        warned_proxy_drag: false,
        warned_missing_proxy: false,
    };
    if state.proxy.is_none() {
        warn!("no --proxy given: windows arriving from Windows will not be shown");
    }

    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            ev = events.recv() => {
                let Some(ev) = ev else {
                    warn!("compositor backend stopped");
                    return Ok(());
                };
                if let Some(msg) = local_event(&mut state, ev) {
                    state.log.outgoing(&msg);
                    link.send(msg);
                }
            }
            ev = link.recv() => match ev {
                Some(LinkEvent::Connected(peer)) => info!("handoff link up with {}", peer.host),
                Some(LinkEvent::Disconnected) => {
                    info!("handoff link down");
                    park_proxy(backend, &state).await;
                }
                Some(LinkEvent::Message(msg)) => remote_message(backend, &mut state, msg).await,
                None => return Ok(()),
            },
            _ = &mut shutdown => {
                park_proxy(backend, &state).await;
                return Ok(());
            }
        }
    }
}

fn local_event(state: &mut State, ev: BackendEvent) -> Option<Message> {
    match ev {
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
        BackendEvent::Window(w) => {
            // The proxy deliberately hangs off the edge; never echo it back.
            if state.is_proxy(&w) {
                tracing::debug!("proxy now at {:?}", w.rect);
                if w.moving && !state.warned_proxy_drag {
                    warn!(
                        "\"{}\" is the --proxy window: it only shows windows arriving from \
                         Windows and never crosses itself. Drag a different window to the left edge.",
                        w.title
                    );
                }
                state.warned_proxy_drag = w.moving;
            }
            let feed = state.desktop_known && !state.is_proxy(&w) && (w.active || w.moving);
            let msg = feed.then(|| state.tracker.sample(&w.id, w.rect, w.moving)).flatten();
            state.windows.insert(w.id.clone(), w);
            msg
        }
    }
}

async fn remote_message(backend: &Backend, state: &mut State, msg: Message) {
    state.log.incoming(&msg);
    match msg {
        Message::Sync(p) if p.direction == Direction::WindowsToLinux => {
            show_proxy(backend, state, &p).await
        }
        Message::Sync(p) => warn!("ignoring sync heading the wrong way: {p:?}"),
        Message::Release { .. } => park_proxy(backend, state).await,
        _ => {}
    }
}

async fn show_proxy(backend: &Backend, state: &mut State, p: &SyncPacket) {
    let Some(proxy) = state.proxy_window().cloned() else {
        if !state.warned_missing_proxy {
            match &state.proxy {
                Some(p) => warn!(
                    "a window is arriving from Windows, but no window matches --proxy \"{p}\""
                ),
                None => warn!("a window is arriving from Windows, but no --proxy window was given"),
            }
            state.warned_missing_proxy = true;
        }
        return;
    };
    state.warned_missing_proxy = false;
    if (proxy.rect.width, proxy.rect.height) != (p.window_dim.width, p.window_dim.height) {
        tracing::debug!(
            "proxy is {}x{}, remote window is {}x{}",
            proxy.rect.width,
            proxy.rect.height,
            p.window_dim.width,
            p.window_dim.height
        );
    }
    let target = state.edge().to_local(proxy_placement(p.direction, p.window_dim, p.edge_delta));
    if let Err(e) = backend.place_window(&proxy.id, target).await {
        warn!("placing proxy: {e}");
    }
}

/// Slides the proxy back off the left edge of the desktop.
async fn park_proxy(backend: &Backend, state: &State) {
    let Some(proxy) = state.proxy_window() else { return };
    let x = state.edge().bezel_x - proxy.rect.width;
    if let Err(e) = backend.place_window(&proxy.id, Rect { x, ..proxy.rect }).await {
        warn!("parking proxy: {e}");
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
