//! Terminal line editing for the REPL: raw termios (Windows: VT console
//! modes), cursor keys, Emacs keys (`C-a` `C-e` `C-b` `C-f` `C-k` `C-u`
//! `C-w` `C-d` `C-p` `C-n` `C-l` `C-r` `C-z`, `M-b` `M-f`), Tab
//! completion, colours, bracketed paste and whole-cell history in
//! `$ROK_HOME/repl_history`. Long lines scroll sideways by display width.

mod highlight;
#[rustfmt::skip]
mod width_table;

use std::io::{Read, Write};
use std::path::PathBuf;

pub use highlight::CONTEXTUAL;
use highlight::Class;
use rokc_core::term::{Depth, Stream, depth, sgr};

/// What one `read_line` got.
#[derive(Debug, PartialEq, Eq)]
pub enum Input {
    /// A line, or a whole pasted or recalled cell (lines joined by `\n`).
    Line(String),
    /// `C-c`: drop the line (and an open multi-line cell).
    Interrupt,
    /// `C-d` on an empty line, or the end of input.
    Eof,
}

/// Tab completion: given the line and the cursor's byte offset, where the
/// word being completed starts and its candidates.
pub type Complete<'a> = dyn FnMut(&str, usize) -> (usize, Vec<String>) + 'a;

/// History entries kept on disk.
const KEEP: usize = 1000;
/// Longer cells stay out of the history file.
const MAX_ENTRY: usize = 64 * 1024;
/// First line of a history file whose entries are escaped (`\n`, `\\`);
/// files without it hold one line per entry.
const HEADER: &str = "#rok repl_history 2";
/// The prompt of a cell's later lines, as the REPL shows it.
const CONT: &str = "... ";
const PASTE_ON: &str = "\x1b[?2004h";
const PASTE_OFF: &str = "\x1b[?2004l";

pub struct Editor {
    history: Vec<String>,
    file: Option<PathBuf>,
    /// The file is missing or in the one-line format: rewrite it whole.
    rewrite: bool,
    color: Depth,
}

impl Editor {
    /// An editor with the history of earlier sessions.
    pub fn new() -> Self {
        let mut ed = Self::with_file(rokc_core::rok_home().map(|h| h.join("repl_history")));
        ed.color = depth(Stream::Stdout);
        ed
    }

    fn with_file(file: Option<PathBuf>) -> Self {
        let text = file.as_ref().and_then(|f| std::fs::read_to_string(f).ok());
        let rewrite = !text.as_deref().is_some_and(|t| t.starts_with(HEADER));
        let history = text.as_deref().map(decode).unwrap_or_default();
        Self {
            history,
            file,
            rewrite,
            color: Depth::Off,
        }
    }

    /// Remember `cell` (and save it to the history file).
    pub fn add(&mut self, cell: &str) {
        if cell.trim().is_empty() || self.history.last().is_some_and(|l| l == cell) {
            return;
        }
        self.history.push(cell.to_owned());
        let Some(f) = &self.file else { return };
        if self.history.len() > KEEP * 2 {
            let drop = self.history.len() - KEEP;
            self.history.drain(..drop);
            self.rewrite = true;
        }
        if let Some(dir) = f.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let kept = |e: &&String| e.len() <= MAX_ENTRY;
        if self.rewrite {
            let mut text = format!("{HEADER}\n");
            for e in self.history.iter().filter(kept) {
                text.push_str(&encode(e));
                text.push('\n');
            }
            self.rewrite = std::fs::write(f, text).is_err();
        } else if cell.len() <= MAX_ENTRY {
            let _ = std::fs::OpenOptions::new()
                .append(true)
                .open(f)
                .and_then(|mut h| writeln!(h, "{}", encode(cell)));
        }
    }

    /// Read one line after `prompt`, on a line of its own; plain
    /// `read_line` when stdin is not a terminal raw mode can be set on.
    pub fn read_line(&self, prompt: &str, complete: &mut Complete<'_>) -> Input {
        use std::io::IsTerminal;
        let mut out = std::io::stdout();
        let mut raw = Raw::enter();
        let tty = raw.is_some() && out.is_terminal();
        if tty {
            let _ = write!(out, "{}{PASTE_ON}", fresh_line(columns()));
        }
        let _ = write!(out, "{prompt}");
        let _ = out.flush();
        if raw.is_none() {
            let mut line = String::new();
            return match std::io::stdin().read_line(&mut line) {
                Ok(0) | Err(_) => Input::Eof,
                Ok(_) => Input::Line(line.trim_end_matches(['\n', '\r']).to_owned()),
            };
        }
        let mut ed = Line::new(
            prompt,
            &self.history,
            if tty { self.color } else { Depth::Off },
        );
        let mut stdin = std::io::stdin().lock();
        let input = loop {
            let Some(k) = key(&mut stdin) else {
                break Input::Eof;
            };
            match ed.key(k, &mut out, columns(), complete) {
                Step::More => {}
                Step::Done(input) => break input,
                Step::Suspend => {
                    if tty {
                        let _ = write!(out, "{PASTE_OFF}");
                        let _ = out.flush();
                    }
                    drop(raw.take());
                    suspend();
                    raw = Raw::enter();
                    if tty {
                        let _ = write!(out, "{}{PASTE_ON}", fresh_line(columns()));
                    }
                    ed.draw(&mut out, columns());
                }
            }
        };
        if tty {
            let _ = write!(out, "{PASTE_OFF}");
        }
        let _ = out.flush();
        input
    }
}

