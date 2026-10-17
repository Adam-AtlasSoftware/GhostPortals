//! KWin (Plasma 6) backend.
//!
//! Loads `kwin/bridge.js` into KWin through its Scripting DBus API. The script
//! calls back into the `io.ghostportals.KWinBridge` object served here with
//! JSON payloads, and long-polls `NextCommand` for moves.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use ghostportals_core::Rect;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::{Mutex, mpsc};
use tracing::{debug, warn};
use zbus::{Connection, interface};

use super::{BackendEvent, WindowInfo};

const BUS_NAME: &str = "io.ghostportals.Linux";
const OBJECT_PATH: &str = "/io/ghostportals/KWin";
const SCRIPT_NAME: &str = "ghostportals-bridge";
const SCRIPT: &str = include_str!("../../kwin/bridge.js");
/// Below the default 25 s DBus reply timeout the script's call runs with.
const POLL_TIMEOUT: Duration = Duration::from_secs(20);

pub struct Kwin {
    conn: Connection,
    commands: mpsc::Sender<String>,
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
    commands: Arc<Mutex<mpsc::Receiver<String>>>,
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

    /// Returns the next command for the script, or "" after a while so the
    /// script's call never times out.
    async fn next_command(&self) -> String {
        let mut rx = self.commands.lock().await;
        match tokio::time::timeout(POLL_TIMEOUT, rx.recv()).await {
            Ok(Some(cmd)) => cmd,
            _ => String::new(),
        }
    }
}

impl Kwin {
    pub async fn connect(events: mpsc::Sender<BackendEvent>) -> Result<Self> {
        let (cmd_tx, cmd_rx) = mpsc::channel(64);
        let bridge = Bridge { events, commands: Arc::new(Mutex::new(cmd_rx)) };
        let conn = zbus::connection::Builder::session()?
            .name(BUS_NAME)?
            .serve_at(OBJECT_PATH, bridge)?
            .build()
            .await
            .with_context(|| {
                format!("cannot claim {BUS_NAME} on the session bus (already running?)")
            })?;

        let path = write_script()?;
        // A previous run that crashed may have left its script behind.
        unload(&conn).await?;
        let id: i32 = conn
            .call_method(
                Some("org.kde.KWin"),
                "/Scripting",
                Some("org.kde.kwin.Scripting"),
                "loadScript",
                &(path.to_string_lossy().as_ref(), SCRIPT_NAME),
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
        Ok(Self { conn, commands: cmd_tx })
    }

    pub async fn move_window(&self, id: &str, x: i32, y: i32) -> Result<()> {
        self.command(json!({ "op": "move", "id": id, "x": x, "y": y })).await
    }

    pub async fn shutdown(self) -> Result<()> {
        // Stop the script's poll loop first so it does not hammer a vanished
        // bus name between our exit and the unload.
        let _ = self.command(json!({ "op": "quit" })).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        unload(&self.conn).await
    }

    async fn command(&self, cmd: serde_json::Value) -> Result<()> {
        self.commands.send(cmd.to_string()).await.context("KWin bridge stopped")
    }
}

async fn unload(conn: &Connection) -> Result<()> {
    conn.call_method(
        Some("org.kde.KWin"),
        "/Scripting",
        Some("org.kde.kwin.Scripting"),
        "unloadScript",
        &(SCRIPT_NAME,),
    )
    .await
    .context("cannot reach KWin scripting")?;
    Ok(())
}

fn write_script() -> Result<PathBuf> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("ghostportals");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join("kwin-bridge.js");
    std::fs::write(&path, SCRIPT).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}
