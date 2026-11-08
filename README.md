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
| Scrollback (10 000 lines) + mouse wheel, Shift+PgUp/PgDn/Home/End | ✅ (overlay scrollbar) |
| DPI scaling | ✅ |
| Unit tests for the VT state machine | ✅ (`cargo test`) |

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

## Mouse

Applications that enable xterm mouse tracking receive clicks, drags, wheel
events, and modifier bits (Ctrl/Alt) as standard reports. Shift is reserved for
the terminal: **Shift+click/drag/wheel** always means "scroll or select locally"
rather than "send this to the app".

Both encodings are supported: SGR (`?1006`, the default for vim, tmux, less,
htop) and legacy X10 bytes for older programs. All three tracking modes are
implemented: `?1000` (clicks), `?1002` (drag), `?1003` (any motion).

## What is missing (the honest list)

- Tabs, splits, multiple windows.
- Text selection and copy (Ctrl+Shift+C), OSC 52 clipboard.
- OSC 8 hyperlinks, OSC 7 cwd.
- Synchronized output (`?2026`).
- Search, scrollback export.
- Config file / themes / font selection (font is the system monospace at 15px).
- Reflow on resize (content is preserved top-left, not rewrapped).
- Scroll regions (`DECSTBM`), tab stops beyond every 8 columns.
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

Rendering happens on a single GPU pass:

1. Clear to the default background.
2. Draw one instanced rectangle per cell that has a non-default background
   (plus underlines and the cursor outline).
3. Draw all glyphs with glyphon from **one** shaped buffer (rich text spans for
   per-run colors, so a screen costs one shaping pass, not one per cell).

Text shaping uses generous buffer width so cosmic-text never soft-wraps; the
terminal grid owns line breaking.

## Configuration

| Variable | Default | Meaning |
| --- | --- | --- |
| `GHOSTTY_WIN_SHELL` | `powershell.exe` | Shell command, arguments allowed: `pwsh.exe -NoLogo`, `wsl.exe`, `cmd.exe` |

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
3. **Selection + clipboard** — drag to select, Ctrl+Shift+C, and OSC 52.
4. **Config** — TOML file for font family/size, theme colors, padding, cursor style.
5. **Resize reflow** — rewrap logical lines instead of cropping.
6. **Tabs/splits** — requires a window manager layer above `winit` windows.
7. **Performance pass** — dirty-row tracking, glyph cache reuse, frame pacing.

## Legal

Ghostty is MIT-licensed, © Mitchell Hashimoto and contributors.
This project is an independent implementation, not affiliated with or endorsed
by the Ghostty project. It reuses no Ghostty source code.

Licensed under MIT — see [LICENSE](LICENSE).