/// History file text to entries.
fn decode(text: &str) -> Vec<String> {
    let Some(rest) = text.strip_prefix(HEADER) else {
        return text.lines().map(str::to_owned).collect();
    };
    let unescape = |line: &str| {
        let mut s = String::with_capacity(line.len());
        let mut cs = line.chars();
        while let Some(c) = cs.next() {
            match (c, c == '\\') {
                (_, true) => match cs.next() {
                    Some('n') => s.push('\n'),
                    Some(o) => s.push(o),
                    None => s.push('\\'),
                },
                (c, false) => s.push(c),
            }
        }
        s
    };
    rest.lines()
        .filter(|l| !l.is_empty())
        .map(unescape)
        .collect()
}

/// An entry as one history file line.
fn encode(entry: &str) -> String {
    entry.replace('\\', "\\\\").replace('\n', "\\n")
}

/// One key (or pasted block) from the terminal.
#[derive(Debug, PartialEq, Eq)]
enum Key {
    Char(char),
    /// A control byte: `C-a` is 1, Backspace 127.
    Ctrl(u8),
    /// The rest of an escape sequence after ESC: `[A`, `[3~`, `b` (`M-b`).
    Esc(String),
    /// Text between `ESC[200~` and `ESC[201~`, line ends as `\n`.
    Paste(String),
}

fn key(r: &mut impl Read) -> Option<Key> {
    let b = byte(r)?;
    Some(match b {
        0x1b => match escape(r)? {
            s if s == "[200~" => Key::Paste(paste(r)),
            s => Key::Esc(s),
        },
        b if b < 0x20 || b == 0x7f => Key::Ctrl(b),
        b => match utf8(b, r) {
            Some(c) => Key::Char(c),
            None => Key::Ctrl(0),
        },
    })
}

/// A pasted block up to `ESC[201~` (or the end of input).
fn paste(r: &mut impl Read) -> String {
    const END: &[u8] = b"\x1b[201~";
    let mut bytes = Vec::new();
    while let Some(b) = byte(r) {
        bytes.push(b);
        if bytes.ends_with(END) {
            bytes.truncate(bytes.len() - END.len());
            break;
        }
    }
    String::from_utf8_lossy(&bytes)
        .replace("\r\n", "\n")
        .replace('\r', "\n")
}

enum Step {
    More,
    Done(Input),
    /// `C-z`: stop the job until the shell resumes it.
    Suspend,
}

/// `C-r`: the query and the history entry it matched (`len`: none yet),
/// with the line it started from.
struct Search {
    query: String,
    at: usize,
    failed: bool,
    saved: (Vec<char>, usize),
}

struct Line<'a> {
    prompt: &'a str,
    history: &'a [String],
    color: Depth,
    st: State,
    search: Option<Search>,
}

impl<'a> Line<'a> {
    const fn new(prompt: &'a str, history: &'a [String], color: Depth) -> Self {
        Self {
            prompt,
            history,
            color,
            st: State {
                buf: Vec::new(),
                pos: 0,
                hist: history.len(),
                draft: Vec::new(),
            },
            search: None,
        }
    }

    fn text(&self) -> String {
        self.st.buf.iter().collect()
    }

    fn key(
        &mut self,
        k: Key,
        out: &mut impl Write,
        width: usize,
        complete: &mut Complete<'_>,
    ) -> Step {
        if self.search.is_some() {
            match self.search_key(&k) {
                Some(()) => {
                    self.draw(out, width);
                    return Step::More;
                }
                None => self.search = None,
            }
        }
        match k {
            Key::Ctrl(b'\r' | b'\n') => {
                let _ = write!(
                    out,
                    "\r{}\x1b[K\r\n",
                    self.render(0..self.st.buf.len(), true)
                );
                let _ = out.flush();
                return Step::Done(Input::Line(self.text()));
            }
            Key::Ctrl(9) => self.complete(out, width, complete),
            Key::Ctrl(18) => {
                self.search = Some(Search {
                    query: String::new(),
                    at: self.history.len(),
                    failed: false,
                    saved: (self.st.buf.clone(), self.st.pos),
                });
            }
            k => {
                if let Some(step) = self.edit(k, out) {
                    return step;
                }
            }
        }
        self.draw(out, width);
        Step::More
    }

