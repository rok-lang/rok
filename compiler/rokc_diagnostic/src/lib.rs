//! Diagnostics shared across compiler stages.

mod cite;
mod json;

pub use json::Json;
pub use rokc_core::suggest::{edit_distance, suggest, suggest_or_synonym};

use std::fmt;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU8, Ordering};

use rokc_core::term::{Depth, Role};

/// Source location as byte offsets plus 1-based line/column. `file` is
/// a `SourceMap` id (0 = unknown).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash, PartialOrd, Ord)]
pub struct Span {
    pub start: u32,
    pub end: u32,
    pub line: u32,
    pub col: u32,
    pub file: u32,
}

impl Span {
    pub const DUMMY: Self = Self {
        start: 0,
        end: 0,
        line: 0,
        col: 0,
        file: 0,
    };

    #[must_use]
    pub const fn new(start: u32, end: u32, line: u32, col: u32) -> Self {
        Self {
            start,
            end,
            line,
            col,
            file: 0,
        }
    }

    #[must_use]
    pub const fn is_dummy(&self) -> bool {
        self.start == 0 && self.end == 0 && self.line == 0 && self.col == 0
    }
}

/// `ROK_TIME_PASSES=1`: each compiler pass's wall time on stderr.
#[must_use]
pub fn time_passes() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| rokc_core::env_flag("ROK_TIME_PASSES"))
}

/// Run `f` as pass `what`, timed under `ROK_TIME_PASSES`.
pub fn time<T>(what: &str, f: impl FnOnce() -> T) -> T {
    if !time_passes() {
        return f();
    }
    let t = std::time::Instant::now();
    let r = f();
    eprintln!("time: {:>9.1}ms  {what}", t.elapsed().as_secs_f64() * 1e3);
    r
}

/// One compile's source files by id (`Span::file` - 1), for locations and
/// carets.
#[derive(Clone, Debug, Default)]
pub struct SourceMap {
    files: Vec<(String, String)>,
    views: Vec<View>,
}

/// A stretch of a file shown as another: a REPL cell as `<repl>:1:1`, a
/// loaded program as its own lines.
#[derive(Clone, Debug)]
struct View {
    id: u32,
    path: String,
    /// First line and column of the stretch in file `id`.
    from: (u32, u32),
    /// Its last line.
    last: u32,
    /// Where `from` shows; later lines keep their columns.
    at: (u32, u32),
}

