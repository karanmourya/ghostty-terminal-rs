//! `builtin` — an in-process command runner whose output is fed straight into
//! the terminal state machine, bypassing ConPTY. This lets protocols ConPTY
//! swallows (kitty graphics APC/DCS, etc.) work end-to-end for built-in
//! commands. Unknown commands are returned to the caller to spawn via ConPTY.

use crate::term::base64_encode;

pub const PROMPT: &str = "\x1b[32mghostty-rs\x1b[0m> ";

pub enum CommandResult {
    /// Bytes to feed directly into the terminal.
    Output(Vec<u8>),
    /// A command line the caller should spawn on a ConPTY (full TTY semantics).
    External(String),
    /// A command line to spawn on a raw pipe. The child's stdout/stderr bytes
    /// are fed straight into the terminal, so escape sequences ConPTY would
    /// swallow (APC/DCS kitty graphics, etc.) survive.
    Pipe(String),
    /// Leave the built-in shell.
    Exit,
}

fn png_size(bytes: &[u8]) -> Option<(u32, u32)> {
    // PNG IHDR: width at bytes 16..20, height at 20..24 (big-endian).
    if bytes.len() < 24 || &bytes[..8] != b"\x89PNG\r\n\x1a\n" {
        return None;
    }
    let w = u32::from_be_bytes(bytes[16..20].try_into().ok()?);
    let h = u32::from_be_bytes(bytes[20..24].try_into().ok()?);
    Some((w, h))
}

/// Read a PNG from disk and emit a kitty transmit + place sequence. The image
/// is stretched to `cols x rows` cells; when omitted, rows follow the aspect
/// ratio (terminal cells are about twice as tall as wide).
fn img_command(args: &str) -> Vec<u8> {
    // The path may be quoted because Windows paths often contain spaces.
    let args = args.trim();
    let (path, rest) = if let Some(rest) = args.strip_prefix('"') {
        match rest.find('"') {
            Some(end) => (&rest[..end], rest[end + 1..].trim()),
            None => (args, ""),
        }
    } else if let Some(rest) = args.strip_prefix('\'') {
        match rest.find('\'') {
            Some(end) => (&rest[..end], rest[end + 1..].trim()),
            None => (args, ""),
        }
    } else {
        match args.split_once(char::is_whitespace) {
            Some((p, r)) => (p, r.trim()),
            None => (args, ""),
        }
    };
    let mut words = rest.split_whitespace();
    if path.is_empty() {
        return b"usage: img <path.png> [cols] [rows]\r\n".to_vec();
    }
    let png = match std::fs::read(path) {
        Ok(p) => p,
        Err(err) => return format!("img: cannot read {path}: {err}\r\n").into_bytes(),
    };
    let (w, h) = match png_size(&png) {
        Some(s) => s,
        None => return b"img: not a PNG file\r\n".to_vec(),
    };
    let cols: u32 = words
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(80)
        .clamp(1, 200);
    let rows: u32 = words
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| ((cols as f64 * h as f64 / w as f64 / 2.0).round() as u32).clamp(1, 60))
        .clamp(1, 60);
    let b64 = base64_encode(&png);
    let mut out = Vec::with_capacity(b64.len() + 96);
    out.extend_from_slice(format!("\x1b_Ga=T,f=100,i=7,d={b64}\x1b\\").as_bytes());
    out.extend_from_slice(format!("\x1b_Ga=p,i=7,c={cols},r={rows},C=1,R=1\x1b\\").as_bytes());
    // Move below the image so the next prompt does not overlap.
    for _ in 0..=rows {
        out.extend_from_slice(b"\r\n");
    }
    out
}

fn kitty_demo() -> Vec<u8> {
    // 8x8 checkerboard in raw RGBA (f=32), so no PNG encoder is needed.
    let mut rgba = Vec::with_capacity(8 * 8 * 4);
    for y in 0..8 {
        for x in 0..8 {
            if (x + y) % 2 == 0 {
                rgba.extend_from_slice(&[220, 80, 120, 255]);
            } else {
                rgba.extend_from_slice(&[80, 180, 255, 255]);
            }
        }
    }
    let b64 = base64_encode(&rgba);
    let mut out = Vec::new();
    out.extend_from_slice(format!("\x1b_Ga=T,f=32,i=1,w=8,h=8,d={b64}\x1b\\").as_bytes());
    // Place 8 cols x 4 rows starting at the cursor, then move below it.
    out.extend_from_slice(b"\x1b_Ga=p,i=1,c=8,r=4,C=1,R=1\x1b\\");
    // Move the cursor below the 4-row image so the next prompt does not overlap.
    out.extend_from_slice(b"\r\n\r\n\r\n\r\n\r\n\r\n");
    out
}