    /// The other keys; `Some` when the line is done.
    fn edit(&mut self, k: Key, out: &mut impl Write) -> Option<Step> {
        let st = &mut self.st;
        match k {
            Key::Ctrl(3) => {
                let _ = write!(out, "^C\r\n");
                let _ = out.flush();
                return Some(Step::Done(Input::Interrupt));
            }
            Key::Ctrl(4) if st.buf.is_empty() => {
                let _ = write!(out, "\r\n");
                return Some(Step::Done(Input::Eof));
            }
            Key::Ctrl(26) if cfg!(unix) => return Some(Step::Suspend),
            Key::Ctrl(4) => st.delete(),
            Key::Ctrl(1) => st.pos = 0,
            Key::Ctrl(5) => st.pos = st.buf.len(),
            Key::Ctrl(2) => st.pos = st.pos.saturating_sub(1),
            Key::Ctrl(6) => st.pos = (st.pos + 1).min(st.buf.len()),
            Key::Ctrl(11) => st.buf.truncate(st.pos),
            Key::Ctrl(21) => {
                st.buf.drain(..st.pos);
                st.pos = 0;
            }
            Key::Ctrl(23) => st.kill_word(),
            Key::Ctrl(12) => {
                let _ = write!(out, "\x1b[H\x1b[2J");
            }
            Key::Ctrl(16) => st.recall(self.history, -1, false),
            Key::Ctrl(14) => st.recall(self.history, 1, false),
            Key::Ctrl(8 | 127) => {
                if st.pos > 0 {
                    st.pos -= 1;
                    st.buf.remove(st.pos);
                }
            }
            Key::Ctrl(_) => {}
            Key::Esc(s) => match s.as_str() {
                "[A" | "OA" => st.recall(self.history, -1, true),
                "[B" | "OB" => st.recall(self.history, 1, true),
                "[C" | "OC" => st.pos = (st.pos + 1).min(st.buf.len()),
                "[D" | "OD" => st.pos = st.pos.saturating_sub(1),
                "[H" | "OH" | "[1~" | "[7~" => st.pos = 0,
                "[F" | "OF" | "[4~" | "[8~" => st.pos = st.buf.len(),
                "[3~" => st.delete(),
                // `M-b` / `M-f`, and Alt or Ctrl with the arrows.
                "b" | "B" | "[1;3D" | "[1;5D" => st.pos = st.word_left(),
                "f" | "F" | "[1;3C" | "[1;5C" => st.pos = st.word_right(),
                "\x7f" | "\x08" => {
                    let i = st.word_left();
                    st.buf.drain(i..st.pos);
                    st.pos = i;
                }
                "d" => {
                    let i = st.word_right();
                    st.buf.drain(st.pos..i);
                }
                _ => {}
            },
            Key::Paste(text) => {
                // Text as typed, Tab included; the cell runs on Enter.
                let text = text.trim_end_matches('\n');
                let keep = |c: &char| !c.is_control() || matches!(c, '\n' | '\t');
                for c in text.chars().filter(keep) {
                    st.insert(c);
                }
            }
            Key::Char(c) => st.insert(c),
        }
        None
    }

    /// A key while searching; `None` ends the search and the key is
    /// handled as usual (Enter then runs the match).
    fn search_key(&mut self, k: &Key) -> Option<()> {
        let s = self.search.as_mut()?;
        let found = |query: &str, from: usize| {
            (0..from.min(self.history.len()))
                .rev()
                .find(|&i| self.history[i].contains(query))
        };
        match k {
            Key::Char(c) => {
                s.query.push(*c);
                let from = (s.at + 1).min(self.history.len());
                match found(&s.query, from) {
                    Some(i) => (s.at, s.failed) = (i, false),
                    None => s.failed = true,
                }
            }
            Key::Ctrl(18) if !s.query.is_empty() => match found(&s.query, s.at) {
                Some(i) => (s.at, s.failed) = (i, false),
                None => s.failed = true,
            },
            Key::Ctrl(18) => {}
            Key::Ctrl(8 | 127) => {
                s.query.pop();
                (s.at, s.failed) = (self.history.len(), false);
                if !s.query.is_empty() {
                    match found(&s.query, self.history.len()) {
                        Some(i) => s.at = i,
                        None => s.failed = true,
                    }
                }
            }
            // `C-g` / `C-c`: back to the line the search started from.
            Key::Ctrl(7 | 3) => {
                let (buf, pos) = s.saved.clone();
                (self.st.buf, self.st.pos) = (buf, pos);
                if matches!(k, Key::Ctrl(7)) {
                    self.search = None;
                    return Some(());
                }
                return None;
            }
            _ => {
                if let Some(h) = self.history.get(s.at) {
                    self.st.buf = h.chars().collect();
                    self.st.pos = h[..h.find(&s.query).unwrap_or(0)].chars().count();
                    self.st.hist = s.at;
                }
                return None;
            }
        }
        Some(())
    }

    /// Tab: complete the word before the cursor to the candidates'
    /// common prefix; when that adds nothing, list them. At the start of
    /// a line it indents.
    fn complete(&mut self, out: &mut impl Write, width: usize, complete: &mut Complete<'_>) {
        let st = &mut self.st;
        let before: String = st.buf[..st.pos].iter().collect();
        if before.trim().is_empty() {
            for _ in 0..4 {
                st.insert(' ');
            }
            return;
        }
        let line: String = st.buf.iter().collect();
        let (start, mut found) = complete(&line, before.len());
        found.sort();
        found.dedup();
        let start = before[..start.min(before.len())].chars().count();
        let Some(first) = found.first() else {
            return;
        };
        let mut common: Vec<char> = first.chars().collect();
        for f in &found[1..] {
            let n = common
                .iter()
                .zip(f.chars())
                .take_while(|(a, b)| **a == *b)
                .count();
            common.truncate(n);
        }
        if common.len() > st.pos - start {
            st.buf.splice(start..st.pos, common.iter().copied());
            st.pos = start + common.len();
        } else if found.len() > 1 {
            let _ = write!(out, "\r\n{}", columnize(&found, width));
        }
    }

    /// What the line shows: the prompt, text and cursor (`C-r`: the query
    /// and its match).
    fn view(&self) -> (String, Vec<char>, usize) {
        let Some(s) = &self.search else {
            return (self.prompt.to_owned(), self.st.buf.clone(), self.st.pos);
        };
        let failed = if s.failed { "failed " } else { "" };
        let prompt = format!("({failed}reverse-i-search)`{}': ", s.query);
        match self.history.get(s.at) {
            Some(h) => {
                let at = h[..h.find(&s.query).unwrap_or(0)].chars().count();
                (prompt, h.chars().collect(), at)
            }
            None => (prompt, s.saved.0.clone(), s.saved.1),
        }
    }

    fn draw(&self, out: &mut impl Write, width: usize) {
        let (prompt, buf, pos) = self.view();
        let prompt_cols = str_width(&prompt);
        let shown = window(&buf, pos, prompt_cols, width);
        let col = prompt_cols
            + buf[shown.start..pos]
                .iter()
                .map(|c| cols(*c))
                .sum::<usize>();
        let line = render(&buf, shown, self.color, None);
        let _ = write!(out, "\r{prompt}{line}\x1b[K\r");
        if col > 0 {
            let _ = write!(out, "\x1b[{col}C");
        }
        let _ = out.flush();
    }