impl SourceMap {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            files: Vec::new(),
            views: Vec::new(),
        }
    }

    /// Add a source file; its id goes into the `file` of its spans. A
    /// file already added with the same text keeps its id.
    pub fn add(&mut self, path: String, source: String) -> u32 {
        let i = self
            .files
            .iter()
            .position(|(p, s)| *p == path && *s == source)
            .unwrap_or_else(|| {
                self.files.push((path, source));
                self.files.len() - 1
            });
        u32::try_from(i + 1).unwrap_or(0)
    }

    /// How many files there are (the next one's id is this plus 1).
    #[must_use]
    pub fn len(&self) -> usize {
        self.files.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Every file `(path, text)`, by id.
    #[must_use]
    pub fn files(&self) -> &[(String, String)] {
        &self.files
    }

    /// Path and text of file `id` (a span's `file`).
    #[must_use]
    pub fn file(&self, id: u32) -> Option<&(String, String)> {
        self.files.get(usize::try_from(id).ok()?.checked_sub(1)?)
    }

    /// The path of file `id`.
    #[must_use]
    pub fn path(&self, id: u32) -> Option<&str> {
        self.file(id).map(|(p, _)| p.as_str())
    }

    /// The id of the last file added at `path`.
    #[must_use]
    pub fn id_of(&self, path: &str) -> Option<u32> {
        let i = self.files.iter().rposition(|(p, _)| p == path)?;
        u32::try_from(i + 1).ok()
    }

    fn source_by_path(&self, path: &str) -> Option<&str> {
        self.files
            .iter()
            .find(|(p, _)| p == path)
            .map(|(_, s)| s.as_str())
    }

    /// Show lines `from.0..=last` of file `id` as `path`, `from` at `at`.
    /// Spans outside every stretch show the first `path`, unpositioned.
    pub fn view(&mut self, id: u32, path: String, from: (u32, u32), last: u32, at: (u32, u32)) {
        self.views.push(View {
            id,
            path,
            from,
            last,
            at,
        });
    }

    /// `path:line:col` of `span`, as a panic reports it; `<unknown>` for
    /// a span without a source position.
    #[must_use]
    pub fn location(&self, span: Span) -> String {
        let mut of_file = self.views.iter().filter(|v| v.id == span.file).peekable();
        if let Some(first) = of_file.peek() {
            let path = first.path.clone();
            let inside = |v: &&View| (v.from.0..=v.last).contains(&span.line);
            return match of_file.find(inside) {
                Some(v) if span.line == v.from.0 => {
                    let col = (span.col + v.at.1).saturating_sub(v.from.1).max(1);
                    format!("{}:{}:{col}", v.path, v.at.0)
                }
                Some(v) => format!(
                    "{}:{}:{}",
                    v.path,
                    span.line - v.from.0 + v.at.0,
                    span.col.max(1)
                ),
                None => path,
            };
        }
        match self.path(span.file) {
            Some(p) if span.line != 0 => format!("{p}:{}:{}", span.line, span.col.max(1)),
            _ => "<unknown>".to_owned(),
        }
    }

    /// Line and code-point column where `span` ends, counted from its
    /// start as the lexer counts.
    #[must_use]
    pub fn end(&self, span: Span) -> Option<(u32, u32)> {
        if span.line == 0 {
            return None;
        }
        let (_, text) = self.file(span.file)?;
        let s = usize::try_from(span.start).ok()?;
        let e = usize::try_from(span.end).ok()?;
        let bytes = text.get(s..e)?.as_bytes();
        let (mut line, mut col) = (span.line, span.col.max(1));
        let mut prev = s
            .checked_sub(1)
            .and_then(|p| text.as_bytes().get(p).copied());
        for &b in bytes {
            match b {
                b'\n' if prev == Some(b'\r') => {}
                b'\n' | b'\r' => (line, col) = (line + 1, 1),
                b if b & 0xC0 == 0x80 => {}
                _ => col += 1,
            }
            prev = Some(b);
        }
        Some((line, col))
    }

    /// The file path of `diag`: explicit, else its primary span's file.
    fn diag_path(&self, diag: &RokDiagnostic) -> Option<String> {
        diag.file.clone().or_else(|| {
            let span = diag.primary.as_ref()?.span;
            Some(self.path(span.file)?.to_owned())
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Severity {
    /// Compile fails but later passes still collect diagnostics.
    Error,
    /// Compiler cannot proceed (OOM, stack overflow, ICE).
    Fatal,
    /// Build still passes.
    Warning,
    /// Supplementary hint attached to another diagnostic.
    Note,
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Error => f.write_str("error"),
            Self::Fatal => f.write_str("fatal"),
            Self::Warning => f.write_str("warning"),
            Self::Note => f.write_str("note"),
        }
    }
}

/// Coarse error category.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ErrorKind {
    Lexical,
    Parse,
    Resolve,
    Type,
    Visibility,
    Module,
    Pattern,
    /// A value used after another owner took it (stdlib 2.13.9).
    Handoff,
    Meta,
    Lowering,
    Codegen,
    Linkage,
    Unsupported,
    Internal,
    Other,
    /// A `rokc lint` finding (its lint name is in a note).
    Lint,
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Lexical => "lexical",
            Self::Parse => "parse",
            Self::Resolve => "resolve",
            Self::Type => "type",
            Self::Visibility => "visibility",
            Self::Module => "module",
            Self::Pattern => "pattern",
            Self::Handoff => "handoff",
            Self::Meta => "meta",
            Self::Lowering => "lowering",
            Self::Codegen => "codegen",
            Self::Linkage => "linkage",
            Self::Unsupported => "unsupported",
            Self::Internal => "internal",
            Self::Other => "other",
            Self::Lint => "lint",
        };
        f.write_str(s)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Label {
    pub span: Span,
    pub message: String,
}

/// A machine-applicable edit: replace `span` with `text`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Fix {
    pub span: Span,
    pub text: String,
    /// What the edit does, as a code action title.
    pub title: String,
}

/// The fixes of a diagnostic: one word while empty, which is most of
/// the time (keeps `RokDiagnostic` small as a `Result` error type).
#[derive(Clone, Debug, Default, Eq, PartialEq)]
#[allow(clippy::box_collection)]
pub struct Fixes(Option<Box<Vec<Fix>>>);

impl Fixes {
    pub fn push(&mut self, fix: Fix) {
        self.0.get_or_insert_default().push(fix);
    }
}

impl std::ops::Deref for Fixes {
    type Target = [Fix];
    fn deref(&self) -> &[Fix] {
        self.0.as_deref().map_or(&[], Vec::as_slice)
    }
}

