//! Thin safe wrappers over the Win32 calls the daemon needs.

use anyhow::{Context, Result, bail};
use ghostportals_core::Rect;
use ghostportals_core::geometry::Margins;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Dwm::{DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute};
use windows::Win32::Graphics::Gdi::{CreateRectRgn, DeleteObject, HGDIOBJ, SetWindowRgn};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GUI_INMOVESIZE, GUITHREADINFO, GetForegroundWindow, GetGUIThreadInfo, GetSystemMetrics,
    GetWindowRect, GetWindowTextW, GetWindowThreadProcessId, IsZoomed, SM_CXVIRTUALSCREEN,
    SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SW_RESTORE, SWP_NOACTIVATE,
    SWP_NOZORDER, SetWindowPos, ShowWindow,
};

/// A top-level window handle. Plain data: the window may be gone by the time
/// it is used, and every call below reports that as an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window(HWND);

// HWNDs are process-independent identifiers, not pointers we dereference.
unsafe impl Send for Window {}

impl Window {
    pub fn from_raw(hwnd: isize) -> Self {
        Self(HWND(hwnd as _))
    }

    pub fn raw(&self) -> u64 {
        self.0.0 as usize as u64
    }

    pub fn id(&self) -> String {
        format!("{:#x}", self.0.0 as usize)
    }

    pub fn foreground() -> Option<Self> {
        let hwnd = unsafe { GetForegroundWindow() };
        (!hwnd.is_invalid()).then_some(Self(hwnd))
    }

    /// The visible frame in virtual-screen coordinates: what the user sees,
    /// without the invisible resize borders Windows 10+ adds around most
    /// windows. Falls back to the outer rectangle without DWM.
    pub fn rect(&self) -> Result<Rect> {
        let mut r = RECT::default();
        let dwm = unsafe {
            DwmGetWindowAttribute(
                self.0,
                DWMWA_EXTENDED_FRAME_BOUNDS,
                &mut r as *mut RECT as *mut _,
                size_of::<RECT>() as u32,
            )
        };
        match dwm {
            Ok(()) => Ok(rect(r)),
            Err(_) => self.outer_rect(),
        }
    }

    /// The rectangle Windows positions the window by, invisible borders
    /// included.
    pub fn outer_rect(&self) -> Result<Rect> {
        let mut r = RECT::default();
        unsafe { GetWindowRect(self.0, &mut r) }.context("GetWindowRect")?;
        Ok(rect(r))
    }

    /// The invisible borders around the visible frame. Only meaningful while
    /// the window is unclipped: a window region changes how DWM frames it.
    pub fn margins(&self) -> Result<Margins> {
        Ok(Margins::between(self.outer_rect()?, self.rect()?))
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

    /// Puts the visible frame exactly on `visible`, restoring the window
    /// first if it is maximized.
    pub fn place(&self, visible: Rect, margins: Margins) -> Result<()> {
        let outer = margins.outset(visible);
        unsafe {
            if IsZoomed(self.0).as_bool() {
                let _ = ShowWindow(self.0, SW_RESTORE);
            }
            SetWindowPos(
                self.0,
                None,
                outer.x,
                outer.y,
                outer.width,
                outer.height,
                SWP_NOZORDER | SWP_NOACTIVATE,
            )
        }
        .context("SetWindowPos")
    }

    /// Restricts drawing to `clip` (relative to the visible frame), or removes
    /// the restriction with `None`. Used so a proxy hanging over the right
    /// edge of the Windows desktop never spills onto a monitor beyond it.
    pub fn clip(&self, clip: Option<Rect>, margins: Margins) -> Result<()> {
        unsafe {
            // Regions are relative to the outer rectangle.
            let region = clip.map(|c| {
                let c = c.translate(margins.left, margins.top);
                CreateRectRgn(c.x, c.y, c.right(), c.y + c.height)
            });
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
}

fn rect(r: RECT) -> Rect {
    Rect::new(r.left, r.top, r.right - r.left, r.bottom - r.top)
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
