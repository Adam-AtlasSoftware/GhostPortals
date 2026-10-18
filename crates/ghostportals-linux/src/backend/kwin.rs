//! KWin (Plasma 6) backend.
//!
//! Loads `kwin/bridge.js` into KWin through its Scripting DBus API. The script
//! calls back into the `io.ghostportals.KWinBridge` object served here with
//! JSON payloads, and long-polls `NextCommand` for placements.
//!
//! Each poll returns every queued command at once, keeping only the newest
//! placement per window, so a fast stream of proxy updates never queues up
//! behind KWin's round trips.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use ghostportals_core::Rect;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, mpsc};
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
    commands: mpsc::Sender<Command>,
    script: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum Command {
    Place { id: String, x: i32, y: i32, width: i32, height: i32 },
    Quit,
}

/// Drops placements superseded by a later one for the same window.
fn coalesce(commands: Vec<Command>) -> Vec<Command> {
    let mut out: Vec<Command> = Vec::with_capacity(commands.len());
    for cmd in commands {
        if let Command::Place { id, .. } = &cmd {
            out.retain(|c| !matches!(c, Command::Place { id: other, .. } if other == id));
        }
        out.push(cmd);
    }
    out
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
    commands: Arc<Mutex<mpsc::Receiver<Command>>>,
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
        let mut rx = self.commands.lock().await;
        let Ok(Some(first)) = tokio::time::timeout(POLL_TIMEOUT, rx.recv()).await else {
            return String::new();
        };
        let mut batch = vec![first];
        while let Ok(cmd) = rx.try_recv() {
            batch.push(cmd);
        }
        serde_json::to_string(&coalesce(batch)).unwrap_or_default()
    }
}

impl Kwin {
    pub async fn connect(events: mpsc::Sender<BackendEvent>) -> Result<Self> {
        let (cmd_tx, cmd_rx) = mpsc::channel(64);
        let bridge = Bridge { events, commands: Arc::new(Mutex::new(cmd_rx)) };
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
        Ok(Self { conn, commands: cmd_tx, script: names.script })
    }

    pub async fn place_window(&self, id: &str, r: Rect) -> Result<()> {
        self.command(Command::Place {
            id: id.to_owned(),
            x: r.x,
            y: r.y,
            width: r.width,
            height: r.height,
        })
        .await
    }

    pub async fn shutdown(self) -> Result<()> {
        // Stop the script's poll loop first so it does not hammer a vanished
        // bus name between our exit and the unload.
        let _ = self.command(Command::Quit).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        unload(&self.conn, &self.script).await
    }

    async fn command(&self, cmd: Command) -> Result<()> {
        self.commands.send(cmd).await.context("KWin bridge stopped")
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

    #[test]
    fn keeps_only_the_latest_placement_per_window() {
        let batch = coalesce(vec![place("a", 1), place("b", 1), place("a", 2), Command::Quit]);
        assert_eq!(batch, vec![place("b", 1), place("a", 2), Command::Quit]);
        assert_eq!(
            serde_json::to_string(&batch[1]).unwrap(),
            r#"{"op":"place","id":"a","x":2,"y":0,"width":10,"height":10}"#
        );
    }
}
