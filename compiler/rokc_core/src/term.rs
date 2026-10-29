//! Terminal output for rok, rokc and rokls (spec 9.1.8): colour, status
//! lines, labels and lists. Colour is decided once per stream.

use std::io::IsTerminal;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stream {
    Stdout,
    Stderr,
}

/// `--color`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ColorChoice {
    #[default]
    Auto,
    Always,
    Never,
}

impl ColorChoice {
    /// `auto|always|never`.
    #[must_use]
    pub fn parse(v: &str) -> Option<Self> {
        match v {
            "auto" => Some(Self::Auto),
            "always" => Some(Self::Always),
            "never" => Some(Self::Never),
            _ => None,
        }
    }
}

/// How many colours a stream gets.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Depth {
    Off,
    Ansi16,
    Ansi256,
    True,
}

impl Depth {
    #[must_use]
    pub const fn code(self) -> u8 {
        self as u8
    }

    #[must_use]
    pub const fn from_code(c: u8) -> Self {
        match c {
            1 => Self::Ansi16,
            2 => Self::Ansi256,
            3 => Self::True,
            _ => Self::Off,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// Status verbs (`Compiling`).
    Status,
    Error,
    Warning,
    Note,
    Help,
    /// A diagnostic's `-->`, `|` and line numbers.
    Gutter,
    /// Bold, no colour.
    Emph,
    /// A list's current entry mark.
    Current,
    Keyword,
    Type,
    Str,
    Number,
    Comment,
    Attribute,
}

pub const RESET: &str = "\x1b[0m";

static CHOICE: AtomicU8 = AtomicU8::new(0);
static DEPTH: [OnceLock<Depth>; 2] = [OnceLock::new(), OnceLock::new()];

/// Set from `--color`, before anything is printed.
pub fn set_choice(c: ColorChoice) {
    CHOICE.store(c as u8, Ordering::Relaxed);
}

fn choice() -> ColorChoice {
    match CHOICE.load(Ordering::Relaxed) {
        1 => ColorChoice::Always,
        2 => ColorChoice::Never,
        _ => ColorChoice::Auto,
    }
}

/// The colour depth of `s`, decided on first use.
#[must_use]
pub fn depth(s: Stream) -> Depth {
    *DEPTH[s as usize].get_or_init(|| {
        let tty = match s {
            Stream::Stdout => std::io::stdout().is_terminal(),
            Stream::Stderr => std::io::stderr().is_terminal(),
        };
        let d = decide(choice(), |k| std::env::var(k).ok(), tty);
        // A console that takes no escapes gets plain text.
        if d != Depth::Off && tty && !enable_vt(s) {
            return Depth::Off;
        }
        d
    })
}

#[must_use]
pub fn enabled(s: Stream) -> bool {
    depth(s) != Depth::Off
}

/// The depth colour would have if on (`--color=always`).
#[must_use]
pub fn forced_depth() -> Depth {
    decide(ColorChoice::Always, |k| std::env::var(k).ok(), true)
}

/// Whether to colour a stream, and how deep.
///
/// `--color`, then `NO_COLOR`, then `CLICOLOR_FORCE`, then `TERM=dumb`,
/// then whether the stream is a terminal. `COLORTERM=truecolor|24bit`
/// gives 24-bit colour, `TERM=*256color*` 256 colours, else 16.
pub fn decide(choice: ColorChoice, env: impl Fn(&str) -> Option<String>, tty: bool) -> Depth {
    let set = |k: &str| env(k).filter(|v| !v.is_empty());
    let on = match choice {
        ColorChoice::Never => false,
        ColorChoice::Always => true,
        ColorChoice::Auto => {
            if set("NO_COLOR").is_some() {
                false
            } else if set("CLICOLOR_FORCE").is_some_and(|v| v != "0") {
                true
            } else if set("TERM").as_deref() == Some("dumb") {
                false
            } else {
                tty
            }
        }
    };
    if !on {
        return Depth::Off;
    }
    let term = set("TERM").unwrap_or_default();
    match set("COLORTERM").as_deref() {
        Some("truecolor" | "24bit") => Depth::True,
        _ if term.contains("256color") => Depth::Ansi256,
        _ => Depth::Ansi16,
    }
}

/// The SGR sequence for `r` at `d`; empty when off.
#[must_use]
pub const fn sgr(d: Depth, r: Role) -> &'static str {
    use Depth::{Ansi16, Ansi256, Off, True};
    match (d, r) {
        (Off, _) => "",
        (_, Role::Emph) => "\x1b[1m",
        (True, Role::Status | Role::Gutter | Role::Current) => "\x1b[1;38;2;56;189;248m",
        (Ansi256, Role::Status | Role::Gutter | Role::Current) => "\x1b[1;38;5;81m",
        (Ansi16, Role::Status | Role::Gutter | Role::Current) => "\x1b[1;96m",
        (True, Role::Error) => "\x1b[1;38;2;255;95;95m",
        (Ansi256, Role::Error) => "\x1b[1;38;5;203m",
        (Ansi16, Role::Error) => "\x1b[1;91m",
        (True, Role::Warning) => "\x1b[1;38;2;224;176;87m",
        (Ansi256, Role::Warning) => "\x1b[1;38;5;179m",
        (Ansi16, Role::Warning) => "\x1b[1;33m",
        (True, Role::Note | Role::Type) => "\x1b[38;2;56;189;248m",
        (Ansi256, Role::Note | Role::Type) => "\x1b[38;5;81m",
        (Ansi16, Role::Note | Role::Type) => "\x1b[96m",
        (True, Role::Help | Role::Keyword | Role::Attribute) => "\x1b[38;2;249;168;212m",
        (Ansi256, Role::Help | Role::Keyword | Role::Attribute) => "\x1b[38;5;218m",
        (Ansi16, Role::Help | Role::Keyword | Role::Attribute) => "\x1b[95m",
        (True, Role::Number) => "\x1b[38;2;224;176;87m",
        (Ansi256, Role::Number) => "\x1b[38;5;179m",
        (Ansi16, Role::Number) => "\x1b[33m",
        (True | Ansi256, Role::Str) => "\x1b[38;5;114m",
        (Ansi16, Role::Str) => "\x1b[32m",
        (True | Ansi256, Role::Comment) => "\x1b[38;5;244m",
        (Ansi16, Role::Comment) => "\x1b[90m",
    }
}

/// `text` in `r` at depth `d`.
#[must_use]
pub fn paint_in(d: Depth, r: Role, text: &str) -> String {
    if d == Depth::Off || text.is_empty() {
        text.to_owned()
    } else {
        format!("{}{text}{RESET}", sgr(d, r))
    }
}

/// `text` in `r` on stream `s`.
#[must_use]
pub fn paint(s: Stream, r: Role, text: &str) -> String {
    paint_in(depth(s), r, text)
}

/// A status line on stderr: `verb` right-aligned to 12 columns, then
/// `msg`.
pub fn status(verb: &str, msg: &str) {
    eprintln!("{}", status_in(depth(Stream::Stderr), verb, msg));
}

#[must_use]
pub fn status_in(d: Depth, verb: &str, msg: &str) -> String {
    let verb = paint_in(d, Role::Status, &format!("{verb:>12}"));
    if msg.is_empty() {
        verb
    } else {
        format!("{verb} {msg}")
    }
}

/// `note: msg` on stderr, `note` aligned with status verbs.
pub fn note(msg: &str) {
    let l = paint(Stream::Stderr, Role::Note, &format!("{:>12}:", "note"));
    eprintln!("{l} {msg}");
}

/// `error:`, `warning:`, `note:` or `help:` for `s`; the text is there
/// with or without colour.
#[must_use]
pub fn label(s: Stream, r: Role) -> String {
    label_in(depth(s), r)
}

#[must_use]
pub fn label_in(d: Depth, r: Role) -> String {
    let word = match r {
        Role::Error => "error:",
        Role::Warning => "warning:",
        Role::Help => "help:",
        _ => "note:",
    };
    paint_in(d, r, word)
}

/// `rows` (name, detail) for stdout, details aligned. With a `mark`
/// (`(default)`), each row starts with a column holding `*` on the
/// `current` row, which also ends in `mark`.
#[must_use]
pub fn list(rows: &[(String, String)], current: Option<usize>, mark: &str) -> String {
    list_in(depth(Stream::Stdout), rows, current, mark)
}

#[must_use]
pub fn list_in(d: Depth, rows: &[(String, String)], current: Option<usize>, mark: &str) -> String {
    let width = rows
        .iter()
        .filter(|r| !r.1.is_empty())
        .map(|r| r.0.chars().count())
        .max()
        .unwrap_or(0);
    let mut out = String::new();
    for (i, (name, detail)) in rows.iter().enumerate() {
        let here = current == Some(i);
        if !mark.is_empty() {
            out.push_str(&if here {
                paint_in(d, Role::Current, "*")
            } else {
                " ".to_owned()
            });
            out.push(' ');
        }
        if detail.is_empty() {
            out.push_str(name);
        } else {
            out.push_str(&format!("{name:<width$} {detail}"));
        }
        if here && !mark.is_empty() {
            out.push(' ');
            out.push_str(mark);
        }
        out.push('\n');
    }
    out
}

/// Turn on escape processing for a Windows console; false if it would
/// not take it.
#[cfg(windows)]
fn enable_vt(s: Stream) -> bool {
    unsafe extern "system" {
        fn GetStdHandle(which: u32) -> isize;
        fn GetConsoleMode(h: isize, mode: *mut u32) -> i32;
        fn SetConsoleMode(h: isize, mode: u32) -> i32;
    }
    const STD_OUTPUT_HANDLE: u32 = 0xFFFF_FFF5;
    const STD_ERROR_HANDLE: u32 = 0xFFFF_FFF4;
    const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 4;
    let which = match s {
        Stream::Stdout => STD_OUTPUT_HANDLE,
        Stream::Stderr => STD_ERROR_HANDLE,
    };
    // SAFETY: a standard handle and a local out-parameter.
    unsafe {
        let h = GetStdHandle(which);
        let mut m = 0;
        // Not a console (an MSYS pty): it reads escapes itself.
        if GetConsoleMode(h, &raw mut m) == 0 {
            return true;
        }
        m & ENABLE_VIRTUAL_TERMINAL_PROCESSING != 0
            || SetConsoleMode(h, m | ENABLE_VIRTUAL_TERMINAL_PROCESSING) != 0
    }
}

#[cfg(not(windows))]
const fn enable_vt(_: Stream) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::{ColorChoice as C, Depth, Role, decide, label_in, list_in, sgr, status_in};

