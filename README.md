# Ghost Portals

Drag a window off the edge of one computer's screen and watch it slide onto the
next one, across operating systems. Ghost Portals runs next to
[Synergy](https://symless.com/synergy): Synergy moves the mouse, while Ghost
Portals keeps window geometry in step across a Windows machine and a Linux
(Wayland) machine.

Each side keeps a pre-authenticated remote view of the other side's app
running and hidden: FreeRDP RemoteApp on Linux, VcXsrv / X11 forwarding on
Windows. That remote view is the **proxy**. When a real window crosses the bezel
on one machine, the daemon there tells its peer how far the window has crossed.
The peer then slides its proxy in from the opposite edge by exactly that
amount.

```
 Windows screen           │ bezel │           Linux screen
 x = -W_win … 0           │ x = 0 │           x = 0 … W_lin
                          │       │
        ┌──────────────┬──┼──┐    │
        │  proxy (Win) │▒▒│  │◀── real Linux window dragged left
        └──────────────┴──┼──┘    │
                 visible_pixels
```

## Layout

| Crate | What it does |
|---|---|
| `crates/ghostportals-core` | Wire protocol, virtual canvas geometry, handoff tracker, self-healing TCP link |
| `crates/ghostportals-linux` | Linux daemon, with KWin, Hyprland and GNOME Shell backends |
| `crates/ghostportals-windows` | Windows daemon (Win32: foreground polling, `SetWindowPos`, `SetWindowRgn`) |
| `extensions/gnome` | GNOME Shell extension that the GNOME backend talks to |

[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) explains why it is built this way.

## Linux

```sh
cargo build --release -p ghostportals-linux
./target/release/ghostportals-linux watch                     # print live window geometry
./target/release/ghostportals-linux run --proxy xfreerdp      # listen on 0.0.0.0:47800
```

The backend is picked from the session (`--backend kwin|hyprland|gnome` forces
one):

- **KDE Plasma 6 (KWin)**: works out of the box. The daemon loads a small KWin
  script (`crates/ghostportals-linux/kwin/bridge.js`) over DBus and unloads it
  on exit.
- **Hyprland**: works out of the box, through Hyprland's IPC sockets. The proxy
  window is made floating so it can be placed freely.
- **GNOME / Ubuntu (Mutter)**: install and enable the extension first:
  ```sh
  cp -r extensions/gnome/ghostportals@ghostportals.io ~/.local/share/gnome-shell/extensions/
  # log out and back in (Wayland), then:
  gnome-extensions enable ghostportals@ghostportals.io
  ```

## Windows

```powershell
cargo build --release -p ghostportals-windows
.\target\release\ghostportals-windows.exe watch
.\target\release\ghostportals-windows.exe run --connect linux-box:47800 --proxy "VcXsrv"
```

## Both sides

- `--proxy <text>` names the local **proxy**: the window that stands in for a
  window arriving from the other machine. It matches a substring of the title
  (or, on Linux, the app id), so pick something unique. The proxy only
  *receives*. Dragging it does nothing, and the daemon warns if you try. To
  test, drag any *other* window over the edge and watch the proxy on the other
  screen slide in. Without `--proxy`, the daemon still reports outgoing windows
  but does not show incoming ones.
- Each handoff logs one `sent handoff …` / `received handoff …` line when it
  starts and one `release` line when it ends. `RUST_LOG=debug` adds every frame
  and every proxy move.
- `--token <secret>` (or `GHOSTPORTALS_TOKEN`) makes the peer prove it knows a
  shared secret. Use it on any network you do not fully trust; traffic is not
  encrypted.

## Development

```sh
cargo test --workspace
# Stand-in for the other machine:
cargo run -p ghostportals-core --example fake_peer -- listen 0.0.0.0:47800
cargo run -p ghostportals-core --example fake_peer -- sweep 127.0.0.1:47800 800x600
```

To type-check the Windows crate from Linux, add the target and run
`cargo check --target x86_64-pc-windows-gnu -p ghostportals-windows`. No linker
is needed for that.

Prebuilt `ghostportals-windows.exe` and `ghostportals-linux` binaries are on the
[releases page](https://github.com/Adam-AtlasSoftware/GhostPortals/releases).
Pushing a `v*` tag builds both binaries and publishes them as a release; tags
with a suffix (`v0.1.0-alpha.1`) are published as pre-releases.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
