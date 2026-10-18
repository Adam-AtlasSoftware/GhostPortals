//! Installing, updating and uninstalling on the user's machine, and starting
//! with the session. Everything is per user: no administrator rights needed.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

pub const APP_NAME: &str = "Ghost Portals";
#[cfg(windows)]
pub const MANAGER_EXE: &str = "ghostportals.exe";

/// Where this executable lives.
pub fn current_exe() -> PathBuf {
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from("ghostportals"))
}

/// `%LOCALAPPDATA%\Programs\Ghost Portals`, like other per-user installs.
#[cfg(windows)]
pub fn install_dir() -> PathBuf {
    dirs::data_local_dir().unwrap_or_else(std::env::temp_dir).join("Programs").join(APP_NAME)
}

/// Whether this executable is the installed copy (and may update itself).
#[cfg(windows)]
pub fn is_installed_copy() -> bool {
    let (Ok(exe), Ok(dir)) = (current_exe().canonicalize(), install_dir().canonicalize()) else {
        return false;
    };
    exe.starts_with(dir)
}

/// The version recorded by the last install into `dir`.
#[cfg(windows)]
pub fn installed_version(dir: &Path) -> Option<String> {
    std::fs::read_to_string(dir.join("version.txt")).ok().map(|s| s.trim().to_owned())
}

#[derive(Debug, Clone, Copy)]
pub struct Options {
    pub desktop_shortcut: bool,
    pub start_on_login: bool,
}

/// Unpacks a downloaded bundle into `dir` and registers it: Start menu
/// shortcut, optional desktop shortcut and autostart, and the entry under
/// Settings → Apps. Files in use (a running manager or daemon) are renamed
/// out of the way first; Windows allows that, and they are cleaned up on the
/// next start.
#[cfg(windows)]
pub fn install(zip: &Path, dir: &Path, version: &str, opts: Options) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let staging = dir.join(".staging");
    let _ = std::fs::remove_dir_all(&staging);
    let files = crate::update::unpack(zip, &staging)?;
    for src in &files {
        let rel = src.strip_prefix(&staging).expect("unpacked under staging");
        let dest = dir.join(rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if dest.exists() && std::fs::remove_file(&dest).is_err() {
            let old = dest.with_extension(format!("old-{}", std::process::id()));
            std::fs::rename(&dest, &old)
                .with_context(|| format!("{} is in use and cannot be replaced", dest.display()))?;
        }
        std::fs::rename(src, &dest).with_context(|| format!("installing {}", dest.display()))?;
    }
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::write(dir.join("version.txt"), version)?;

    let exe = dir.join(MANAGER_EXE);
    windows_impl::shortcut(&windows_impl::start_menu_link()?, &exe, dir)?;
    match windows_impl::desktop_link() {
        Some(link) if opts.desktop_shortcut => windows_impl::shortcut(&link, &exe, dir)?,
        Some(link) => {
            let _ = std::fs::remove_file(link);
        }
        None => {}
    }
    set_autostart(&exe, opts.start_on_login)?;
    windows_impl::register_uninstall(dir, &exe, version, dir_size(dir))?;
    Ok(())
}

/// Removes everything `install` added. The folder itself is deleted by a
/// short-lived helper once this process has exited.
#[cfg(windows)]
pub fn uninstall() -> Result<()> {
    let dir = install_dir();
    let _ = std::fs::remove_file(windows_impl::start_menu_link()?);
    if let Some(link) = windows_impl::desktop_link() {
        let _ = std::fs::remove_file(link);
    }
    set_autostart(&dir.join(MANAGER_EXE), false)?;
    windows_impl::unregister_uninstall();
    if let Some(settings) = dirs::config_dir().map(|d| d.join("GhostPortals")) {
        let _ = std::fs::remove_dir_all(settings);
    }
    windows_impl::delete_after_exit(&dir)
}