/// Run one built-in command line.
pub fn run(line: &str) -> CommandResult {
    let line = line.trim();
    if line.is_empty() {
        return CommandResult::Output(Vec::new());
    }
    let mut parts = line.splitn(2, char::is_whitespace);
    let cmd = parts.next().unwrap_or("");
    let rest = parts.next().unwrap_or("").trim();
    let mut out = Vec::new();
    if cmd == "pipe" {
        return CommandResult::Pipe(rest.to_string());
    }
    match cmd {
        "help" => {
            out.extend_from_slice(
                b"builtins: help, echo <text>, clear, kitty, img <file.png> [cols] [rows], \
                  osc8 <url> <text>, notify <text>, date, pwd, exit\r\n\
                  pipe <cmd>  spawn on a raw pipe (preserves kitty APC/DCS)\r\n\
                  anything else is spawned on ConPTY\r\n",
            );
        }
        "echo" => {
            out.extend_from_slice(rest.as_bytes());
            out.extend_from_slice(b"\r\n");
        }
        "clear" => {
            out.extend_from_slice(b"\x1b[2J\x1b[H");
        }
        "kitty" => {
            out.extend_from_slice(&kitty_demo());
        }
        "img" => {
            out.extend_from_slice(&img_command(rest));
        }
        "osc8" => {
            let mut words = rest.splitn(2, char::is_whitespace);
            let url = words.next().unwrap_or("https://example.com");
            let text = words.next().unwrap_or(url);
            out.extend_from_slice(
                format!("\x1b]8;;{url}\x1b\\{text}\x1b]8;;\x1b\\\r\n").as_bytes(),
            );
        }
        "notify" => {
            out.extend_from_slice(format!("\x1b]9;{rest}\x07").as_bytes());
        }
        "date" => {
            // Seconds since epoch; no date crate needed for a smoke command.
            let secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            out.extend_from_slice(format!("epoch {secs}\r\n").as_bytes());
        }
        "pwd" => {
            let dir = std::env::current_dir()
                .map(|d| d.display().to_string())
                .unwrap_or_else(|_| "?".into());
            out.extend_from_slice(format!("{dir}\r\n").as_bytes());
        }
        "exit" => return CommandResult::Exit,
        _ => {
            // Unknown command: let the caller spawn it on ConPTY, preserving
            // the original line (including arguments).
            return CommandResult::External(line.to_string());
        }
    }
    CommandResult::Output(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kitty_demo_feeds_into_term() {
        use crate::term::Term;
        let out = match run("kitty") {
            CommandResult::Output(b) => b,
            _ => panic!("kitty should be builtin"),
        };
        let mut t = Term::new(80, 24);
        let mut parser = vte::Parser::new();
        t.feed(&mut parser, &out);
        assert_eq!(t.kitty_images().count(), 1, "image should decode");
        assert_eq!(t.kitty_placements().len(), 1, "placement should register");
        let img = t.kitty_images().next().unwrap();
        assert_eq!((img.width, img.height), (8, 8));
        assert!(
            img.rgba.iter().any(|&v| v != 0),
            "pixels should be non-empty"
        );
    }

    #[test]
    fn img_command_embeds_png() {
        let mut png = Vec::new();
        image::RgbaImage::from_raw(1, 1, vec![0, 255, 0, 255])
            .unwrap()
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let path = std::env::temp_dir().join("ghostty_test_img.png");
        std::fs::write(&path, &png).unwrap();
        let arg = format!("{} 80 3", path.to_string_lossy());
        let out = img_command(&arg);
        let s = String::from_utf8_lossy(&out);
        assert!(s.contains("\x1b_Ga=T,f=100,i=7,d="));
        assert!(s.contains("\x1b_Ga=p,i=7,c=80,r=3,C=1,R=1\x1b\\"));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn builtin_commands() {
        match run("echo hi") {
            CommandResult::Output(b) => assert_eq!(b, b"hi\r\n"),
            _ => panic!("echo should be builtin"),
        }
        match run("kitty") {
            CommandResult::Output(b) => {
                let s = String::from_utf8_lossy(&b);
                assert!(s.contains("\x1b_Ga=T,f=32,i=1,w=8,h=8,d="));
                assert!(s.contains("\x1b_Ga=p,i=1,c=8,r=4"));
            }
            _ => panic!("kitty should be builtin"),
        }
        match run("git status") {
            CommandResult::External(c) => assert_eq!(c, "git status"),
            _ => panic!("unknown command should be external"),
        }
        match run("pipe echo hi") {
            CommandResult::Pipe(c) => assert_eq!(c, "echo hi"),
            _ => panic!("pipe command should use raw pipe"),
        }
    }
}
