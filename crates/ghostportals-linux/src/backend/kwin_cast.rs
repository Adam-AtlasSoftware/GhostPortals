//! Window capture on KWin through `zkde_screencast_unstable_v1`.
//!
//! KWin streams a window by its internal UUID into PipeWire without asking
//! the user, but only offers the protocol to executables a `.desktop` file
//! grants it to (`X-KDE-Wayland-Interfaces`). [`ensure_grant`] writes that
//! file for the running executable.
//!
//! Each capture gets its own Wayland connection on its own thread, which
//! holds the stream open until the returned [`Cast`] is dropped.

use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use tracing::{debug, info};
use wayland_client::{Connection, Dispatch, QueueHandle, protocol::wl_registry};

#[allow(dead_code, non_camel_case_types, non_upper_case_globals, unused_imports, clippy::all)]
mod proto {
    use wayland_client;
    use wayland_client::protocol::*;
    pub mod __interfaces {
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("protocols/zkde-screencast-unstable-v1.xml");
    }
    use self::__interfaces::*;
    wayland_scanner::generate_client_code!("protocols/zkde-screencast-unstable-v1.xml");
}

use proto::zkde_screencast_stream_unstable_v1 as stream;
use proto::zkde_screencast_unstable_v1 as cast;

/// Interfaces granted to the daemon: window capture now, input injection
/// for forwarding clicks and keys to captured windows.
const INTERFACES: &str = "zkde_screencast_unstable_v1,org_kde_kwin_fake_input";

/// PipeWire's object serial arrives in protocol version 6.
const MIN_VERSION: u32 = 6;

/// A live window stream. Dropping it ends the stream.
pub struct Cast {
    pub serial: u64,
    stop: mpsc::Sender<()>,
}

impl Drop for Cast {
    fn drop(&mut self) {
        let _ = self.stop.send(());
    }
}

#[derive(Default)]
struct State {
    cast: Option<cast::ZkdeScreencastUnstableV1>,
    serial: Option<u64>,
    failed: Option<String>,
    closed: bool,
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global { name, interface, version } = event
            && interface == "zkde_screencast_unstable_v1"
            && version >= MIN_VERSION
        {
            state.cast = Some(registry.bind(name, MIN_VERSION, qh, ()));
        }
    }
}

impl Dispatch<cast::ZkdeScreencastUnstableV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &cast::ZkdeScreencastUnstableV1,
        _: cast::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<stream::ZkdeScreencastStreamUnstableV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &stream::ZkdeScreencastStreamUnstableV1,
        event: stream::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            stream::Event::Serial { object_serial_hi, object_serial_low } => {
                state.serial = Some(((object_serial_hi as u64) << 32) | object_serial_low as u64);
            }
            stream::Event::Failed { error } => state.failed = Some(error),
            stream::Event::Closed => state.closed = true,
            _ => {}
        }
    }
}

/// Starts streaming the window with KWin UUID `uuid`. Blocks until KWin has
/// created the stream (a few milliseconds), so call it off the async runtime.
pub fn stream_window(uuid: &str) -> Result<Cast> {
    let (ready_tx, ready_rx) = mpsc::channel();
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let uuid = uuid.to_owned();
    std::thread::Builder::new()
        .name("kwin-cast".into())
        .spawn(move || {
            if let Err(e) = run(&uuid, &ready_tx, stop_rx) {
                let _ = ready_tx.send(Err(e));
            }
        })
        .context("starting the capture thread")?;
    let serial = ready_rx
        .recv_timeout(Duration::from_secs(3))
        .map_err(|_| anyhow!("KWin did not answer the capture request"))??;
    Ok(Cast { serial, stop: stop_tx })
}

fn run(uuid: &str, ready: &mpsc::Sender<Result<u64>>, stop: mpsc::Receiver<()>) -> Result<()> {
    let conn = Connection::connect_to_env().context("connecting to the Wayland display")?;
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    conn.display().get_registry(&qh, ());
    let mut state = State::default();
    queue.roundtrip(&mut state)?;
    let Some(manager) = state.cast.clone() else {
        bail!(
            "KWin does not offer window capture to this program; the grant in {} may not be \
             active yet (log out and in once if it persists)",
            desktop_file()?.display()
        );
    };
    // Pointer mode 1: hidden. The proxy shows the remote pointer itself.
    let handle = manager.stream_window(uuid.to_owned(), 1, &qh, ());
    while state.serial.is_none() && state.failed.is_none() && !state.closed {
        queue.blocking_dispatch(&mut state)?;
    }
    if let Some(e) = state.failed {
        bail!("KWin refused to capture {uuid}: {e}");
    }
    let serial = state.serial.ok_or_else(|| anyhow!("KWin closed the stream for {uuid}"))?;
    debug!("KWin streams {uuid} as PipeWire serial {serial}");
    let _ = ready.send(Ok(serial));

    // Keep the connection (and with it the stream) alive until dropped.
    let _ = stop.recv();
    handle.close();
    manager.destroy();
    let _ = conn.flush();
    Ok(())
}

/// Where the grant for the running executable lives. One file per executable
/// path, so debug and release builds can both be granted.
fn desktop_file() -> Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let mut h = DefaultHasher::new();
    exe.hash(&mut h);
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .context("neither XDG_DATA_HOME nor HOME is set")?;
    Ok(data.join("applications").join(format!("io.ghostportals.linux-{:016x}.desktop", h.finish())))
}

/// Makes sure KWin will offer capture and input protocols to this executable,
/// writing the grant and refreshing KDE's application cache if needed.
pub fn ensure_grant() -> Result<()> {
    let exe = std::env::current_exe()?;
    let path = desktop_file()?;
    let contents = format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=Ghost Portals\n\
         Comment=Lets the Ghost Portals daemon capture windows for streaming\n\
         Exec={}\n\
         NoDisplay=true\n\
         X-KDE-Wayland-Interfaces={INTERFACES}\n",
        exe.display()
    );
    if std::fs::read_to_string(&path).is_ok_and(|c| c == contents) {
        return Ok(());
    }
    std::fs::create_dir_all(path.parent().expect("has a parent"))?;
    std::fs::write(&path, contents).with_context(|| format!("writing {}", path.display()))?;
    info!("granted window capture to {} ({})", exe.display(), path.display());
    // KWin reads grants from KDE's service cache; rebuild it now rather than
    // waiting for kded to notice.
    match std::process::Command::new("kbuildsycoca6").arg("--noincremental").output() {
        Ok(out) if out.status.success() => {}
        Ok(out) => debug!("kbuildsycoca6 exited with {}", out.status),
        Err(e) => debug!("kbuildsycoca6 not run: {e}"),
    }
    Ok(())
}
