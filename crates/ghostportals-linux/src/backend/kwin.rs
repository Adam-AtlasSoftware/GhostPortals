//! KWin (Plasma 6) backend.
//!
//! Loads `kwin/bridge.js` into KWin through its Scripting DBus API. The script
//! calls back into the `io.ghostportals.KWinBridge` object served here with
//! JSON payloads, and long-polls `NextCommand` for placements.
//!
//! Commands wait in a [`Queue`] that keeps only the newest placement per
//! window. Each poll takes everything at once, so a fast stream of proxy
//! updates never queues up behind KWin's round trips, and the daemon never
//! blocks on a script that has stopped polling.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use ghostportals_core::Rect;
use serde::{Deserialize, Serialize};
use tokio::sync::{Notify, mpsc};
use tracing::{debug, warn};
use zbus::{Connection, interface};

use super::{BackendEvent, WindowInfo};

const BUS_NAME: &str = "io.ghostportals.Linux";
const OBJECT_PATH: &str = "/io/ghostportals/KWin";
const SCRIPT_NAME: &str = "ghostportals-bridge";
const SCRIPT: &str = include_str!("../../kwin/bridge.js");

/// Bus and script names. `GHOSTPORTALS_INSTANCE` gives a second daemon (a
/// development build, say) its own names, so it can run beside the main one
/// without stealing its bus name or unloading its script.
struct Names {
    bus: String,
    script: String,
}

impl Names {
    fn from_env() -> Self {
        match std::env::var("GHOSTPORTALS_INSTANCE") {
            Ok(i) if !i.is_empty() => {
                let i: String = i.chars().filter(char::is_ascii_alphanumeric).collect();
                Self { bus: format!("{BUS_NAME}.{i}"), script: format!("{SCRIPT_NAME}-{i}") }
            }
            _ => Self { bus: BUS_NAME.into(), script: SCRIPT_NAME.into() },
        }
    }
}
/// Below the default 25 s DBus reply timeout the script's call runs with.
const POLL_TIMEOUT: Duration = Duration::from_secs(20);

pub struct Kwin {
    conn: Connection,
    queue: Arc<Queue>,
    script: String,
    warned_stall: AtomicBool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum Command {
    Place { id: String, x: i32, y: i32, width: i32, height: i32 },
    Quit,
}

/// How long the script may go without polling before it counts as gone.
const STALL: Duration = Duration::from_secs(30);

/// Commands waiting for the script.
struct Queue {
    pending: Mutex<Vec<Command>>,
    ready: Notify,
    last_poll: Mutex<Instant>,
}

impl Queue {
    fn new() -> Self {
        Self {
            pending: Mutex::new(Vec::new()),
            ready: Notify::new(),
            last_poll: Mutex::new(Instant::now()),
        }
    }

    /// Queues `cmd`, replacing any older placement of the same window.
    fn push(&self, cmd: Command) {
        let mut pending = self.pending.lock().unwrap();
        if let Command::Place { id, .. } = &cmd {
            pending.retain(|c| !matches!(c, Command::Place { id: other, .. } if other == id));
        }
        pending.push(cmd);
        drop(pending);
        self.ready.notify_one();
    }

    /// Everything queued, waiting up to `timeout` for something to arrive.
    async fn take(&self, timeout: Duration) -> Vec<Command> {
        *self.last_poll.lock().unwrap() = Instant::now();
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let batch = std::mem::take(&mut *self.pending.lock().unwrap());
            if !batch.is_empty() {
                return batch;
            }
            // notify_one leaves a permit when nobody is waiting, so a push
            // between the check above and this await is not lost.
            if tokio::time::timeout_at(deadline, self.ready.notified()).await.is_err() {
                return batch;
            }
        }
    }

    /// Commands are waiting but the script has not asked for any in a while.
    fn stalled(&self) -> bool {
        !self.pending.lock().unwrap().is_empty() && self.last_poll.lock().unwrap().elapsed() > STALL
    }
}

#[derive(Deserialize)]
struct Update {
    id: String,
    title: String,
    app: String,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    active: bool,
    moving: bool,
}

#[derive(Deserialize)]
struct Screen {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

fn rect(x: f64, y: f64, width: f64, height: f64) -> Rect {
    Rect::new(x.round() as i32, y.round() as i32, width.round() as i32, height.round() as i32)
}

struct Bridge {
    events: mpsc::Sender<BackendEvent>,
    queue: Arc<Queue>,
}

impl Bridge {
    async fn emit(&self, event: BackendEvent) {
        if self.events.send(event).await.is_err() {
            debug!("event receiver gone");
        }
    }
}

#[interface(name = "io.ghostportals.KWinBridge")]
impl Bridge {
    async fn update(&self, payload: String) {
        match serde_json::from_str::<Update>(&payload) {
            Ok(u) => {
                self.emit(BackendEvent::Window(WindowInfo {
                    id: u.id,
                    title: u.title,
                    app_id: u.app,
                    rect: rect(u.x, u.y, u.width, u.height),
                    active: u.active,
                    moving: u.moving,
                }))
                .await
            }
            Err(e) => warn!("bad update from KWin script: {e}: {payload}"),
        }
    }

