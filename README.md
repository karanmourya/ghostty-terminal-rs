# ghostty-windows

A pure-Rust terminal emulator for Windows, inspired by [Ghostty](https://ghostty.org).

Ghostty is written in Zig with native macOS (Swift/AppKit/Metal) and Linux (GTK/OpenGL) front ends.
This project asks: *what would that look like if the whole stack were Rust and the target were Windows?*
It is a working terminal — not a finished Ghostty replacement.

![status](https://img.shields.io/badge/status-MVP-orange)

```
cargo run                 # debug build, PowerShell in a ConPTY
cargo run --release       # optimised
```

## What works today

| Feature | Status |
| --- | --- |
| ConPTY-backed shell (PowerShell / cmd / pwsh / WSL) | ✅ |
| GPU rendering (`wgpu`, DX12/Vulkan via `wgpu-hal`) | ✅ |
| Text shaping + font fallback (`glyphon` / cosmic-text) | ✅ |
| ANSI 16 / 256 / truecolor SGR | ✅ |
| Bold, underline, inverse video | ✅ (underline via quads) |
| CJK / double-width glyphs, emoji | ✅ |
| Cursor movement, erase in line/display, insert/delete | ✅ |
| Alternate screen (`?1049`, vim/less/htop) | ✅ |
| App cursor keys (`?1`) | ✅ |
| Window title from `OSC 0/2` | ✅ |
| Cursor position / device attribute replies (`DSR`, `DA`) | ✅ |
| Keyboard incl. F1–F12, arrows, Home/End/PgUp/PgDn | ✅ |
| Ctrl+V / Ctrl+Shift+V paste | ✅ |
| Bracketed paste (`?2004`) | ✅ |
| Mouse reporting: `?1000`/`?1002`/`?1003`, SGR `?1006` and legacy X10 | ✅ |
| Focus reporting (`?1004`) | ✅ |
| Drag / double-click / triple-click selection, copy-on-select, Ctrl+Shift+C | ✅ |
| OSC 52 clipboard (write, `?` read-query; gated by `clipboard-write`/`clipboard-read`) | ✅ |
| Synchronized output (`?2026`) with a 2 s stuck-sync safety valve | ✅ |
| Scroll regions (`DECSTBM`); region scrolls never pollute history | ✅ |
| OSC 8 hyperlinks — pointer cursor on hover, Ctrl+click opens http(s) | ✅ |
| Scrollback (10 000 lines) + mouse wheel, Shift+PgUp/PgDn/Home/End | ✅ (overlay scrollbar) |
| **Resize reflow** — soft-wrapped lines are joined and re-wrapped on resize | ✅ (wide glyphs never split) |
| DPI scaling | ✅ |
| Config file, themes, font size/family, padding, cursor style | ✅ |
| Unit tests for the VT state machine and config parser | ✅ (`cargo test`) |

## Scrolling

The primary screen keeps up to 10 000 scrolled-off lines. The alternate screen
(vim, less, htop) never writes to history, so full-screen apps behave normally.

| Input | Action |
| --- | --- |
| Mouse wheel | 3 lines per notch |
| Shift+PageUp / Shift+PageDown | One page |
| Shift+Home / Shift+End | Top of history / live screen |
| Any keystroke | Jumps back to the live screen |

While scrolled back, a thin overlay scrollbar appears on the right and the
viewport stays anchored if new output arrives.

## Hyperlinks

Apps that emit OSC 8 hyperlinks (most modern CLIs, `grep --hyperlink`, cargo
diagnostics) get a pointer cursor over their links. **Ctrl+click** opens one in
the default browser. Only `http://` and `https://` URLs are launched, so
terminal output cannot trigger arbitrary protocol handlers.

## Resize reflow

Resizing the window re-wraps the content instead of cropping it. While text is
written, the terminal remembers which lines ended in a soft wrap; on a resize
those fragments are joined back into their logical lines and re-wrapped at the
new width — including everything already in scrollback. Double-width glyphs
(CJK, emoji) are never split across a line break, and reflowing an old
alternate screen (e.g. you resized while vim was up) restores the primary
screen correctly too.

## Mouse

Applications that enable xterm mouse tracking receive clicks, drags, wheel
events, and modifier bits (Ctrl/Alt) as standard reports. Shift is reserved for
the terminal: **Shift+click/drag/wheel** always means "scroll or select locally"
rather than "send this to the app".

Both encodings are supported: SGR (`?1006`, the default for vim, tmux, less,
htop) and legacy X10 bytes for older programs. All three tracking modes are
implemented: `?1000` (clicks), `?1002` (drag), `?1003` (any motion).

## Selection

Drag with the left button to select, double-click for a word, triple-click for
a whole line. The selection is
highlighted behind the text, stays attached to its lines while you scroll, and
is copied to the clipboard when you release the mouse. Ctrl+Shift+C copies it
again; typing or clicking elsewhere clears it. When an application has enabled
mouse reporting, hold **Shift** to select instead of forwarding the drag.

Drag past the top or bottom edge of the window and the viewport auto-scrolls
through history (and back down) until you release.

## Clipboard (OSC 52)

Applications can set the clipboard with `OSC 52 ; c ; <base64>` and read it
back with `OSC 52 ; c ; ?`. Both are enabled by default and can be turned off
with `clipboard-write = false` / `clipboard-read = false` in the config file.

## What is missing (the honest list)

- Tabs, splits, multiple windows.
- OSC 7 cwd.
- Search in scrollback. (Full buffer can be copied with **Ctrl+Shift+A**.)
- Server/daemon mode, shell integration, notifications, kitty graphics protocol.

## Architecture

The layer boundaries deliberately mirror Ghostty's, so behaviour maps 1:1 even
though the code is a clean-room Rust implementation rather than a translation.

| Ghostty | ghostty-windows | File |
| --- | --- | --- |
| `src/terminal`, `src/vt` (Zig) | `vte` crate + grid state machine | `src/term.rs` |
| macOS AppKit / Linux GTK app | `winit` | `src/main.rs` |
| Metal / OpenGL renderer | `wgpu` + instanced quad pipeline | `src/quad.rs`, `src/quad.wgsl` |
| CoreText | `glyphon` (cosmic-text / swash) | `src/main.rs` |
| macOS PTY / Linux pty | `portable-pty` (ConPTY) | `src/main.rs` |
| `~/.config/ghostty/config` | flat `key = value` config file | `src/config.rs` |

Rendering happens on a single GPU pass:

1. Clear to the default background.
2. Draw one instanced rectangle per cell that has a non-default background
   (plus underlines and the cursor outline).
3. Draw all glyphs with glyphon from one shaped buffer **per visible row**
   (rich text spans for per-run colors). A row's content fingerprint is cached,
   so a keystroke re-shapes only the rows that changed instead of the whole
   screen.

Text shaping uses generous buffer width so cosmic-text never soft-wraps; the
terminal grid owns line breaking.

## Configuration

The config file uses Ghostty's flat `key = value` format. It is loaded from
`%APPDATA%\ghostty-windows\config.toml`, or from `GHOSTTY_WIN_CONFIG` when
that variable is set:

```toml
theme = catppuccin-mocha   # default | light | catppuccin-mocha
font-size = 15
font-family = "Cascadia Mono"
padding = 6
cursor-style = block       # block | bar | underline
scrollback-limit = 10000

background = "#1e1e2e"    # explicit colours override themes
foreground = "#cdd6f4"
palette = 3=#f9e2af        # remap one ANSI colour
```

See [`examples/config.toml`](examples/config.toml) for the full list. Unknown
keys or bad values print a warning to stderr but never prevent startup.

| Variable | Default | Meaning |
| --- | --- | --- |
| `GHOSTTY_WIN_SHELL` | `powershell.exe` | Shell command, arguments allowed: `pwsh.exe -NoLogo`, `wsl.exe`, `cmd.exe` |
| `GHOSTTY_WIN_CONFIG` | `%APPDATA%\ghostty-windows\config.toml` | Config file path |

```powershell
$env:GHOSTTY_WIN_SHELL = "pwsh.exe -NoLogo"
cargo run --release
```

## Investigate / test

```powershell
cargo test                  # VT state machine unit tests
cargo clippy --all-targets  # lints
```

Visual smoke test (colors, styles, wide glyphs, box drawing):

```powershell
$env:GHOSTTY_WIN_SHELL = "powershell.exe -NoLogo -NoExit -File C:\Ghostty-windows\examples\render-test.ps1"
cargo run
```

Mouse reporting (needs WSL; each click prints `^[[<button;col;rowM`):

```powershell
$env:GHOSTTY_WIN_SHELL = "wsl.exe -e bash /mnt/c/Ghostty-windows/examples/mouse-test.sh"
cargo run --release
```

## Roadmap to something you could daily-drive

1. ~~Scrollback + scroll wheel~~ — done.
2. ~~Mouse reporting~~ — done.
3. ~~Selection + clipboard~~ — done.
4. ~~Config~~ — done.
5. ~~Clipboard polish~~ — OSC 52 and auto-scroll done (triple-click included).
6. ~~Resize reflow~~ — soft-wrap join + re-wrap, verified live.
7. **Tabs/splits** — requires a window manager layer above `winit` windows.
8. ~~Performance pass~~ — per-row shaped buffers + content fingerprint cache (dirty rows only), done. Frame pacing left if needed.

## Legal

Ghostty is MIT-licensed, © Mitchell Hashimoto and contributors.
This project is an independent implementation, not affiliated with or endorsed
by the Ghostty project. It reuses no Ghostty source code.

Licensed under MIT — see [LICENSE](LICENSE).