/// Deletes files renamed aside by an earlier update.
#[cfg(windows)]
pub fn clean_old_files() {
    fn walk(dir: &Path) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p);
            } else if p.extension().and_then(|x| x.to_str()).is_some_and(|x| x.starts_with("old-"))
            {
                let _ = std::fs::remove_file(&p);
            }
        }
    }
    if is_installed_copy() {
        walk(&install_dir());
    }
}

#[cfg(windows)]
fn dir_size(dir: &Path) -> u64 {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| {
            let p = e.path();
            if p.is_dir() { dir_size(&p) } else { e.metadata().map(|m| m.len()).unwrap_or(0) }
        })
        .sum()
}

/// Starts the manager (minimized, daemon running) when the user logs in.
pub fn set_autostart(exe: &Path, enabled: bool) -> Result<()> {
    #[cfg(windows)]
    {
        windows_impl::set_run_value(enabled.then(|| format!("\"{}\" --minimized", exe.display())))
    }
    #[cfg(not(windows))]
    {
        let path = autostart_file().context("no config directory")?;
        if enabled {
            std::fs::create_dir_all(path.parent().expect("has a parent"))?;
            std::fs::write(
                &path,
                format!(
                    "[Desktop Entry]\nType=Application\nName={APP_NAME}\n\
                     Comment=Carry windows between computers\nExec=\"{}\" --minimized\n\
                     X-GNOME-Autostart-enabled=true\n",
                    exe.display()
                ),
            )?;
        } else if path.exists() {
            std::fs::remove_file(&path)?;
        }
        Ok(())
    }
}

pub fn autostart_enabled() -> bool {
    #[cfg(windows)]
    {
        windows_impl::run_value().is_some()
    }
    #[cfg(not(windows))]
    {
        autostart_file().is_some_and(|p| p.exists())
    }
}

#[cfg(not(windows))]
fn autostart_file() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("autostart").join("io.ghostportals.manager.desktop"))
}

#[cfg(windows)]
mod windows_impl {
    use std::path::{Path, PathBuf};

