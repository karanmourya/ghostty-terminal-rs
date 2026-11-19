//! `builtin` — an in-process command runner whose output is fed straight into
//! the terminal state machine, bypassing ConPTY. This lets protocols ConPTY
//! swallows (kitty graphics APC/DCS, etc.) work end-to-end for built-in
//! commands. Unknown commands are returned to the caller to spawn via ConPTY.

use crate::term::base64_encode;

pub const PROMPT: &str = "\x1b[32mghostty-rs\x1b[0m> ";

pub enum CommandResult {
    /// Bytes to feed directly into the terminal.
    Output(Vec<u8>),
    /// A command line the caller should spawn on a ConPTY.
    External(String),
    /// Leave the built-in shell.
    Exit,
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
    match cmd {
        "help" => {
            out.extend_from_slice(
                b"builtins: help, echo <text>, clear, kitty, osc8 <url> <text>, \
                  notify <text>, date, pwd, exit\r\n\
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
    }
}