/// What a diagnostic is about, for callers that act on it without
/// matching its text.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Tag {
    #[default]
    None,
    /// `{}` on a value without `Display` or `Debug`.
    NoDisplay,
    /// `{}` on a value without `Display` that has `Debug`.
    NoDisplayHasDebug,
    /// `{:?}` on a value without `Debug`.
    NoDebug,
    /// A closure parameter with no type to infer.
    ClosureParamInfer,
    /// A type parameter nothing infers.
    TypeParamInfer,
    /// A use of a `#[deprecated]` item (editors strike it through).
    Deprecated,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RokDiagnostic {
    pub severity: Severity,
    pub kind: ErrorKind,
    pub message: String,
    pub file: Option<String>,
    /// Boxed to keep `RokDiagnostic` small as a `Result` error type.
    pub primary: Option<Box<Label>>,
    pub secondary: Vec<Label>,
    pub notes: Vec<String>,
    pub tag: Tag,
    pub fixes: Fixes,
    /// The lint this warning belongs to: `rokc lint` levels it like the
    /// lint's own findings (`-A name`, `#[allow(name)]`). Boxed as `primary`.
    pub lint: Option<Box<String>>,
}

impl RokDiagnostic {
    /// An error at `here` (`Span::DUMMY`: no location).
    #[must_use]
    pub fn error(kind: ErrorKind, here: Span, message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Error,
            kind,
            message: message.into(),
            file: None,
            primary: (!here.is_dummy()).then(|| {
                Box::new(Label {
                    span: here,
                    message: String::new(),
                })
            }),
            secondary: Vec::new(),
            notes: Vec::new(),
            tag: Tag::None,
            fixes: Fixes::default(),
            lint: None,
        }
    }

    #[must_use]
    pub fn fatal(kind: ErrorKind, here: Span, message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Fatal,
            ..Self::error(kind, here, message)
        }
    }

    #[must_use]
    pub fn warning(kind: ErrorKind, here: Span, message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Warning,
            ..Self::error(kind, here, message)
        }
    }

    #[must_use]
    pub fn with_file(mut self, file: impl Into<String>) -> Self {
        self.file = Some(file.into());
        self
    }

    #[must_use]
    pub fn with_span(mut self, span: Span, message: impl Into<String>) -> Self {
        self.primary = Some(Box::new(Label {
            span,
            message: message.into(),
        }));
        self
    }

    #[must_use]
    pub fn with_secondary(mut self, span: Span, message: impl Into<String>) -> Self {
        self.secondary.push(Label {
            span,
            message: message.into(),
        });
        self
    }

    #[must_use]
    pub fn with_lint(mut self, name: impl Into<String>) -> Self {
        self.lint = Some(Box::new(name.into()));
        self
    }

    #[must_use]
    pub fn with_note(mut self, note: impl Into<String>) -> Self {
        self.notes.push(note.into());
        self
    }

    #[must_use]
    pub fn with_fix(
        mut self,
        span: Span,
        text: impl Into<String>,
        title: impl Into<String>,
    ) -> Self {
        self.fixes.push(Fix {
            span,
            text: text.into(),
            title: title.into(),
        });
        self
    }

    #[must_use]
    pub const fn with_tag(mut self, tag: Tag) -> Self {
        self.tag = tag;
        self
    }

    #[must_use]
    pub const fn is_error(&self) -> bool {
        matches!(self.severity, Severity::Error | Severity::Fatal)
    }
}

impl RokDiagnostic {
    /// The diagnostic as people read it: no spec citations in its
    /// message, labels or notes.
    #[must_use]
    pub fn without_cites(&self) -> Self {
        let mut d = self.clone();
        d.message = cite::strip(&d.message);
        for n in &mut d.notes {
            *n = cite::strip(n);
        }
        for l in d
            .primary
            .iter_mut()
            .map(|l| &mut **l)
            .chain(&mut d.secondary)
        {
            l.message = cite::strip(&l.message);
        }
        d
    }

    /// The spec sections the message and notes cite (`spec 3.12.5`).
    #[must_use]
    pub fn cites(&self) -> Vec<String> {
        std::iter::once(&self.message)
            .chain(&self.notes)
            .flat_map(|t| cite::refs(t))
            .collect()
    }

    /// A `help:` note, printed as `= help: ..`.
    #[must_use]
    pub fn with_help(self, help: impl Into<String>) -> Self {
        self.with_note(format!("help: {}", help.into()))
    }
}

