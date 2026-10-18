//! The unified virtual canvas.
//!
//! The Windows screen sits to the left of the Linux screen and the bezel
//! between them is canvas `x = 0`: Windows spans `-W_win..0`, Linux `0..W_lin`.
//! Canvas `y = 0` is the top of both screens. Each daemon converts between its
//! local desktop coordinates and the canvas with a [`ScreenEdge`].

use serde::{Deserialize, Serialize};

use crate::protocol::{Direction, EdgeDelta, WindowDim};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Windows,
    Linux,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, width: i32, height: i32) -> Self {
        Self { x, y, width, height }
    }

    pub const fn right(&self) -> i32 {
        self.x + self.width
    }

    pub const fn translate(&self, dx: i32, dy: i32) -> Self {
        Self::new(self.x + dx, self.y + dy, self.width, self.height)
    }
}

/// Space between a window's visible frame and the rectangle the OS positions
/// it by, such as the invisible resize borders on Windows 10 and later.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Margins {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl Margins {
    /// The margins of `visible` inside `outer`.
    pub const fn between(outer: Rect, visible: Rect) -> Self {
        Self {
            left: visible.x - outer.x,
            top: visible.y - outer.y,
            right: outer.right() - visible.right(),
            bottom: (outer.y + outer.height) - (visible.y + visible.height),
        }
    }

    /// The outer rectangle that makes the visible frame land on `visible`.
    pub const fn outset(&self, visible: Rect) -> Rect {
        Rect::new(
            visible.x - self.left,
            visible.y - self.top,
            visible.width + self.left + self.right,
            visible.height + self.top + self.bottom,
        )
    }
}

/// Where the bezel and the top of the screen are in one side's local desktop
/// coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScreenEdge {
    pub side: Side,
    /// Local x of the bezel: the right edge of the Windows desktop, or the left
    /// edge of the Linux desktop.
    pub bezel_x: i32,
    /// Local y of the top of the screen.
    pub top_y: i32,
}

impl ScreenEdge {
    /// Builds the edge from the bounds of the whole local desktop.
    pub const fn from_desktop(side: Side, desktop: Rect) -> Self {
        let bezel_x = match side {
            Side::Windows => desktop.right(),
            Side::Linux => desktop.x,
        };
        Self { side, bezel_x, top_y: desktop.y }
    }

    pub const fn to_canvas(&self, local: Rect) -> Rect {
        local.translate(-self.bezel_x, -self.top_y)
    }

    pub const fn to_local(&self, canvas: Rect) -> Rect {
        canvas.translate(self.bezel_x, self.top_y)
    }

    /// The direction a window on this side travels when it crosses the bezel.
    pub const fn outgoing(&self) -> Direction {
        match self.side {
            Side::Windows => Direction::WindowsToLinux,
            Side::Linux => Direction::LinuxToWindows,
        }
    }

    /// How far a local window (in canvas coordinates) hangs over the bezel
    /// into the other side, or `None` if it does not touch it.
    pub fn overhang(&self, canvas: Rect) -> Option<EdgeDelta> {
        let past = match self.side {
            Side::Linux => -canvas.x,
            Side::Windows => canvas.right(),
        };
        (past > 0).then(|| EdgeDelta { visible_pixels: past.min(canvas.width), offset_y: canvas.y })
    }
}

/// Where the proxy for a remote window goes on the receiving side, in canvas
/// coordinates, so that exactly `visible_pixels` of it show next to the bezel.
///
/// A Linux window entering Windows shows its left slice at the right edge of
/// the Windows screen; a Windows window entering Linux shows its right slice at
/// the left edge of the Linux screen.
pub fn proxy_placement(direction: Direction, dim: WindowDim, delta: EdgeDelta) -> Rect {
    let visible = delta.visible_pixels.clamp(0, dim.width);
    let x = match direction {
        Direction::LinuxToWindows => -visible,
        Direction::WindowsToLinux => visible - dim.width,
    };
    Rect::new(x, delta.offset_y, dim.width, dim.height)
}