    use anyhow::{Context, Result, bail};
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Com::{
        CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
        IPersistFile,
    };
    use windows::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_DWORD, REG_OPTION_NON_VOLATILE, REG_SZ,
        RRF_RT_REG_SZ, RegCloseKey, RegCreateKeyExW, RegDeleteTreeW, RegDeleteValueW, RegGetValueW,
        RegSetValueExW,
    };
    use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
    use windows::core::{HSTRING, Interface, PCWSTR};

    use super::APP_NAME;

    const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    const UNINSTALL_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\GhostPortals";

    pub fn start_menu_link() -> Result<PathBuf> {
        let programs = dirs::data_dir()
            .context("no AppData folder")?
            .join(r"Microsoft\Windows\Start Menu\Programs");
        Ok(programs.join(format!("{APP_NAME}.lnk")))
    }

    pub fn desktop_link() -> Option<PathBuf> {
        Some(dirs::desktop_dir()?.join(format!("{APP_NAME}.lnk")))
    }

    pub fn shortcut(link: &Path, target: &Path, workdir: &Path) -> Result<()> {
        unsafe {
            // Already initialised on this thread is fine too.
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            let sl: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)
                .context("creating a shortcut object")?;
            sl.SetPath(&HSTRING::from(target.as_os_str()))?;
            sl.SetWorkingDirectory(&HSTRING::from(workdir.as_os_str()))?;
            sl.SetDescription(&HSTRING::from("Carry windows between computers"))?;
            sl.SetIconLocation(&HSTRING::from(target.as_os_str()), 0)?;
            if let Some(parent) = link.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let file: IPersistFile = sl.cast()?;
            file.Save(&HSTRING::from(link.as_os_str()), true)
                .with_context(|| format!("saving {}", link.display()))?;
        }
        Ok(())
    }

    struct Key(HKEY);

    impl Drop for Key {
        fn drop(&mut self) {
            unsafe {
                let _ = RegCloseKey(self.0);
            }
        }
    }

    fn open(path: &str) -> Result<Key> {
        let mut key = HKEY::default();
        let err = unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                &HSTRING::from(path),
                None,
                PCWSTR::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_READ | KEY_WRITE,
                None,
                &mut key,
                None,
            )
        };
        if err != ERROR_SUCCESS {
            bail!("opening HKCU\\{path}: {err:?}");
        }
        Ok(Key(key))
    }

    fn set_string(key: &Key, name: &str, value: &str) -> Result<()> {
        let wide: Vec<u16> = value.encode_utf16().chain(Some(0)).collect();
        let bytes =
            unsafe { std::slice::from_raw_parts(wide.as_ptr() as *const u8, wide.len() * 2) };
        let err = unsafe { RegSetValueExW(key.0, &HSTRING::from(name), None, REG_SZ, Some(bytes)) };
        if err != ERROR_SUCCESS {
            bail!("writing registry value {name}: {err:?}");
        }
        Ok(())
    }

    fn set_dword(key: &Key, name: &str, value: u32) -> Result<()> {
        let bytes = value.to_le_bytes();
        let err =
            unsafe { RegSetValueExW(key.0, &HSTRING::from(name), None, REG_DWORD, Some(&bytes)) };
        if err != ERROR_SUCCESS {
            bail!("writing registry value {name}: {err:?}");
        }
        Ok(())
    }

    pub fn set_run_value(command: Option<String>) -> Result<()> {
        let key = open(RUN_KEY)?;
        match command {
            Some(cmd) => set_string(&key, APP_NAME, &cmd),
            None => {
                unsafe {
                    let _ = RegDeleteValueW(key.0, &HSTRING::from(APP_NAME));
                }
                Ok(())
            }
        }
    }

    pub fn run_value() -> Option<String> {
        let mut buf = [0u16; 1024];
        let mut len = (buf.len() * 2) as u32;
        let err = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                &HSTRING::from(RUN_KEY),
                &HSTRING::from(APP_NAME),
                RRF_RT_REG_SZ,
                None,
                Some(buf.as_mut_ptr() as *mut _),
                Some(&mut len),
            )
        };
        (err == ERROR_SUCCESS)
            .then(|| String::from_utf16_lossy(&buf[..(len as usize / 2).saturating_sub(1)]))
    }

    pub fn register_uninstall(dir: &Path, exe: &Path, version: &str, bytes: u64) -> Result<()> {
        let key = open(UNINSTALL_KEY)?;
        set_string(&key, "DisplayName", APP_NAME)?;
        set_string(&key, "DisplayVersion", version)?;
        set_string(&key, "Publisher", "Adam Wiley")?;
        set_string(&key, "URLInfoAbout", "https://github.com/Adam-AtlasSoftware/GhostPortals")?;
        set_string(&key, "InstallLocation", &dir.display().to_string())?;
        set_string(&key, "DisplayIcon", &exe.display().to_string())?;
        set_string(&key, "UninstallString", &format!("\"{}\" --uninstall", exe.display()))?;
        set_string(
            &key,
            "QuietUninstallString",
            &format!("\"{}\" --uninstall --silent", exe.display()),
        )?;
        set_dword(&key, "NoModify", 1)?;
        set_dword(&key, "NoRepair", 1)?;
        set_dword(&key, "EstimatedSize", (bytes / 1024) as u32)?;
        Ok(())
    }

    pub fn unregister_uninstall() {
        unsafe {
            let _ = RegDeleteTreeW(HKEY_CURRENT_USER, &HSTRING::from(UNINSTALL_KEY));
        }
    }

    /// A running executable cannot delete its own folder, so a detached
    /// `cmd` waits a moment and removes it.
    pub fn delete_after_exit(dir: &Path) -> Result<()> {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        std::process::Command::new("cmd")
            .raw_arg(format!("/c ping -n 3 127.0.0.1 >nul & rmdir /s /q \"{}\"", dir.display()))
            .creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS)
            .spawn()
            .context("scheduling the folder's removal")?;
        Ok(())
    }
}