    fn env<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        |k| {
            vars.iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| (*v).to_owned())
        }
    }

    #[test]
    fn decide_matrix() {
        let xterm = [("TERM", "xterm")];
        assert_eq!(decide(C::Auto, env(&xterm), true), Depth::Ansi16);
        assert_eq!(decide(C::Auto, env(&xterm), false), Depth::Off);
        assert_eq!(decide(C::Auto, env(&[]), true), Depth::Ansi16);
        // The flag beats everything.
        let all = [("NO_COLOR", "1"), ("CLICOLOR_FORCE", "1"), ("TERM", "dumb")];
        assert_eq!(decide(C::Always, env(&all), false), Depth::Ansi16);
        assert_eq!(decide(C::Never, env(&xterm), true), Depth::Off);
        // NO_COLOR beats CLICOLOR_FORCE; empty is unset.
        assert_eq!(decide(C::Auto, env(&all), true), Depth::Off);
        let empty = [("NO_COLOR", ""), ("TERM", "xterm")];
        assert_eq!(decide(C::Auto, env(&empty), true), Depth::Ansi16);
        // CLICOLOR_FORCE beats TERM=dumb and a pipe; `0` is off.
        let force = [("CLICOLOR_FORCE", "1"), ("TERM", "dumb")];
        assert_eq!(decide(C::Auto, env(&force), false), Depth::Ansi16);
        let zero = [("CLICOLOR_FORCE", "0")];
        assert_eq!(decide(C::Auto, env(&zero), false), Depth::Off);
        assert_eq!(decide(C::Auto, env(&[("TERM", "dumb")]), true), Depth::Off);
        // Depth.
        let t256 = [("TERM", "xterm-256color")];
        assert_eq!(decide(C::Auto, env(&t256), true), Depth::Ansi256);
        for ct in ["truecolor", "24bit"] {
            let tc = [("TERM", "xterm-256color"), ("COLORTERM", ct)];
            assert_eq!(decide(C::Auto, env(&tc), true), Depth::True);
        }
        let other = [("TERM", "xterm"), ("COLORTERM", "yes")];
        assert_eq!(decide(C::Auto, env(&other), true), Depth::Ansi16);
    }

    #[test]
    fn sgr_table_per_depth() {
        assert_eq!(sgr(Depth::Off, Role::Error), "");
        let row = |r| [Depth::True, Depth::Ansi256, Depth::Ansi16].map(|d| sgr(d, r));
        assert_eq!(
            row(Role::Status),
            ["\x1b[1;38;2;56;189;248m", "\x1b[1;38;5;81m", "\x1b[1;96m"]
        );
        assert_eq!(
            row(Role::Error),
            ["\x1b[1;38;2;255;95;95m", "\x1b[1;38;5;203m", "\x1b[1;91m"]
        );
        assert_eq!(
            row(Role::Warning),
            ["\x1b[1;38;2;224;176;87m", "\x1b[1;38;5;179m", "\x1b[1;33m"]
        );
        assert_eq!(
            row(Role::Note),
            ["\x1b[38;2;56;189;248m", "\x1b[38;5;81m", "\x1b[96m"]
        );
        assert_eq!(
            row(Role::Help),
            ["\x1b[38;2;249;168;212m", "\x1b[38;5;218m", "\x1b[95m"]
        );
        assert_eq!(row(Role::Keyword), row(Role::Help));
        assert_eq!(row(Role::Type), row(Role::Note));
        assert_eq!(
            row(Role::Str),
            ["\x1b[38;5;114m", "\x1b[38;5;114m", "\x1b[32m"]
        );
        assert_eq!(
            row(Role::Comment),
            ["\x1b[38;5;244m", "\x1b[38;5;244m", "\x1b[90m"]
        );
        for d in [Depth::Off, Depth::Ansi16, Depth::Ansi256, Depth::True] {
            assert_eq!(Depth::from_code(d.code()), d);
        }
    }

    #[test]
    fn status_and_labels() {
        assert_eq!(status_in(Depth::Off, "Created", "x"), "     Created x");
        assert_eq!(
            status_in(Depth::Ansi16, "Created", "x"),
            "\x1b[1;96m     Created\x1b[0m x"
        );
        assert_eq!(label_in(Depth::Off, Role::Error), "error:");
        assert_eq!(
            label_in(Depth::Ansi16, Role::Warning),
            "\x1b[1;33mwarning:\x1b[0m"
        );
    }

    #[test]
    fn list_marks_current_without_colour() {
        let rows = |r: &[(&str, &str)]| -> Vec<(String, String)> {
            r.iter()
                .map(|(a, b)| ((*a).to_owned(), (*b).to_owned()))
                .collect()
        };
        let tcs = rows(&[("0.1.4-x", ""), ("0.1.5-x", "")]);
        assert_eq!(
            list_in(Depth::Off, &tcs, Some(1), "(default)"),
            "  0.1.4-x\n* 0.1.5-x (default)\n"
        );
        assert_eq!(
            list_in(Depth::Ansi16, &tcs, Some(0), "(default)"),
            "\x1b[1;96m*\x1b[0m 0.1.4-x (default)\n  0.1.5-x\n"
        );
        assert_eq!(
            list_in(Depth::Off, &tcs, None, "(default)"),
            "  0.1.4-x\n  0.1.5-x\n"
        );
        let kv = rows(&[("host:", "x"), ("llvm:", "none"), ("commit-hash:", "abc")]);
        assert_eq!(
            list_in(Depth::Off, &kv, None, ""),
            "host:        x\nllvm:        none\ncommit-hash: abc\n"
        );
    }
}
