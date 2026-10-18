//! Running the daemon as a child process and reading its log into a status.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::settings::Settings;

const LOG_LINES: usize = 2000;

#[cfg(windows)]
pub const DAEMON: &str = "ghostportals-windows.exe";
#[cfg(not(windows))]
pub const DAEMON: &str = "ghostportals-linux";

/// What the log says the daemon is doing.
#[derive(Debug, Clone, Default)]
pub struct Status {
    pub listening: Option<String>,
    pub peer: Option<String>,
    pub outgoing: Option<String>,
    pub incoming: Option<String>,
    pub encoder: Option<String>,
    pub decoder: Option<String>,
    pub last_problem: Option<String>,
}

impl Status {
    fn apply(&mut self, line: &str) {
        let after =
            |marker: &str| line.find(marker).map(|i| line[i + marker.len()..].trim().to_owned());
        if let Some(addr) = after("listening on ") {
            self.listening = Some(addr);
        } else if let Some(peer) = after("handoff link up with ") {
            self.peer = Some(peer);
        } else if line.contains("handoff link down") {
            self.peer = None;
            self.incoming = None;
        } else if line.contains("sent release of handoff") {
            self.outgoing = None;
        } else if line.contains("received release of handoff") {
            self.incoming = None;
        } else if let Some(what) = after("sent handoff ") {
            self.outgoing = Some(what);
        } else if let Some(what) = after("received handoff ") {
            self.incoming = Some(what);
        } else if let Some(what) = after("streaming ") {
            self.encoder = Some(what);
        } else if let Some(what) = after("decoding with ") {
            self.decoder = Some(what);
        }
        if line.contains(" WARN ") || line.contains(" ERROR ") || line.starts_with("Error:") {
            self.last_problem = Some(line.to_owned());
        }
    }
}

#[derive(Default)]
struct Shared {
    log: VecDeque<String>,
    status: Status,
}

pub struct Daemon {
    child: Option<Child>,
    shared: Arc<Mutex<Shared>>,
    started: Option<Instant>,
    exit: Option<String>,
}

/// The daemon beside this executable, or the one on PATH.
pub fn binary() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|d| d.join(DAEMON)))
        .filter(|p| p.exists())
        .unwrap_or_else(|| PathBuf::from(DAEMON))
}

fn command(args: &[String], settings: &Settings) -> Command {
    let mut cmd = Command::new(binary());
    cmd.args(args)
        .env("NO_COLOR", "1")
        .env("RUST_LOG", std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into()))
        .stdin(Stdio::null());
    if !settings.token.is_empty() {
        // Through the environment, so it does not show up in process lists.
        cmd.env("GHOSTPORTALS_TOKEN", &settings.token);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;
        // If the manager dies without stopping it, the daemon still gets a
        // SIGTERM and cleans up instead of running on unseen.
        // Safety: prctl is async-signal-safe.
        unsafe {
            cmd.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                Ok(())
            });
        }
    }
    cmd
}

/// Ties the daemon's lifetime to the manager's on Windows: the job closes
/// with the manager's process, killing everything in it.
#[cfg(windows)]
fn kill_with_manager(child: &Child) {
    use std::os::windows::io::AsRawHandle;
    use std::sync::OnceLock;

    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };

    static JOB: OnceLock<Option<isize>> = OnceLock::new();
    let job = JOB.get_or_init(|| unsafe {
        let job = CreateJobObjectW(None, None).ok()?;
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const _,
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
        .ok()?;
        Some(job.0 as isize)
    });
    if let Some(job) = job {
        unsafe {
            let _ = AssignProcessToJobObject(HANDLE(*job as _), HANDLE(child.as_raw_handle()));
        }
    }
}