/// `leaf` defined at both `a` and `b`; no overloading, so the later one
/// is the error. `callable`: suggest default parameters (spec 4.2.3).
#[must_use]
pub fn duplicate(
    kind: ErrorKind,
    message: impl Into<String>,
    leaf: &str,
    a: Span,
    b: Option<Span>,
    callable: bool,
) -> RokDiagnostic {
    let (here, first) = match b.filter(|b| !b.is_dummy() && *b != a) {
        Some(b) if (b.file, b.start) > (a.file, a.start) => (b, Some(a)),
        b => (a, b),
    };
    let leaf = leaf.rsplit("::").next().unwrap_or(leaf);
    let mut d = RokDiagnostic::error(kind, here, message);
    if !here.is_dummy() {
        d = d.with_span(here, "defined again here");
    }
    if let Some(first) = first {
        d = d.with_secondary(first, "first defined here");
    }
    let rename = format!("rename one of them, e.g. `{leaf}2`");
    d.with_help(if callable {
        format!(
            "{rename}; there is no overloading, so to take fewer arguments keep one and give \
             its last parameters defaults, as in `b: i32 = 0` (spec 4.2.3)"
        )
    } else {
        rename
    })
}

impl SourceMap {
    /// `diag` on one line: `error[E200]: path:line:col: message`.
    #[must_use]
    pub fn line(&self, diag: &RokDiagnostic) -> String {
        let diag = &diag.without_cites();
        let mut out = format!("{}[{}]: ", diag.severity, diag.kind.code());
        if let Some(file) = self.diag_path(diag) {
            match diag.primary.as_ref().map(|l| l.span) {
                Some(s) if s.line != 0 => {
                    out.push_str(&format!("{file}:{}:{}: ", s.line, s.col.max(1)));
                }
                _ => out.push_str(&format!("{file}: ")),
            }
        }
        out.push_str(&diag.message);
        out
    }
}

/// One underlined source line of a snippet.
struct Mark<'a> {
    line: u32,
    text: &'a str,
    lead: String,
    width: usize,
    primary: bool,
    label: &'a str,
}

/// Underline of `span` within its source line `text`: (leading blanks,
/// width). Tabs are kept so the marks line up under them.
fn underline(text: &str, span: Span, source: &str) -> (String, usize) {
    let col = usize::try_from(span.col).unwrap_or(1).saturating_sub(1);
    let lead: String = text
        .chars()
        .take(col)
        .map(|c| if c == '\t' { '\t' } else { ' ' })
        .collect();
    let avail = text.chars().count().saturating_sub(col).max(1);
    let width = usize::try_from(span.start)
        .ok()
        .zip(usize::try_from(span.end).ok())
        .and_then(|(s, e)| source.get(s..e))
        .map_or(1, |s| s.split('\n').next().unwrap_or("").chars().count())
        .clamp(1, avail);
    (lead, width)
}

/// How `report` prints (spec 901.8 / 909.4); set once by the CLI. JSON,
/// else the colour depth of the text.
static STYLE: AtomicU8 = AtomicU8::new(0);
const STYLE_JSON: u8 = 2;
/// Colour: this bit, and the depth code below it.
const STYLE_COLOR: u8 = 0x10;

/// `json`: one JSON object per line on stdout (`--message-format=json`);
/// otherwise human text on stderr, coloured when `color`.
pub fn set_report_style(json: bool, color: bool) {
    let depth = if color {
        rokc_core::term::forced_depth()
    } else {
        Depth::Off
    };
    set_report_depth(json, depth);
}

/// [`set_report_style`] with text in colour depth `depth`.
pub fn set_report_depth(json: bool, depth: Depth) {
    let style = match depth {
        _ if json => STYLE_JSON,
        Depth::Off => 0,
        d => STYLE_COLOR | d.code(),
    };
    STYLE.store(style, Ordering::Relaxed);
}

impl ErrorKind {
    /// Spec 909.4 code ranges: E0xx lexical / syntax, E1xx names /
    /// visibility, E2xx types, E3xx meta, E4xx build / link, E5xx other.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Lexical => "E001",
            Self::Parse => "E002",
            Self::Resolve => "E100",
            Self::Visibility => "E101",
            Self::Module => "E102",
            Self::Type => "E200",
            Self::Pattern => "E201",
            Self::Handoff => "E202",
            Self::Meta => "E300",
            Self::Lowering => "E400",
            Self::Codegen => "E401",
            Self::Linkage => "E402",
            Self::Unsupported => "E500",
            Self::Internal => "E501",
            Self::Other => "E502",
            Self::Lint => "E503",
        }
    }
}

/// The `schema` of JSON output: diagnostics and the test, bench and deps
/// records. A schema only gains fields; removing or changing one bumps it.
pub const JSON_SCHEMA: u32 = 1;

