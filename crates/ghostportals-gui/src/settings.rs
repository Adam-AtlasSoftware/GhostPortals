//! What the manager remembers between runs.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Dial the other machine (host:port). Empty: listen for it instead.
    pub connect: String,
    /// Port to listen on when not dialling.
    pub listen_port: u16,
    pub token: String,
    /// Start the daemon as soon as the manager opens.
    pub autostart_daemon: bool,
    /// Look for a newer release when the manager opens.
    pub check_updates: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            // Windows usually dials the Linux side, which listens.
            connect: String::new(),
            listen_port: 47800,
            token: String::new(),
            autostart_daemon: true,
            check_updates: true,
        }
    }
}

fn path() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("GhostPortals").join("settings.json"))
}

impl Settings {
    pub fn load() -> Self {
        path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let p = path().ok_or_else(|| anyhow::anyhow!("no config directory"))?;
        std::fs::create_dir_all(p.parent().expect("has a parent"))?;
        std::fs::write(&p, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    /// Arguments for `<daemon> run`.
    pub fn run_args(&self) -> Vec<String> {
        let mut args = vec!["run".to_owned()];
        if self.connect.trim().is_empty() {
            args.extend(["--listen".into(), format!("0.0.0.0:{}", self.listen_port)]);
        } else {
            args.extend(["--connect".into(), self.connect.trim().to_owned()]);
        }
        args
    }
}
