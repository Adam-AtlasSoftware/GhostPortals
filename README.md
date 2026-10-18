# Ghost Portals

Drag a window off the edge of one computer's screen and watch it slide onto the
next one, across operating systems. Ghost Portals runs next to
[Synergy](https://symless.com/synergy): Synergy moves the mouse, while Ghost
Portals carries the window itself between a Windows machine and a Linux
(Wayland) machine.

When a real window crosses the bezel on one machine, the daemon there starts
capturing it and streams it as H.264 to its peer. The peer shows the stream in
a borderless **proxy** window that slides in from the opposite edge by exactly
as far as the real window has crossed, at exactly its size. Dragging the proxy
by its top strip (where the streamed title bar is) drags the real window along.

```
 Windows screen           │ bezel │           Linux screen
 x = -W_win … 0           │ x = 0 │           x = 0 … W_lin
                          │       │
        ┌──────────────┬──┼──┐    │
        │  proxy (Win) │▒▒│  │◀── real Linux window dragged left
        └──────────────┴──┼──┘    │
                 visible_pixels
```

Clicks, scrolling and keys on a proxy are replayed on the real window, so a
window carried across is usable where it lands. A click in the proxy's top
strip that does not drag still reaches the window, so its title bar buttons
work.

**Status:** alpha. One window at a time in each direction. Streaming *from*
Linux works on KDE Plasma 6; GNOME and Hyprland can receive windows from
Windows but cannot send theirs yet. Using a **Linux** window from its proxy on
Windows does not work while Linux is the Synergy (or Input Leap, Deskflow)
server: see [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md#input). Using a
Windows window from its proxy on Linux works either way.

## Layout

| Crate | What it does |
|---|---|
| `crates/ghostportals-core` | Wire protocol, virtual canvas geometry, handoff tracker, self-healing TCP link |
| `crates/ghostportals-media` | Window capture, H.264 encoding and decoding on GStreamer, with the best encoder picked at runtime |
| `crates/ghostportals-linux` | Linux daemon: KWin, Hyprland and GNOME Shell backends, GTK proxy window |
| `crates/ghostportals-windows` | Windows daemon: foreground polling, Windows.Graphics.Capture, GDI proxy window |
| `extensions/gnome` | GNOME Shell extension that the GNOME backend talks to |

[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) explains why it is built this way.

## Linux

Needs GStreamer (with the PipeWire, `libav` and bad plugins; your GPU's
encoder plugin for hardware encoding) and GTK 4.

```sh
cargo build --release -p ghostportals-linux -p ghostportals-gui
./target/release/ghostportals                  # the app, or use the daemon directly:
./target/release/ghostportals-linux selftest   # can this machine encode and decode?
./target/release/ghostportals-linux watch      # print live window geometry
./target/release/ghostportals-linux run        # listen on 0.0.0.0:47800
```

The backend is picked from the session (`--backend kwin|hyprland|gnome` forces
one):

- **KDE Plasma 6 (KWin)**: works out of the box. The daemon loads a small KWin
  script (`crates/ghostportals-linux/kwin/bridge.js`) over DBus and unloads it
  on exit. To capture windows without asking, it registers itself with KWin
  through a `.desktop` file in `~/.local/share/applications`
  (`io.ghostportals.linux-*.desktop`, one per executable path).
- **Hyprland**: through Hyprland's IPC sockets. The proxy window is made
  floating so it can be placed freely.
- **GNOME / Ubuntu (Mutter)**: install and enable the extension first:
  ```sh
  cp -r extensions/gnome/ghostportals@ghostportals.io ~/.local/share/gnome-shell/extensions/
  # log out and back in (Wayland), then:
  gnome-extensions enable ghostportals@ghostportals.io
  ```

## Windows

Download **`GhostPortalsSetup.exe`** from the
[releases page](https://github.com/Adam-AtlasSoftware/GhostPortals/releases)
and run it. It fetches the newest version from GitHub, checks its SHA-256,
installs it for your user (no administrator rights) into
`%LOCALAPPDATA%\Programs\Ghost Portals`, and adds a Start menu shortcut and,
if you like, a desktop shortcut and "start when I log in". Remove it from
Settings → Apps.

The installed **Ghost Portals** app starts and stops sharing, shows what is
going on, and installs updates itself when a new release appears.

Prefer a terminal? `ghostportals-windows.zip` holds the same files: unpack it
and run `ghostportals-windows.exe run --connect linux-box:47800`. It carries
its own GStreamer. To build it yourself, install GStreamer's MSVC development
package and `pkg-config`, then
`cargo build --release -p ghostportals-windows -p ghostportals-gui`.

## The Ghost Portals app

`ghostportals` (`ghostportals.exe` on Windows) is a small window for running
the daemon without a terminal, on both systems:

- start and stop sharing, with the connection, the window being sent or
  received, and the encoder and decoder in use;
- settings: wait for the other computer or connect to it, shared secret,
  start when you log in;
- a self-test button and the daemon's live log;
- update checks against GitHub releases (Windows installs them; Linux points
  you to the release).

It runs the daemon found beside it (or on `PATH`) and stops it when it
closes.

## Both sides

- Each handoff logs one `sent handoff …` / `received handoff …` line when it
  starts and one `release` line when it ends, plus which encoder or decoder it
  uses. `RUST_LOG=debug` adds every frame and every proxy move.
- `--no-remote-control` (or the app's "Let the other computer click and type
  in my windows") ignores input sent from the other computer's proxies.
- `--token <secret>` (or `GHOSTPORTALS_TOKEN`) makes the peer prove it knows a
  shared secret. Use it on any network you do not fully trust; traffic is not
  encrypted.
- Video shares the control connection, so port 47800/TCP is the only one to
  open.

## Development

```sh
cargo test --workspace
cargo run -p ghostportals-media --example selftest
# Stand-in for the other machine:
cargo run -p ghostportals-media --example fake_peer -- listen 0.0.0.0:47800 dump.h264
cargo run -p ghostportals-media --example fake_peer -- sweep 127.0.0.1:47800 800x600
# One captured frame of a window, by the id `watch` prints:
cargo run -p ghostportals-linux -- snapshot '{uuid}' frame.png
```

To run a development build next to a daemon that is already running, give it
its own DBus and KWin script names with `GHOSTPORTALS_INSTANCE=dev`. Without
that, the second daemon refuses to start instead of taking over.

To type-check the Windows crate from Linux, add the target and run
`PKG_CONFIG_ALLOW_CROSS=1 cargo clippy --target x86_64-pc-windows-gnu -p ghostportals-windows`.
The host's GStreamer `.pc` files are enough for a check, which links nothing.

Pushing a `v*` tag builds both daemons, self-tests them and publishes them as a
release; tags with a suffix (`v0.1.0-alpha.1`) are published as pre-releases.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE). The Windows zip also contains
GStreamer and its dependencies under their own licenses; see the `README.txt`
inside it.