impl SourceMap {
    /// `diag` as one-line JSON (spec 909.4):
    /// `{"type","code","kind","message","spans":[..],"notes":[..],"fixes":[..],"spec":[..],
    /// "help":[..],"lint","schema"}`.
    #[must_use]
    pub fn json(&self, diag: &RokDiagnostic) -> String {
        let end = |span: Span| match self.end(span) {
            Some((l, c)) => (Json::num(l), Json::num(c)),
            None => (Json::Null, Json::Null),
        };
        let span_json = |l: &Label, primary: bool, file: Option<String>| {
            let file = file.or_else(|| Some(self.path(l.span.file)?.to_owned()));
            let (line_end, col_end) = end(l.span);
            Json::obj([
                ("file_name", file.map_or(Json::Null, Json::Str)),
                ("line_start", Json::num(l.span.line)),
                ("col_start", Json::num(l.span.col)),
                ("byte_start", Json::num(l.span.start)),
                ("byte_end", Json::num(l.span.end)),
                ("is_primary", Json::Bool(primary)),
                ("label", Json::str(&l.message)),
                ("line_end", line_end),
                ("col_end", col_end),
            ])
        };
        let mut spans = Vec::new();
        if let Some(p) = &diag.primary {
            spans.push(span_json(p, true, self.diag_path(diag)));
        } else if let Some(file) = self.diag_path(diag) {
            spans.push(Json::obj([
                ("file_name", Json::Str(file)),
                ("is_primary", Json::Bool(true)),
            ]));
        }
        spans.extend(diag.secondary.iter().map(|l| span_json(l, false, None)));
        let fixes = diag.fixes.iter().map(|f| {
            let file = self.path(f.span.file).map(str::to_owned);
            let file = file.or_else(|| self.diag_path(diag));
            let (line_end, col_end) = end(f.span);
            Json::obj([
                ("file_name", file.map_or(Json::Null, Json::Str)),
                ("line_start", Json::num(f.span.line)),
                ("col_start", Json::num(f.span.col)),
                ("byte_start", Json::num(f.span.start)),
                ("byte_end", Json::num(f.span.end)),
                ("text", Json::str(&f.text)),
                ("title", Json::str(&f.title)),
                ("line_end", line_end),
                ("col_end", col_end),
            ])
        });
        // Help stays a `help: ` note as well, for readers of `notes`.
        let help = diag
            .notes
            .iter()
            .filter_map(|n| n.strip_prefix("help: "))
            .map(Json::str);
        Json::obj([
            ("type", Json::str(diag.severity.to_string())),
            ("code", Json::str(diag.kind.code())),
            ("kind", Json::str(diag.kind.to_string())),
            ("message", Json::str(&diag.message)),
            ("spans", Json::Arr(spans)),
            (
                "notes",
                Json::Arr(diag.notes.iter().map(Json::str).collect()),
            ),
            ("fixes", Json::Arr(fixes.collect())),
            (
                "spec",
                Json::Arr(diag.cites().iter().map(Json::str).collect()),
            ),
            ("help", Json::Arr(help.collect())),
            ("lint", diag.lint.as_deref().map_or(Json::Null, Json::str)),
            ("schema", Json::num(JSON_SCHEMA)),
        ])
        .to_string()
    }
}

/// The report style in effect (part of a build key whose warnings are
/// replayed as rendered).
#[must_use]
pub fn report_style() -> u8 {
    STYLE.load(Ordering::Relaxed)
}

/// Warnings `report`ed inside `capture_warnings`, rendered.
static CAPTURED: Mutex<Option<String>> = Mutex::new(None);

impl SourceMap {
    /// Print one diagnostic: human text on stderr, or a JSON line on
    /// stdout (see `set_report_style`).
    pub fn report(&self, diag: &RokDiagnostic) {
        let text = self.render(diag);
        if !diag.is_error() {
            if let Some(c) = CAPTURED
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_mut()
            {
                c.push_str(&text);
            }
        }
        print_rendered(&text);
    }
}

/// Run `f`; also what it reported as warnings (on any thread), rendered,
/// for a build record to replay.
pub fn capture_warnings<T>(f: impl FnOnce() -> T) -> (T, String) {
    *CAPTURED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(String::new());
    let r = f();
    let text = CAPTURED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
        .unwrap_or_default();
    (r, text)
}

/// From a panic hook: if `output_closed`, die of SIGPIPE quietly like a C
/// program (Windows: exit 1). SIGPIPE stays ignored until here so other
/// pipes and sockets still see `EPIPE`.
pub fn end_if_output_closed(info: &std::panic::PanicHookInfo<'_>) {
    if !output_closed(info) {
        return;
    }
    rokc_core::tmp::remove_live();
    #[cfg(unix)]
    {
        const SIGPIPE: i32 = 13;
        unsafe extern "C" {
            fn signal(sig: i32, handler: usize) -> usize;
            fn raise(sig: i32) -> i32;
        }
        // SAFETY: SIG_DFL (0) for SIGPIPE, then raise it on this thread.
        unsafe {
            signal(SIGPIPE, 0);
            raise(SIGPIPE);
        }
    }
    std::process::exit(1);
}

