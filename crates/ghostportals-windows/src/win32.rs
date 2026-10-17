//! Thin safe wrappers over the Win32 calls the daemon needs.

use anyhow::{Context, Result, bail};
use ghostportals_core::Rect;
use windows::Win32::Foundation::{HWND, LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{CreateRectRgn, DeleteObject, HGDIOBJ, SetWindowRgn};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GUI_INMOVESIZE, GUITHREADINFO, GetForegroundWindow, GetGUIThreadInfo,
    GetSystemMetrics, GetWindowRect, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible,
    SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SWP_NOACTIVATE,
    SWP_NOSIZE, SWP_NOZORDER, SetWindowPos,
};
use windows::core::BOOL;

/// A top-level window handle. Plain data: the window may be gone by the time
/// it is used, and every call below reports that as an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window(HWND);

// HWNDs are process-independent identifiers, not pointers we dereference.
unsafe impl Send for Window {}

impl Window {
    pub fn id(&self) -> String {
        format!("{:#x}", self.0.0 as usize)
    }

    pub fn foreground() -> Option<Self> {
        let hwnd = unsafe { GetForegroundWindow() };
        (!hwnd.is_invalid()).then_some(Self(hwnd))
    }

    /// Outer window rectangle in virtual-screen coordinates. Includes the
    /// invisible resize borders Windows 10+ adds around most windows.
    pub fn rect(&self) -> Result<Rect> {
        let mut r = RECT::default();
        unsafe { GetWindowRect(self.0, &mut r) }.context("GetWindowRect")?;
        Ok(Rect::new(r.left, r.top, r.right - r.left, r.bottom - r.top))
    }

    pub fn title(&self) -> String {
        let mut buf = [0u16; 512];
        let n = unsafe { GetWindowTextW(self.0, &mut buf) };
        String::from_utf16_lossy(&buf[..n.max(0) as usize])
    }

    /// Whether the user is dragging or resizing this window right now.
    pub fn in_move_size(&self) -> bool {
        let thread = unsafe { GetWindowThreadProcessId(self.0, None) };
        let mut info =
            GUITHREADINFO { cbSize: size_of::<GUITHREADINFO>() as u32, ..Default::default() };
        unsafe { GetGUIThreadInfo(thread, &mut info) }.is_ok()
            && (info.flags & GUI_INMOVESIZE).0 != 0
            && info.hwndMoveSize == self.0
    }

    pub fn move_to(&self, x: i32, y: i32) -> Result<()> {
        unsafe {
            SetWindowPos(self.0, None, x, y, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE)
        }
        .context("SetWindowPos")
    }

    /// Restricts drawing to `clip` (in window coordinates), or removes the
    /// restriction with `None`. Used so a proxy hanging over the right edge of
    /// the Windows desktop never spills onto a monitor placed beyond it.
    pub fn clip(&self, clip: Option<Rect>) -> Result<()> {
        unsafe {
            let region = clip.map(|c| CreateRectRgn(c.x, c.y, c.right(), c.y + c.height));
            if region.is_some_and(|r| r.is_invalid()) {
                bail!("CreateRectRgn failed");
            }
            // On success the system owns the region; on failure it is ours.
            if SetWindowRgn(self.0, region, true) == 0 {
                if let Some(r) = region {
                    let _ = DeleteObject(HGDIOBJ(r.0));
                }
                bail!("SetWindowRgn failed: {}", windows::core::Error::from_thread());
            }
        }
        Ok(())
    }

    /// The first visible top-level window whose title contains `needle`.
    pub fn find_by_title(needle: &str) -> Option<Self> {
        struct Search<'a> {
            needle: &'a str,
            found: Option<Window>,
        }
        unsafe extern "system" fn visit(hwnd: HWND, lparam: LPARAM) -> BOOL {
            let search = unsafe { &mut *(lparam.0 as *mut Search) };
            let w = Window(hwnd);
            if unsafe { IsWindowVisible(hwnd) }.as_bool() && w.title().contains(search.needle) {
                search.found = Some(w);
                return false.into();
            }
            true.into()
        }
        let mut search = Search { needle, found: None };
        // EnumWindows reports "failure" when the callback stops it early.
        let _ = unsafe { EnumWindows(Some(visit), LPARAM(&mut search as *mut Search as isize)) };
        search.found
    }
}

/// Bounds of the whole virtual desktop (all monitors).
pub fn virtual_desktop() -> Rect {
    unsafe {
        Rect::new(
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    }
}

/// Opts into physical-pixel coordinates so geometry matches what is on
/// screen regardless of display scaling.
pub fn enable_per_monitor_dpi() -> Result<()> {
    unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) }
        .context("SetProcessDpiAwarenessContext")
}