/// The part of the proxy window, in its own client coordinates, that should be
/// drawn. Needed when the proxy would otherwise spill onto a neighbouring
/// monitor instead of falling off the edge of the desktop.
pub fn proxy_clip(direction: Direction, dim: WindowDim, delta: EdgeDelta) -> Rect {
    let visible = delta.visible_pixels.clamp(0, dim.width);
    let x = match direction {
        Direction::LinuxToWindows => 0,
        Direction::WindowsToLinux => dim.width - visible,
    };
    Rect::new(x, 0, visible, dim.height)
}

#[cfg(test)]
mod tests {
    use super::*;

    const WIN_DESKTOP: Rect = Rect::new(0, 0, 2560, 1440);
    const LIN_DESKTOP: Rect = Rect::new(0, 0, 3840, 2160);

    #[test]
    fn margins_round_trip() {
        // A Windows 11 window: 7 px invisible borders left, right and bottom.
        let outer = Rect::new(93, 50, 814, 607);
        let visible = Rect::new(100, 50, 800, 600);
        let m = Margins::between(outer, visible);
        assert_eq!(m, Margins { left: 7, top: 0, right: 7, bottom: 7 });
        assert_eq!(m.outset(visible), outer);
    }

    #[test]
    fn canvas_round_trip() {
        let edge = ScreenEdge::from_desktop(Side::Windows, WIN_DESKTOP);
        let local = Rect::new(2000, 100, 800, 600);
        let canvas = edge.to_canvas(local);
        assert_eq!(canvas, Rect::new(-560, 100, 800, 600));
        assert_eq!(edge.to_local(canvas), local);
    }

    #[test]
    fn linux_window_dragged_left() {
        let edge = ScreenEdge::from_desktop(Side::Linux, LIN_DESKTOP);
        let canvas = edge.to_canvas(Rect::new(-145, 320, 1280, 720));
        assert_eq!(edge.overhang(canvas), Some(EdgeDelta { visible_pixels: 145, offset_y: 320 }));
        assert_eq!(edge.overhang(edge.to_canvas(Rect::new(0, 0, 100, 100))), None);
    }

    #[test]
    fn windows_window_dragged_right() {
        let edge = ScreenEdge::from_desktop(Side::Windows, WIN_DESKTOP);
        let canvas = edge.to_canvas(Rect::new(2400, 50, 400, 300));
        assert_eq!(edge.overhang(canvas), Some(EdgeDelta { visible_pixels: 240, offset_y: 50 }));
    }

    #[test]
    fn overhang_is_capped_at_window_width() {
        let edge = ScreenEdge::from_desktop(Side::Linux, LIN_DESKTOP);
        let delta = edge.overhang(Rect::new(-5000, 0, 1280, 720)).unwrap();
        assert_eq!(delta.visible_pixels, 1280);
    }

    #[test]
    fn proxy_shows_the_crossed_slice() {
        let dim = WindowDim { width: 1280, height: 720 };
        let delta = EdgeDelta { visible_pixels: 145, offset_y: 320 };

        let on_windows = proxy_placement(Direction::LinuxToWindows, dim, delta);
        assert_eq!(on_windows, Rect::new(-145, 320, 1280, 720));
        assert_eq!(proxy_clip(Direction::LinuxToWindows, dim, delta), Rect::new(0, 0, 145, 720));

        let on_linux = proxy_placement(Direction::WindowsToLinux, dim, delta);
        assert_eq!(on_linux, Rect::new(145 - 1280, 320, 1280, 720));
        assert_eq!(on_linux.right(), 145);
        assert_eq!(
            proxy_clip(Direction::WindowsToLinux, dim, delta),
            Rect::new(1280 - 145, 0, 145, 720)
        );
    }
}