/// Whether a panic is std's `print!` / `eprint!` failing on a stdout
/// or stderr whose reader is gone (`rokc .. | head`): no compiler error.
#[must_use]
pub fn output_closed(info: &std::panic::PanicHookInfo<'_>) -> bool {
    let p = info.payload();
    let msg = p
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| p.downcast_ref::<&str>().copied())
        .unwrap_or("");
    // "failed printing to stdout: Broken pipe (os error 32)"
    msg.strip_prefix("failed printing to std")
        .and_then(|m| {
            m.rsplit_once("(os error ")?
                .1
                .strip_suffix(')')?
                .parse()
                .ok()
        })
        .is_some_and(|code| {
            std::io::Error::from_raw_os_error(code).kind() == std::io::ErrorKind::BrokenPipe
        })
}

/// Print text from `render` where `report` would.
pub fn print_rendered(text: &str) {
    if report_style() == STYLE_JSON {
        print!("{text}");
    } else {
        eprint!("{text}");
    }
}

impl SourceMap {
    /// `diag` as `report` prints it, newline-terminated (spec 9.1.8):
    ///
    /// ```text
    /// error[E100]: cannot find value `count` in this scope
    ///   --> src/main.rok:5:10
    ///    |
    ///  5 |     let x = count + 1;
    ///    |             ^^^^^ not found in this scope
    ///    |
    ///    = help: a local with a similar name exists: `counter`
    /// ```
    #[must_use]
    pub fn render(&self, diag: &RokDiagnostic) -> String {
        let style = report_style();
        if style == STYLE_JSON {
            return format!("{}\n", self.json(diag));
        }
        let diag = &diag.without_cites();
        let depth = Depth::from_code(style & !STYLE_COLOR);
        let paint = |r: Role, text: &str| rokc_core::term::paint_in(depth, r, text);
        let color = match diag.severity {
            Severity::Error | Severity::Fatal => Role::Error,
            Severity::Warning => Role::Warning,
            Severity::Note => Role::Note,
        };
        let mut out = format!(
            "{}{}\n",
            paint(color, &format!("{}[{}]", diag.severity, diag.kind.code())),
            paint(Role::Emph, &format!(": {}", diag.message))
        );
        let path = self.diag_path(diag);
        let primary = diag.primary.as_deref();
        let source = primary.and_then(|l| {
            self.file(l.span.file)
                .map(|(_, s)| s.as_str())
                .or_else(|| self.source_by_path(path.as_deref()?))
        });
        // Labels in the primary's file go under its snippet; others are
        // listed by location.
        let mut marks: Vec<Mark<'_>> = Vec::new();
        let mut elsewhere: Vec<&Label> = Vec::new();
        let mut place = |l: &'_ Label, is_primary: bool| -> bool {
            let (Some(p), Some(src)) = (primary, source) else {
                return false;
            };
            if !is_primary && l.span.file != p.span.file && l.span.file != 0 {
                return false;
            }
            let idx = usize::try_from(l.span.line)
                .ok()
                .and_then(|n| n.checked_sub(1));
            // The line after the last one: end of file.
            let eof = |i: usize| (i == src.lines().count()).then_some("");
            let Some(text) = idx.and_then(|i| src.lines().nth(i).or_else(|| eof(i))) else {
                return false;
            };
            let (lead, width) = underline(text, l.span, src);
            marks.push(Mark {
                line: l.span.line,
                text,
                lead,
                width,
                primary: is_primary,
                label: "",
            });
            true
        };
        let mut labels: Vec<(usize, &str)> = Vec::new();
        if let Some(p) = primary {
            if place(p, true) {
                labels.push((0, &p.message));
            }
        }
        for l in &diag.secondary {
            if place(l, false) {
                labels.push((labels.len(), &l.message));
            } else {
                elsewhere.push(l);
            }
        }
        for (i, label) in labels {
            marks[i].label = label;
        }
        marks.sort_by_key(|m| (m.line, m.lead.len()));
        let width = marks
            .iter()
            .map(|m| m.line)
            .max()
            .map_or(1, |n| n.to_string().len());
        let pad = " ".repeat(width + 1);
        let bar = paint(Role::Gutter, "|");
        if let Some(file) = &path {
            let loc = match primary.map(|l| l.span) {
                Some(s) if s.line != 0 => format!("{file}:{}:{}", s.line, s.col.max(1)),
                _ => file.clone(),
            };
            out.push_str(&format!("{pad}{} {loc}\n", paint(Role::Gutter, "-->")));
        }
        if !marks.is_empty() {
            out.push_str(&format!("{pad} {bar}\n"));
            let mut last: Option<u32> = None;
            for m in &marks {
                if last != Some(m.line) {
                    if last.is_some_and(|l| m.line > l + 1) {
                        out.push_str(&format!(" {}\n", paint(Role::Gutter, "...")));
                    }
                    let n = format!("{:>width$}", m.line);
                    out.push_str(&format!(" {} {bar} {}\n", paint(Role::Gutter, &n), m.text));
                }
                last = Some(m.line);
                let (ch, c) = if m.primary {
                    ('^', color)
                } else {
                    ('-', Role::Gutter)
                };
                let mut under: String = std::iter::repeat_n(ch, m.width).collect();
                if !m.label.is_empty() {
                    under = format!("{under} {}", m.label);
                }
                out.push_str(&format!("{pad} {bar} {}{}\n", m.lead, paint(c, &under)));
            }
        }
        for l in elsewhere {
            // A label in another file (a meta block's template) names it.
            let loc = match self.path(l.span.file) {
                Some(p) => format!("{p}:{}:{}", l.span.line, l.span.col.max(1)),
                None => format!("{}:{}", l.span.line, l.span.col.max(1)),
            };
            out.push_str(&format!(
                "{pad}{} {loc}: {}\n",
                paint(Role::Gutter, ":::"),
                l.message
            ));
        }
        if !diag.notes.is_empty() && !marks.is_empty() {
            out.push_str(&format!("{pad} {bar}\n"));
        }
        for note in &diag.notes {
            let (kind, role, text) = match note.strip_prefix("help: ") {
                Some(h) => ("help", Role::Help, h),
                None => ("note", Role::Note, note.as_str()),
            };
            out.push_str(&format!(
                "{pad} {} {}: {text}\n",
                paint(Role::Gutter, "="),
                paint(role, kind)
            ));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_line_escapes_and_codes() {
        let d = RokDiagnostic::error(ErrorKind::Type, Span::DUMMY, "bad \"x\"\n")
            .with_file("a.rok")
            .with_note("n");
        assert_eq!(
            SourceMap::new().json(&d),
            r#"{"type":"error","code":"E200","kind":"type","message":"bad \"x\"\n","spans":[{"file_name":"a.rok","is_primary":true}],"notes":["n"],"fixes":[],"spec":[],"help":[],"lint":null,"schema":1}"#
        );
    }

    #[test]
    fn json_has_schema_lint_end_help() {
        let mut map = SourceMap::new();
        let file = map.add("a.rok".into(), "let é = 1\r\nlet y = x\n".into());
        // `é = 1\r\nlet` spans the CRLF; columns count code points.
        let at = Span {
            start: 4,
            end: 15,
            line: 1,
            col: 5,
            file,
        };
        let d = RokDiagnostic::warning(ErrorKind::Other, at, "m")
            .with_lint("unused_variables")
            .with_help("write `_` instead");
        let json = Json::parse(&map.json(&d)).unwrap();
        assert_eq!(json.get("schema").as_u32(), Some(1));
        assert_eq!(json.get("lint").as_str(), Some("unused_variables"));
        assert_eq!(
            json.get("help"),
            &Json::Arr(vec![Json::str("write `_` instead")])
        );
        // The note stays until a new schema.
        assert_eq!(
            json.get("notes"),
            &Json::Arr(vec![Json::str("help: write `_` instead")])
        );
        let Json::Arr(spans) = json.get("spans") else {
            panic!("{json}")
        };
        assert_eq!(spans[0].get("line_end").as_u32(), Some(2));
        assert_eq!(spans[0].get("col_end").as_u32(), Some(4));
        // A one-line span; no source: no end.
        let one = Span { end: 6, ..at };
        let json =
            Json::parse(&map.json(&RokDiagnostic::error(ErrorKind::Type, one, "m"))).unwrap();
        let Json::Arr(spans) = json.get("spans") else {
            panic!("{json}")
        };
        assert_eq!(spans[0].get("line_end").as_u32(), Some(1));
        assert_eq!(spans[0].get("col_end").as_u32(), Some(6));
        assert_eq!(json.get("lint"), &Json::Null);
        let json = SourceMap::new().json(&RokDiagnostic::error(ErrorKind::Type, at, "m"));
        assert!(json.contains(r#""line_end":null,"col_end":null"#), "{json}");
    }

    #[test]
    fn json_lists_fixes() {
        let mut map = SourceMap::new();
        let file = map.add("a.rok".into(), "let x = 1\n".into());
        let at = Span {
            start: 9,
            end: 9,
            line: 1,
            col: 10,
            file,
        };
        let d = RokDiagnostic::error(ErrorKind::Parse, at, "expected `;`").with_fix(
            at,
            ";",
            "insert `;`",
        );
        let json = map.json(&d);
        assert!(
            json.ends_with(
                r#""fixes":[{"file_name":"a.rok","line_start":1,"col_start":10,"byte_start":9,"byte_end":9,"text":";","title":"insert `;`","line_end":1,"col_end":10}],"spec":[],"help":[],"lint":null,"schema":1}"#
            ),
            "{json}"
        );
    }

    #[test]
    fn tag_and_fixes_round_trip() {
        use rokc_core::codec::{Codec, Dec, Enc};
        let at = Span {
            start: 3,
            end: 5,
            line: 2,
            col: 4,
            file: 0,
        };
        for tag in [
            Tag::None,
            Tag::NoDisplay,
            Tag::NoDisplayHasDebug,
            Tag::NoDebug,
            Tag::ClosureParamInfer,
            Tag::TypeParamInfer,
            Tag::Deprecated,
        ] {
            let plain = RokDiagnostic::error(ErrorKind::Type, at, "m").with_tag(tag);
            let fixed = plain
                .clone()
                .with_fix(at, "x", "t")
                .with_fix(Span::DUMMY, "", "");
            for d in [plain, fixed] {
                let mut e = Enc::new(true);
                d.enc(&mut e);
                let mut dec = Dec::new(&e.buf);
                assert_eq!(RokDiagnostic::dec(&mut dec), Some(d));
                assert!(dec.done());
            }
        }
    }
}

impl rokc_core::codec::Codec for Fixes {
    fn enc(&self, e: &mut rokc_core::codec::Enc) {
        // As a `Vec<Fix>`.
        e.u(self.len() as u64);
        for f in self.iter() {
            f.enc(e);
        }
    }
    fn dec(d: &mut rokc_core::codec::Dec<'_>) -> Option<Self> {
        let v = Vec::<Fix>::dec(d)?;
        Some(Self((!v.is_empty()).then(|| Box::new(v))))
    }
}

impl rokc_core::codec::Codec for Span {
    fn enc(&self, e: &mut rokc_core::codec::Enc) {
        if e.spans {
            if self.file != 0 && !e.files.contains(&self.file) {
                e.files.push(self.file);
            }
            if let Some((f, a, b)) = e.home {
                let inside = self.file == f && self.start >= a && self.end <= b;
                if self.file != 0 && !inside {
                    match e.foreign.iter_mut().find(|(f, _)| *f == self.file) {
                        Some((_, end)) => *end = (*end).max(self.end),
                        None => e.foreign.push((self.file, self.end)),
                    }
                }
            }
            e.u(u64::from(self.file));
            e.u(u64::from(self.start));
            e.u(u64::from(self.end.wrapping_sub(self.start)));
            e.u(u64::from(self.line));
            e.u(u64::from(self.col));
        }
    }
    fn dec(d: &mut rokc_core::codec::Dec<'_>) -> Option<Self> {
        let old = u32::try_from(d.u()?).ok()?;
        let file = d.file(old)?;
        let start = u32::try_from(d.u()?).ok()?;
        let len = u32::try_from(d.u()?).ok()?;
        Some(Self {
            start,
            end: start.wrapping_add(len),
            line: u32::try_from(d.u()?).ok()?,
            col: u32::try_from(d.u()?).ok()?,
            file,
        })
    }
}

rokc_core::codec_enum!(Severity {
    Error,
    Fatal,
    Warning,
    Note
});
rokc_core::codec_enum!(ErrorKind {
    Lexical,
    Parse,
    Resolve,
    Type,
    Visibility,
    Module,
    Pattern,
    Meta,
    Lowering,
    Codegen,
    Linkage,
    Unsupported,
    Internal,
    Other,
    Lint,
    Handoff,
});
rokc_core::codec_struct!(Label { span, message });
rokc_core::codec_struct!(Fix { span, text, title });
rokc_core::codec_enum!(Tag {
    None,
    NoDisplay,
    NoDisplayHasDebug,
    NoDebug,
    ClosureParamInfer,
    TypeParamInfer,
    Deprecated,
});
rokc_core::codec_struct!(RokDiagnostic {
    severity,
    kind,
    message,
    file,
    primary,
    secondary,
    notes,
    tag,
    fixes,
    lint,
});
