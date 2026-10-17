use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use ghostportals_core::geometry::{proxy_clip, proxy_placement};
use ghostportals_core::{
    Direction, Endpoint, HandoffTracker, Hello, Link, LinkEvent, Message, PROTOCOL_VERSION,
    ScreenEdge, Side, SyncPacket,
};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

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
        /// Title substring of the local window that mirrors Linux windows
        /// (the VcXsrv / X11-forwarded window).
        #[arg(long)]
        proxy: Option<String>,
    },
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
        Command::Run { connect, listen, token, proxy } => {
            let endpoint = match (connect, listen) {
                (Some(addr), _) => Endpoint::Connect(addr),
                (None, Some(addr)) => Endpoint::Listen(addr),
                (None, None) => bail!("pass --connect or --listen"),
            };
            run(endpoint, token, proxy).await
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

struct Proxy {
    needle: String,
    window: Option<Window>,
}

impl Proxy {
    fn get(&mut self) -> Option<Window> {
        if self.window.is_none_or(|w| w.rect().is_err()) {
            self.window = Window::find_by_title(&self.needle);
        }
        self.window
    }

    fn is(&self, w: Window) -> bool {
        self.window == Some(w) || w.title().contains(&self.needle)
    }
}

async fn run(endpoint: Endpoint, token: Option<String>, proxy: Option<String>) -> Result<()> {
    let hello = Hello {
        protocol_version: PROTOCOL_VERSION,
        side: Side::Windows,
        host: std::env::var("COMPUTERNAME").unwrap_or_else(|_| "windows".into()),
        token: token.clone(),
    };
    let mut link = Link::spawn(endpoint, hello, token);
    let mut tracker = HandoffTracker::new(edge());
    let mut proxy = proxy.map(|needle| Proxy { needle, window: None });
    if proxy.is_none() {
        warn!("no --proxy given: windows arriving from Linux will not be shown");
    }
    let mut ticker = tokio::time::interval(POLL);
    let mut tracked: Option<Window> = None;

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                tracker.set_edge(edge());
                if let Some(msg) = poll(&mut tracker, &mut tracked, proxy.as_ref()) {
                    link.send(msg);
                }
            }
            ev = link.recv() => match ev {
                Some(LinkEvent::Connected(peer)) => info!("handoff link up with {}", peer.host),
                Some(LinkEvent::Disconnected) => {
                    info!("handoff link down");
                    park_proxy(proxy.as_mut(), tracker.edge());
                }
                Some(LinkEvent::Message(Message::Sync(p))) if p.direction == Direction::LinuxToWindows => {
                    show_proxy(proxy.as_mut(), tracker.edge(), &p);
                }
                Some(LinkEvent::Message(Message::Release { .. })) => park_proxy(proxy.as_mut(), tracker.edge()),
                Some(LinkEvent::Message(other)) => warn!("ignoring {other:?}"),
                None => return Ok(()),
            },
            _ = tokio::signal::ctrl_c() => {
                park_proxy(proxy.as_mut(), tracker.edge());
                return Ok(());
            }
        }
    }
}

fn poll(
    tracker: &mut HandoffTracker,
    tracked: &mut Option<Window>,
    proxy: Option<&Proxy>,
) -> Option<Message> {
    // A window that was crossing and has since closed ends its session.
    if let Some(w) = *tracked
        && w.rect().is_err()
    {
        *tracked = None;
        return tracker.removed(&w.id());
    }
    let w = Window::foreground()?;
    // The proxy deliberately hangs off the edge; never echo it back.
    if proxy.is_some_and(|p| p.is(w)) {
        return None;
    }
    let rect = w.rect().ok()?;
    let msg = tracker.sample(&w.id(), rect, w.in_move_size());
    if matches!(msg, Some(Message::Sync(_))) {
        *tracked = Some(w);
    }
    msg
}

fn show_proxy(proxy: Option<&mut Proxy>, edge: ScreenEdge, p: &SyncPacket) {
    let Some(w) = proxy.and_then(Proxy::get) else { return };
    let target = edge.to_local(proxy_placement(p.direction, p.window_dim, p.edge_delta));
    let result = w
        .move_to(target.x, target.y)
        .and_then(|()| w.clip(Some(proxy_clip(p.direction, p.window_dim, p.edge_delta))));
    if let Err(e) = result {
        warn!("updating proxy: {e:#}");
    }
}

/// Slides the proxy back past the right edge of the desktop and unclips it.
fn park_proxy(proxy: Option<&mut Proxy>, edge: ScreenEdge) {
    let Some(w) = proxy.and_then(Proxy::get) else { return };
    let result = w.rect().and_then(|r| w.move_to(edge.bezel_x, r.y)).and_then(|()| w.clip(None));
    if let Err(e) = result {
        warn!("parking proxy: {e:#}");
    }
}