    /// The prompt and chars `range` of the line; `whole`: unscrolled, a
    /// cell's later lines after the continuation prompt.
    fn render(&self, range: std::ops::Range<usize>, whole: bool) -> String {
        let cont = whole.then_some(CONT);
        format!(
            "{}{}",
            self.prompt,
            render(&self.st.buf, range, self.color, cont)
        )
    }
}

/// Chars `range` of `buf` for the terminal, coloured at `depth`. A line
/// break is a dim `↵`, or with `cont` a new line after that prompt; Tab
/// is a space.
fn render(buf: &[char], range: std::ops::Range<usize>, depth: Depth, cont: Option<&str>) -> String {
    let color = depth != Depth::Off;
    let mut class = vec![None; buf.len()];
    if color {
        let text: String = buf.iter().collect();
        let mut at = 0;
        let mut ci = 0;
        for (r, c) in highlight::spans(&text) {
            for ch in &buf[ci..] {
                if at >= r.end {
                    break;
                }
                if at >= r.start {
                    class[ci] = Some(c);
                }
                at += ch.len_utf8();
                ci += 1;
            }
        }
    }
    let mut out = String::new();
    let mut cur: Option<Class> = None;
    for i in range {
        let c = buf[i];
        if color && class[i] != cur && c != '\n' {
            if cur.is_some() {
                out.push_str("\x1b[0m");
            }
            if let Some(k) = class[i] {
                out.push_str(sgr(depth, k.role()));
            }
            cur = class[i];
        }
        match (c, cont) {
            ('\n', Some(p)) => {
                if cur.take().is_some() {
                    out.push_str("\x1b[0m");
                }
                out.push_str("\x1b[K\r\n");
                out.push_str(p);
            }
            ('\n', None) if color => {
                if cur.take().is_some() {
                    out.push_str("\x1b[0m");
                }
                out.push_str("\x1b[2m\u{21b5}\x1b[0m");
            }
            ('\n', None) => out.push('\u{21b5}'),
            ('\t', _) => out.push(' '),
            (c, _) => out.push(c),
        }
    }
    if cur.is_some() {
        out.push_str("\x1b[0m");
    }
    out
}

/// Candidates in columns across a `width` columns wide terminal.
fn columnize(items: &[String], width: usize) -> String {
    const MAX: usize = 200;
    let col = items.iter().map(|s| str_width(s)).max().unwrap_or(0) + 2;
    let per = (width / col).max(1);
    let mut out = String::new();
    for (i, s) in items.iter().take(MAX).enumerate() {
        out.push_str(s);
        if (i + 1) % per == 0 || i + 1 == items.len().min(MAX) {
            out.push_str("\r\n");
        } else {
            out.push_str(&" ".repeat(col - str_width(s)));
        }
    }
    if items.len() > MAX {
        out.push_str(&format!("({} more)\r\n", items.len() - MAX));
    }
    out
}

struct State {
    buf: Vec<char>,
    pos: usize,
    /// History index being shown (`len`: the line being typed).
    hist: usize,
    /// The line being typed, kept while browsing history.
    draft: Vec<char>,
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

impl State {
    fn insert(&mut self, c: char) {
        self.buf.insert(self.pos, c);
        self.pos += 1;
    }

    fn delete(&mut self) {
        if self.pos < self.buf.len() {
            self.buf.remove(self.pos);
        }
    }

    fn kill_word(&mut self) {
        let mut i = self.pos;
        while i > 0 && self.buf[i - 1].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !self.buf[i - 1].is_whitespace() {
            i -= 1;
        }
        self.buf.drain(i..self.pos);
        self.pos = i;
    }

    fn word_left(&self) -> usize {
        let mut i = self.pos;
        while i > 0 && !is_word(self.buf[i - 1]) {
            i -= 1;
        }
        while i > 0 && is_word(self.buf[i - 1]) {
            i -= 1;
        }
        i
    }

    fn word_right(&self) -> usize {
        let mut i = self.pos;
        while i < self.buf.len() && !is_word(self.buf[i]) {
            i += 1;
        }
        while i < self.buf.len() && is_word(self.buf[i]) {
            i += 1;
        }
        i
    }