/// Removes terminal colour codes, in case a daemon ignores NO_COLOR.
fn plain(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

impl Daemon {
    pub fn new() -> Self {
        Self { child: None, shared: Arc::default(), started: None, exit: None }
    }

    pub fn start(
        &mut self,
        settings: &Settings,
        repaint: impl Fn() + Send + Sync + 'static,
    ) -> Result<()> {
        self.stop();
        let mut child = command(&settings.run_args(), settings)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("starting {}", binary().display()))?;
        #[cfg(windows)]
        kill_with_manager(&child);
        {
            let mut s = self.shared.lock().unwrap();
            s.status = Status::default();
            s.log.push_back(format!("--- started {} {}", DAEMON, settings.run_args().join(" ")));
        }
        let repaint = Arc::new(repaint);
        let pipes: [Box<dyn std::io::Read + Send>; 2] = [
            Box::new(child.stdout.take().expect("piped")),
            Box::new(child.stderr.take().expect("piped")),
        ];
        for pipe in pipes {
            let shared = self.shared.clone();
            let repaint = repaint.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(pipe).lines().map_while(|l| l.ok()) {
                    let line = plain(&line);
                    let mut s = shared.lock().unwrap();
                    s.status.apply(&line);
                    s.log.push_back(line);
                    while s.log.len() > LOG_LINES {
                        s.log.pop_front();
                    }
                    drop(s);
                    repaint();
                }
            });
        }
        self.child = Some(child);
        self.started = Some(Instant::now());
        self.exit = None;
        Ok(())
    }

    /// Asks the daemon to shut down cleanly (it unloads what it installed in
    /// the compositor), and kills it if it does not within a few seconds.
    pub fn stop(&mut self) {
        let Some(mut child) = self.child.take() else { return };
        #[cfg(unix)]
        {
            // Safety: plain syscall on our own child's pid.
            unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
            let deadline = Instant::now() + Duration::from_secs(3);
            while Instant::now() < deadline && matches!(child.try_wait(), Ok(None)) {
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        let _ = child.kill();
        let _ = child.wait();
        self.shared.lock().unwrap().log.push_back("--- stopped".into());
        self.started = None;
    }

    pub fn running(&mut self) -> bool {
        let Some(child) = &mut self.child else { return false };
        match child.try_wait() {
            Ok(None) => true,
            Ok(Some(status)) => {
                self.exit = Some(format!("exited ({status})"));
                self.shared.lock().unwrap().log.push_back(format!("--- {DAEMON} {status}"));
                self.child = None;
                self.started = None;
                false
            }
            Err(_) => false,
        }
    }

    pub fn uptime(&self) -> Option<Duration> {
        self.started.map(|s| s.elapsed())
    }

    pub fn exit(&self) -> Option<&str> {
        self.exit.as_deref()
    }

    pub fn status(&self) -> Status {
        self.shared.lock().unwrap().status.clone()
    }

    pub fn log(&self) -> Vec<String> {
        self.shared.lock().unwrap().log.iter().cloned().collect()
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Runs `<daemon> selftest` and returns its last line.
pub fn selftest(settings: &Settings) -> Result<String> {
    let out = command(&["selftest".into()], settings)
        .output()
        .with_context(|| format!("running {}", binary().display()))?;
    let text = plain(&String::from_utf8_lossy(&out.stdout));
    let err = plain(&String::from_utf8_lossy(&out.stderr));
    let summary = text.lines().last().unwrap_or_default().to_owned();
    if out.status.success() {
        Ok(summary)
    } else {
        anyhow::bail!(
            "{}",
            err.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("self-test failed")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_follows_the_log() {
        let mut s = Status::default();
        for line in [
            "2026-10-18T02:00:00Z  INFO ghostportals_core::link: listening on 0.0.0.0:47800",
            "2026-10-18T02:00:01Z  INFO ghostportals_linux::daemon: handoff link up with DESKTOP-1",
            "2026-10-18T02:00:02Z  INFO ghostportals_core::handoff: sent handoff handoff-1 LinuxToWindows: 800x450 window, 12 px across",
            "2026-10-18T02:00:02Z  INFO ghostportals_media::pipeline: streaming 800x450 with nvh264enc",
        ] {
            s.apply(line);
        }
        assert_eq!(s.listening.as_deref(), Some("0.0.0.0:47800"));
        assert_eq!(s.peer.as_deref(), Some("DESKTOP-1"));
        assert!(s.outgoing.as_deref().unwrap().starts_with("handoff-1"));
        assert_eq!(s.encoder.as_deref(), Some("800x450 with nvh264enc"));
        s.apply("2026-10-18T02:00:03Z  INFO ghostportals_core::handoff: sent release of handoff handoff-1");
        assert_eq!(s.outgoing, None);
        s.apply("2026-10-18T02:00:04Z  WARN ghostportals_core::link: link lost: peer closed the connection");
        s.apply("2026-10-18T02:00:04Z  INFO ghostportals_linux::daemon: handoff link down");
        assert_eq!(s.peer, None);
        assert!(s.last_problem.unwrap().contains("link lost"));
    }

    #[test]
    fn plain_strips_colours() {
        assert_eq!(plain("\x1b[32m INFO\x1b[0m hi"), " INFO hi");
    }
}