    async fn removed(&self, id: String) {
        self.emit(BackendEvent::Removed(id)).await;
    }

    async fn screen(&self, payload: String) {
        match serde_json::from_str::<Screen>(&payload) {
            Ok(s) => self.emit(BackendEvent::Desktop(rect(s.x, s.y, s.width, s.height))).await,
            Err(e) => warn!("bad screen from KWin script: {e}: {payload}"),
        }
    }

    /// Returns the pending commands as a JSON array, or "" after a while so
    /// the script's call never times out.
    async fn next_command(&self) -> String {
        let batch = self.queue.take(POLL_TIMEOUT).await;
        if batch.is_empty() {
            return String::new();
        }
        serde_json::to_string(&batch).unwrap_or_default()
    }
}

impl Kwin {
    pub async fn connect(events: mpsc::Sender<BackendEvent>) -> Result<Self> {
        let queue = Arc::new(Queue::new());
        let bridge = Bridge { events, queue: queue.clone() };
        let names = Names::from_env();
        // zbus lets a newer process take over a name by default, which would
        // silently orphan a running daemon and its script. Refuse instead.
        let conn = zbus::connection::Builder::session()?
            .allow_name_replacements(false)
            .replace_existing_names(false)
            .name(names.bus.as_str())?
            .serve_at(OBJECT_PATH, bridge)?
            .build()
            .await
            .with_context(|| {
                format!("cannot claim {} on the session bus (already running?)", names.bus)
            })?;

        let path = write_script(&names)?;
        // A previous run that crashed may have left its script behind.
        unload(&conn, &names.script).await?;
        let id: i32 = conn
            .call_method(
                Some("org.kde.KWin"),
                "/Scripting",
                Some("org.kde.kwin.Scripting"),
                "loadScript",
                &(path.to_string_lossy().as_ref(), names.script.as_str()),
            )
            .await
            .context("KWin scripting is not available (is this a Plasma session?)")?
            .body()
            .deserialize()?;
        if id < 0 {
            anyhow::bail!("KWin refused to load {}", path.display());
        }
        let run = conn
            .call_method(
                Some("org.kde.KWin"),
                format!("/Scripting/Script{id}").as_str(),
                Some("org.kde.kwin.Script"),
                "run",
                &(),
            )
            .await;
        if let Err(e) = run {
            debug!("Script{id}.run failed ({e}), falling back to Scripting.start");
            conn.call_method(
                Some("org.kde.KWin"),
                "/Scripting",
                Some("org.kde.kwin.Scripting"),
                "start",
                &(),
            )
            .await?;
        }
        Ok(Self { conn, queue, script: names.script, warned_stall: AtomicBool::new(false) })
    }

    pub async fn place_window(&self, id: &str, r: Rect) -> Result<()> {
        if self.queue.stalled() && !self.warned_stall.swap(true, Ordering::Relaxed) {
            warn!(
                "the KWin script has not collected commands for {}s; was it unloaded? \
                 Restart the daemon to reload it",
                STALL.as_secs()
            );
        }
        self.queue.push(Command::Place {
            id: id.to_owned(),
            x: r.x,
            y: r.y,
            width: r.width,
            height: r.height,
        });
        Ok(())
    }

    pub async fn shutdown(self) -> Result<()> {
        // Stop the script's poll loop first so it does not hammer a vanished
        // bus name between our exit and the unload.
        self.queue.push(Command::Quit);
        tokio::time::sleep(Duration::from_millis(100)).await;
        unload(&self.conn, &self.script).await
    }
}

async fn unload(conn: &Connection, script: &str) -> Result<()> {
    conn.call_method(
        Some("org.kde.KWin"),
        "/Scripting",
        Some("org.kde.kwin.Scripting"),
        "unloadScript",
        &(script,),
    )
    .await
    .context("cannot reach KWin scripting")?;
    Ok(())
}

fn write_script(names: &Names) -> Result<PathBuf> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("ghostportals");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join(format!("{}.js", names.script));
    let script = SCRIPT.replace(&format!("\"{BUS_NAME}\""), &format!("\"{}\"", names.bus));
    std::fs::write(&path, script).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn place(id: &str, x: i32) -> Command {
        Command::Place { id: id.into(), x, y: 0, width: 10, height: 10 }
    }

    #[tokio::test]
    async fn keeps_only_the_latest_placement_per_window() {
        let q = Queue::new();
        for cmd in [place("a", 1), place("b", 1), place("a", 2), Command::Quit] {
            q.push(cmd);
        }
        let batch = q.take(Duration::from_millis(10)).await;
        assert_eq!(batch, vec![place("b", 1), place("a", 2), Command::Quit]);
        assert_eq!(
            serde_json::to_string(&batch[1]).unwrap(),
            r#"{"op":"place","id":"a","x":2,"y":0,"width":10,"height":10}"#
        );
    }

    #[tokio::test]
    async fn take_waits_for_a_push_and_times_out_empty() {
        let q = Arc::new(Queue::new());
        assert!(q.take(Duration::from_millis(20)).await.is_empty());

        let pusher = q.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            pusher.push(Command::Quit);
        });
        assert_eq!(q.take(Duration::from_secs(5)).await, vec![Command::Quit]);
        assert!(!q.stalled());
    }
}