    /// The previous (`step` -1) or next entry; with `prefix` only those
    /// starting with the line typed before browsing, skipping the one
    /// shown.
    fn recall(&mut self, history: &[String], step: isize, prefix: bool) {
        if self.hist == history.len() {
            self.draft.clone_from(&self.buf);
        }
        let typed: String = if prefix {
            self.draft.iter().collect()
        } else {
            String::new()
        };
        let shown: String = self.buf.iter().collect();
        let mut i = self.hist;
        loop {
            let Some(next) = i.checked_add_signed(step).filter(|n| *n <= history.len()) else {
                return;
            };
            i = next;
            match history.get(i) {
                Some(h) if !h.starts_with(&typed) || (prefix && *h == shown) => {}
                _ => break,
            }
        }
        self.hist = i;
        self.buf = match history.get(i) {
            Some(h) => h.chars().collect(),
            None => self.draft.clone(),
        };
        self.pos = self.buf.len();
    }
}

/// Terminal columns of `c`: 0 for combining marks and format characters,
/// 2 for East Asian Wide / Fullwidth and emoji presentation.
pub fn width(c: char) -> usize {
    let cp = u32::from(c);
    if cp < 0x300 {
        return usize::from(cp >= 0x20 && !(0x7f..0xa0).contains(&cp));
    }
    let has = |t: &[(u32, u32)]| {
        t.binary_search_by(|&(lo, hi)| {
            if hi < cp {
                std::cmp::Ordering::Less
            } else if lo > cp {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
    };
    if has(width_table::ZERO) {
        0
    } else if has(width_table::WIDE) {
        2
    } else {
        1
    }
}

/// Columns of `c` as the line shows it (`render`).
fn cols(c: char) -> usize {
    if matches!(c, '\n' | '\t') {
        1
    } else {
        width(c)
    }
}

fn str_width(s: &str) -> usize {
    s.chars().map(cols).sum()
}

/// The chars of `buf` shown after a `prompt` columns wide prompt on a
/// `width` columns wide terminal: the cursor at `pos` stays in view.
fn window(buf: &[char], pos: usize, prompt: usize, width: usize) -> std::ops::Range<usize> {
    // The last column stays free for the cursor at the end.
    let room = width.saturating_sub(prompt + 1).max(1);
    let mut used = buf.get(pos).map_or(1, |c| cols(*c).max(1));
    let mut start = pos;
    while start > 0 && used + cols(buf[start - 1]) <= room {
        start -= 1;
        used += cols(buf[start]);
    }
    let mut end = start;
    let mut used = 0;
    while end < buf.len() && used + cols(buf[end]) <= room {
        used += cols(buf[end]);
        end += 1;
    }
    start..end.max(pos)
}

/// Move to column 0 of a fresh line without asking the terminal where
/// the cursor is (zsh `PROMPT_SP`): spaces up to the last column wrap
/// only after output that did not end its line; `\r` and clearing the
/// line then erase the rest. Nothing is drawn.
fn fresh_line(width: usize) -> String {
    format!(" {:1$}\r\x1b[K", "", width.saturating_sub(1))
}

/// The terminal's width in columns (80 when unknown).
#[cfg(unix)]
fn columns() -> usize {
    #[cfg(target_os = "macos")]
    const TIOCGWINSZ: u64 = 0x4008_7468;
    #[cfg(not(target_os = "macos"))]
    const TIOCGWINSZ: u64 = 0x5413;
    unsafe extern "C" {
        fn ioctl(fd: i32, request: u64, ...) -> i32;
    }
    // `struct winsize`: rows, columns, x / y pixels.
    let mut ws = [0u16; 4];
    // SAFETY: `TIOCGWINSZ` fills a `struct winsize`.
    let ok = unsafe { ioctl(1, TIOCGWINSZ, ws.as_mut_ptr()) } == 0;
    if ok && ws[1] > 0 {
        usize::from(ws[1])
    } else {
        80
    }
}

/// The console window's width in columns (80 when unknown).
#[cfg(windows)]
fn columns() -> usize {
    const STD_OUTPUT: u32 = -11i32 as u32;
    unsafe extern "system" {
        fn GetConsoleScreenBufferInfo(h: Handle, info: *mut i16) -> i32;
    }
    // `CONSOLE_SCREEN_BUFFER_INFO` as `i16`s: size, cursor, attributes,
    // window (left, top, right, bottom), maximum size.
    let mut info = [0i16; 11];
    // SAFETY: a std handle and a buffer the size of the structure.
    let ok = unsafe { GetConsoleScreenBufferInfo(GetStdHandle(STD_OUTPUT), info.as_mut_ptr()) };
    let width = i32::from(info[7]) - i32::from(info[5]) + 1;
    usize::try_from(width)
        .ok()
        .filter(|w| ok != 0 && *w > 0)
        .unwrap_or(80)
}

/// Stop the whole job, as `C-z` does in cooked mode: `rok repl` waits on
/// this process in the same process group.
#[cfg(unix)]
fn suspend() {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    const SIGTSTP: i32 = 20;
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    const SIGTSTP: i32 = 18;
    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    // SAFETY: signals this process group.
    unsafe { kill(0, SIGTSTP) };
}

#[cfg(windows)]
const fn suspend() {}

fn byte(r: &mut impl Read) -> Option<u8> {
    let mut b = [0u8; 1];
    loop {
        match r.read(&mut b) {
            Ok(1) => return Some(b[0]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            _ => return None,
        }
    }
}

/// The rest of an escape sequence after ESC: `[A`, `[3~`, `OH`, ..
fn escape(r: &mut impl Read) -> Option<String> {
    let first = byte(r)?;
    let mut s = String::from(char::from(first));
    if first != b'[' && first != b'O' {
        return Some(s);
    }
    loop {
        let b = byte(r)?;
        s.push(char::from(b));
        if b.is_ascii_alphabetic() || b == b'~' {
            return Some(s);
        }
    }
}

/// A char starting with byte `b` (reading its continuation bytes).
fn utf8(b: u8, r: &mut impl Read) -> Option<char> {
    let len = match b {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => return None,
    };
    let mut bytes = vec![b];
    for _ in 1..len {
        bytes.push(byte(r)?);
    }
    std::str::from_utf8(&bytes).ok()?.chars().next()
}

/// Raw mode on stdin while alive: no line buffering, no echo, no
/// signal keys (`C-c` is read as a byte).
#[cfg(unix)]
struct Raw {
    saved: [u8; TERMIOS_SIZE],
}

/// Room for the platform's `struct termios` (72 bytes on macOS, 60 on
/// Linux).
#[cfg(unix)]
const TERMIOS_SIZE: usize = 256;

#[cfg(target_os = "macos")]
mod term {
    pub const FLAG_BYTES: usize = 8;
    pub const IFLAG_AT: usize = 0;
    pub const LFLAG_AT: usize = 24;
    pub const CC_AT: usize = 32;
    pub const ICRNL: u64 = 0x100;
    pub const ECHO: u64 = 0x8;
    pub const ICANON: u64 = 0x100;
    pub const ISIG: u64 = 0x80;
    pub const IEXTEN: u64 = 0x400;
    pub const VMIN: usize = 16;
    pub const VTIME: usize = 17;
}

#[cfg(all(unix, not(target_os = "macos")))]
mod term {
    pub const FLAG_BYTES: usize = 4;
    pub const IFLAG_AT: usize = 0;
    pub const LFLAG_AT: usize = 12;
    pub const CC_AT: usize = 17;
    pub const ICRNL: u64 = 0o400;
    pub const ECHO: u64 = 0o10;
    pub const ICANON: u64 = 0o2;
    pub const ISIG: u64 = 0o1;
    pub const IEXTEN: u64 = 0o100000;
    pub const VMIN: usize = 6;
    pub const VTIME: usize = 5;
}

#[cfg(unix)]
unsafe extern "C" {
    fn tcgetattr(fd: i32, termios: *mut u8) -> i32;
    fn tcsetattr(fd: i32, action: i32, termios: *const u8) -> i32;
}

/// `TCSANOW` (the same on macOS and Linux): keeps input typed ahead.
#[cfg(unix)]
const TCSANOW: i32 = 0;

#[cfg(unix)]
impl Raw {
    fn enter() -> Option<Self> {
        use std::io::IsTerminal;
        if !std::io::stdin().is_terminal() {
            return None;
        }
        let mut saved = [0u8; TERMIOS_SIZE];
        // SAFETY: the buffer is larger than any `struct termios`.
        if unsafe { tcgetattr(0, saved.as_mut_ptr()) } != 0 {
            return None;
        }
        let mut raw = saved;
        let mut clear = |at: usize, bits: u64| {
            let at = at..at + term::FLAG_BYTES;
            let mut flag = [0u8; 8];
            flag[..term::FLAG_BYTES].copy_from_slice(&raw[at.clone()]);
            let flag = u64::from_ne_bytes(flag) & !bits;
            raw[at].copy_from_slice(&flag.to_ne_bytes()[..term::FLAG_BYTES]);
        };
        let lflags = term::ECHO | term::ICANON | term::ISIG | term::IEXTEN;
        clear(term::LFLAG_AT, lflags);
        // A pasted CR LF stays one line end.
        clear(term::IFLAG_AT, term::ICRNL);
        raw[term::CC_AT + term::VMIN] = 1;
        raw[term::CC_AT + term::VTIME] = 0;
        // SAFETY: `raw` is a `struct termios` read by `tcgetattr`.
        (unsafe { tcsetattr(0, TCSANOW, raw.as_ptr()) } == 0).then_some(Self { saved })
    }
}

#[cfg(unix)]
impl Drop for Raw {
    fn drop(&mut self) {
        // SAFETY: `saved` is what `tcgetattr` filled in.
        unsafe {
            tcsetattr(0, TCSANOW, self.saved.as_ptr());
        }
    }
}

/// Windows: console input without line mode, echo or Ctrl-C processing,
/// keys (and bracketed paste) as VT sequences; output interpreting them.
#[cfg(windows)]
struct Raw {
    saved: [(Handle, u32); 2],
}

#[cfg(windows)]
type Handle = *mut std::ffi::c_void;

#[cfg(windows)]
unsafe extern "system" {
    fn GetStdHandle(which: u32) -> Handle;
    fn GetConsoleMode(h: Handle, mode: *mut u32) -> i32;
    fn SetConsoleMode(h: Handle, mode: u32) -> i32;
}

#[cfg(windows)]
impl Raw {
    fn enter() -> Option<Self> {
        const STD_INPUT: u32 = -10i32 as u32;
        const STD_OUTPUT: u32 = -11i32 as u32;
        const PROCESSED_INPUT: u32 = 0x1;
        const LINE_INPUT: u32 = 0x2;
        const ECHO_INPUT: u32 = 0x4;
        const VT_INPUT: u32 = 0x200;
        const VT_PROCESSING: u32 = 0x4;
        // Defer the wrap at the last column, as VT terminals do (`fresh_line`).
        const NO_AUTO_RETURN: u32 = 0x8;
        let mode = |which| {
            // SAFETY: a std handle and an out parameter.
            let h = unsafe { GetStdHandle(which) };
            let mut m = 0;
            (unsafe { GetConsoleMode(h, &mut m) } != 0).then_some((h, m))
        };
        let (input, output) = (mode(STD_INPUT)?, mode(STD_OUTPUT)?);
        let raw = Self {
            saved: [input, output],
        };
        let set = |(h, m)| {
            // SAFETY: a console handle `GetConsoleMode` accepted.
            unsafe { SetConsoleMode(h, m) != 0 }
        };
        let raw_in = (input.1 & !(PROCESSED_INPUT | LINE_INPUT | ECHO_INPUT)) | VT_INPUT;
        (set((input.0, raw_in)) && set((output.0, output.1 | VT_PROCESSING | NO_AUTO_RETURN)))
            .then_some(raw)
    }
}

#[cfg(windows)]
impl Drop for Raw {
    fn drop(&mut self) {
        for (h, m) in self.saved {
            // SAFETY: restores the mode `GetConsoleMode` read.
            unsafe { SetConsoleMode(h, m) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(s: &str, pos: usize) -> State {
        State {
            buf: s.chars().collect(),
            pos,
            hist: 2,
            draft: Vec::new(),
        }
    }

    fn chars(s: &str) -> Vec<char> {
        s.chars().collect()
    }

    /// Feed `bytes` to a line editor as keys: what it returned (`Eof` when
    /// they run out) and wrote.
    fn feed(bytes: &[u8], history: &[&str]) -> (Input, String) {
        let history: Vec<String> = history.iter().map(|&h| h.to_owned()).collect();
        let mut ed = Line::new(">>> ", &history, Depth::Off);
        let mut out = Vec::new();
        let names = ["println", "print", "List", "len"];
        let complete = &mut |line: &str, at: usize| {
            let start = line[..at].trim_end_matches(is_word).len();
            let word = &line[start..at];
            let found = names.iter().filter(|n| n.starts_with(word));
            (start, found.map(|n| (*n).to_owned()).collect())
        };
        let mut r = bytes;
        while let Some(k) = key(&mut r) {
            if let Step::Done(i) = ed.key(k, &mut out, 40, complete) {
                return (i, String::from_utf8_lossy(&out).into_owned());
            }
        }
        (Input::Eof, String::from_utf8_lossy(&out).into_owned())
    }

    fn line(s: &str) -> Input {
        Input::Line(s.to_owned())
    }

    #[test]
    fn edits_and_history() {
        let mut st = state("let x = foo bar", 15);
        st.kill_word();
        assert_eq!(st.buf.iter().collect::<String>(), "let x = foo ");
        let history = ["a".to_owned(), "b".to_owned()];
        st.recall(&history, -1, false);
        assert_eq!(st.buf.iter().collect::<String>(), "b");
        st.recall(&history, -1, false);
        st.recall(&history, -1, false);
        assert_eq!(st.buf.iter().collect::<String>(), "a");
        st.recall(&history, 1, false);
        st.recall(&history, 1, false);
        assert_eq!(st.buf.iter().collect::<String>(), "let x = foo ");
        assert_eq!(utf8(0xc3, &mut &[0xa9u8][..]), Some('é'));
        assert_eq!(escape(&mut &b"[3~"[..]).as_deref(), Some("[3~"));
    }

    #[test]
    fn up_searches_by_prefix() {
        let history = ["let a = 1", "f(2)", "let b = 2", "let b = 2", "g()"].map(str::to_owned);
        let mut st = state("let", 3);
        st.hist = history.len();
        st.recall(&history, -1, true);
        assert_eq!(st.buf, chars("let b = 2"));
        st.recall(&history, -1, true);
        assert_eq!(st.buf, chars("let a = 1"));
        st.recall(&history, -1, true);
        assert_eq!(st.buf, chars("let a = 1"));
        st.recall(&history, 1, true);
        st.recall(&history, 1, true);
        assert_eq!(st.buf, chars("let"));
        assert_eq!(feed(b"\x1b[A\x1b[A\r", &["x", "y"]).0, line("x"));
        assert_eq!(feed(b"x\x1b[A\r", &["xa", "y", "xb"]).0, line("xb"));
    }

    #[test]
    fn word_moves() {
        let mut st = state("foo.bar(baz)", 12);
        st.pos = st.word_left();
        assert_eq!(st.pos, 8);
        st.pos = st.word_left();
        assert_eq!(st.pos, 4);
        st.pos = st.word_right();
        assert_eq!(st.pos, 7);
        // `M-b`, then type; `C-Left` the same.
        assert_eq!(feed(b"ab cd\x1bbX\r", &[]).0, line("ab Xcd"));
        assert_eq!(feed(b"ab cd\x1b[1;5DX\x1bfY\r", &[]).0, line("ab XcdY"));
        assert_eq!(feed(b"ab cd\x1b\x7f\r", &[]).0, line("ab "));
    }

    #[test]
    fn keys_decode() {
        let keys = |b: &[u8]| {
            let mut r = b;
            std::iter::from_fn(|| key(&mut r)).collect::<Vec<_>>()
        };
        assert_eq!(
            keys(b"a\x01\x1b[A\x1bb\xc3\xa9\x7f"),
            [
                Key::Char('a'),
                Key::Ctrl(1),
                Key::Esc("[A".into()),
                Key::Esc("b".into()),
                Key::Char('é'),
                Key::Ctrl(127),
            ]
        );
        assert_eq!(
            keys(b"\x1b[200~fn f() {\r\n\t1\r\n}\r\n\x1b[201~x"),
            [Key::Paste("fn f() {\n\t1\n}\n".into()), Key::Char('x')]
        );
        assert_eq!(keys(b"\x1b[200~ab"), [Key::Paste("ab".into())]);
    }

    #[test]
    fn a_paste_is_one_cell() {
        let (got, out) = feed(b"\x1b[200~let a = 1\r\nlet b\t= 2\r\n\x1b[201~\r", &[]);
        assert_eq!(got, line("let a = 1\nlet b\t= 2"));
        // Shown on one line while editing, then as the cell's lines.
        assert!(out.contains("let a = 1\u{21b5}let b = 2"), "{out:?}");
        assert!(
            out.ends_with(">>> let a = 1\x1b[K\r\n... let b = 2\x1b[K\r\n"),
            "{out:?}"
        );
        // Tab in a paste is text, not completion.
        assert_eq!(feed(b"\x1b[200~pri\t\x1b[201~\r", &[]).0, line("pri\t"));
    }

    #[test]
    fn tab_completes() {
        assert_eq!(feed(b"pri\t\r", &[]).0, line("print"));
        assert_eq!(feed(b"Li\t(\r", &[]).0, line("List("));
        assert_eq!(feed(b"x = l\t\r", &[]).0, line("x = len"));
        // No common prefix to add: the candidates are listed.
        let (got, out) = feed(b"print\t\r", &[]);
        assert_eq!(got, line("print"));
        assert!(out.contains("\r\nprint    println\r\n"), "{out:?}");
        // At the start of a line Tab indents.
        assert_eq!(feed(b"\tx\r", &[]).0, line("    x"));
    }

    #[test]
    fn reverse_search() {
        let history = ["let a = 1", "f(a)", "let ab = 2"];
        assert_eq!(feed(b"\x12a\r", &history).0, line("let ab = 2"));
        assert_eq!(feed(b"\x12a\x12\r", &history).0, line("f(a)"));
        assert_eq!(feed(b"\x12a =\x12\r", &history).0, line("let a = 1"));
        // `C-g` keeps the line typed; a move key keeps the match to edit.
        assert_eq!(feed(b"z\x12a\x07\r", &history).0, line("z"));
        assert_eq!(feed(b"\x12f(\x1b[FX\r", &history).0, line("f(a)X"));
        let (_, out) = feed(b"\x12qq", &history);
        assert!(out.contains("(failed reverse-i-search)`qq': "), "{out:?}");
        assert_eq!(feed(b"z\x12a\x03", &history).0, Input::Interrupt);
    }

    #[test]
    fn display_width() {
        assert_eq!(width('a'), 1);
        assert_eq!(width('한'), 2);
        assert_eq!(width('ｱ'), 1);
        assert_eq!(width('Ａ'), 2);
        assert_eq!(width('中'), 2);
        assert_eq!(width('\u{301}'), 0);
        assert_eq!(width('\u{200b}'), 0);
        assert_eq!(width('\u{1160}'), 0);
        assert_eq!(width('😀'), 2);
        assert_eq!(width('⌚'), 2);
        assert_eq!(width('→'), 1);
        assert_eq!(width('\u{ad}'), 1);
        assert_eq!(width('\u{3400}'), 2);
        // `"한글"` with a 4-column prompt: the cursor goes to column 10.
        let (_, out) = feed("\"한글\"".as_bytes(), &[]);
        assert!(out.ends_with("\r>>> \"한글\"\x1b[K\r\x1b[10C"), "{out:?}");
    }

    #[test]
    fn long_lines_scroll_to_the_cursor() {
        // A 2-column prompt and 10 columns: 7 columns fit before the cursor's column.
        let w = |s: &str, pos| window(&chars(s), pos, 2, 10);
        assert_eq!(w("abc", 3), 0..3);
        assert_eq!(w("abcdefghijkl", 12), 6..12);
        assert_eq!(w("abcdefghijkl", 0), 0..7);
        assert_eq!(w("abcdefghijkl", 9), 3..10);
        // Wide chars take two of the 7.
        assert_eq!(w("가나다라마", 5), 2..5);
        assert_eq!(w("가나다라마", 0), 0..3);
        assert_eq!(w("ab가나다", 3), 0..4);
    }

    #[test]
    fn colours_and_no_colour() {
        let buf = chars("let s = \"a\"");
        let c16 = Depth::Ansi16;
        assert_eq!(
            render(&buf, 0..buf.len(), c16, None),
            "\x1b[95mlet\x1b[0m s = \x1b[32m\"a\"\x1b[0m"
        );
        assert_eq!(
            render(&buf, 4..buf.len(), c16, None),
            "s = \x1b[32m\"a\"\x1b[0m"
        );
        assert_eq!(
            render(&buf, 0..buf.len(), Depth::Off, None),
            "let s = \"a\""
        );
        assert_eq!(
            render(&buf, 0..buf.len(), Depth::Ansi256, None),
            "\x1b[38;5;218mlet\x1b[0m s = \x1b[38;5;114m\"a\"\x1b[0m"
        );
        let buf = chars("// a\n1: i32");
        assert_eq!(
            render(&buf, 0..buf.len(), c16, Some(CONT)),
            "\x1b[90m// a\x1b[0m\x1b[K\r\n... \x1b[33m1\x1b[0m: \x1b[96mi32\x1b[0m"
        );
    }

    #[test]
    fn history_file_keeps_cells() {
        let dir = std::env::temp_dir().join(format!("rok-line-edit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let f = dir.join("repl_history");
        // The one-line format of older files still loads.
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&f, "1 + 1\nlet s = \"a\\nb\"\n").unwrap();
        let mut ed = Editor::with_file(Some(f.clone()));
        assert_eq!(ed.history, ["1 + 1", "let s = \"a\\nb\""]);
        ed.add("fn f() {\n    1\n}");
        ed.add("fn f() {\n    1\n}");
        ed.add("x \\ y");
        let ed = Editor::with_file(Some(f.clone()));
        assert_eq!(
            ed.history,
            ["1 + 1", "let s = \"a\\nb\"", "fn f() {\n    1\n}", "x \\ y"]
        );
        let text = std::fs::read_to_string(&f).unwrap();
        assert!(text.starts_with(HEADER), "{text}");
        assert_eq!(text.lines().count(), 5, "{text}");
        // Capped: the oldest go when it doubles.
        let mut ed = Editor::with_file(Some(f.clone()));
        for i in 0..KEEP * 2 {
            ed.add(&format!("{i}"));
        }
        let ed = Editor::with_file(Some(f));
        assert!(ed.history.len() <= KEEP * 2, "{}", ed.history.len());
        assert_eq!(ed.history.last().map(String::as_str), Some("1999"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prompt_starts_a_fresh_line() {
        assert_eq!(fresh_line(4), "    \r\x1b[K");
        assert_eq!(fresh_line(0), " \r\x1b[K");
    }
}
