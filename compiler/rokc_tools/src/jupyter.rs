//! `rokc jupyter`: a Jupyter kernel (spec 908) on the REPL engine.
//!
//! Messaging 5.3 over a minimal hand-written ZMTP 3.0 (TCP, NULL
//! mechanism): shell, control and stdin as ROUTER, iopub as PUB, heartbeat
//! as REP, HMAC-SHA256 signed.
//!
//! Not supported: CURVE / ipc, iopub topic filtering, rich display, the
//! debugger, `input_request` (cells read `/dev/null`). `complete` /
//! `inspect` are the language server's; `history` is this kernel's cells.
//! Output is sent when a cell ends.

use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{SystemTime, UNIX_EPOCH};

use rokc_core::hash::{hex, hmac_sha256};
use rokc_interface::BuildOptions;

use crate::analysis::Analysis;
use crate::lsp::complete;
use crate::repl::{Repl, Start, incomplete, split_cells};
use rokc_diagnostic::Json;

const PROTOCOL: &str = "5.3";
const DELIM: &[u8] = b"<IDS|MSG>";
/// Written after a cell to stdout / stderr: the capture ends there.
const END: &[u8] = b"\x00@@rok-kernel-end@@\x00";

/// The Jupyter signature of a message's four JSON frames (hex; empty
/// when there is no key).
pub fn sign(key: &[u8], frames: &[&[u8]]) -> String {
    if key.is_empty() {
        return String::new();
    }
    hex(&hmac_sha256(key, frames))
}

/// Greeting and `READY` both ways, as a socket of `kind` (`ROUTER`,
/// `PUB`, `REP`, or a client's `DEALER` / `SUB` / `REQ`).
pub fn handshake(s: &mut TcpStream, kind: &str) -> io::Result<()> {
    let mut g = [0u8; 64];
    g[0] = 0xff;
    g[9] = 0x7f;
    g[10] = 3;
    g[12..16].copy_from_slice(b"NULL");
    s.write_all(&g)?;
    let mut peer = [0u8; 64];
    s.read_exact(&mut peer)?;
    if peer[0] != 0xff || peer[9] & 1 == 0 || peer[10] < 3 || &peer[12..17] != b"NULL\0" {
        return Err(bad("not a ZMTP 3 NULL peer"));
    }
    let mut ready = b"\x05READY\x0bSocket-Type".to_vec();
    ready.extend_from_slice(&(kind.len() as u32).to_be_bytes());
    ready.extend_from_slice(kind.as_bytes());
    s.write_all(&frame(&ready, false, true))?;
    match read_frame(s)? {
        (flags, body) if flags & 4 != 0 && body.starts_with(b"\x05READY") => Ok(()),
        _ => Err(bad("no READY")),
    }
}

fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

/// One frame on the wire.
pub fn frame(body: &[u8], more: bool, command: bool) -> Vec<u8> {
    let flags = u8::from(more) | (u8::from(command) << 2);
    let mut out = Vec::with_capacity(body.len() + 9);
    if body.len() > 255 {
        out.push(flags | 2);
        out.extend_from_slice(&(body.len() as u64).to_be_bytes());
    } else {
        out.push(flags);
        out.push(body.len() as u8);
    }
    out.extend_from_slice(body);
    out
}

/// One frame: its flags and body.
fn read_frame(s: &mut impl Read) -> io::Result<(u8, Vec<u8>)> {
    let mut flags = [0u8];
    s.read_exact(&mut flags)?;
    let len = if flags[0] & 2 != 0 {
        let mut n = [0u8; 8];
        s.read_exact(&mut n)?;
        u64::from_be_bytes(n)
    } else {
        let mut n = [0u8];
        s.read_exact(&mut n)?;
        u64::from(n[0])
    };
    if len > 1 << 30 {
        return Err(bad("frame too large"));
    }
    let mut body = vec![0; len as usize];
    s.read_exact(&mut body)?;
    Ok((flags[0], body))
}

/// The next message (its frames); commands (`PING`, `SUBSCRIBE`, ..)
/// are skipped.
pub fn read_msg(s: &mut impl Read) -> io::Result<Vec<Vec<u8>>> {
    let mut frames = Vec::new();
    loop {
        let (flags, body) = read_frame(s)?;
        if flags & 4 != 0 {
            continue;
        }
        frames.push(body);
        if flags & 1 == 0 {
            return Ok(frames);
        }
    }
}

/// A message as one write (so writers sharing a stream do not mix).
pub fn write_msg(s: &mut impl Write, frames: &[Vec<u8>]) -> io::Result<()> {
    let mut buf = Vec::new();
    for (i, f) in frames.iter().enumerate() {
        buf.extend(frame(f, i + 1 < frames.len(), false));
    }
    s.write_all(&buf)
}

pub struct Msg {
    /// Routing frames before `<IDS|MSG>`.
    pub ids: Vec<Vec<u8>>,
    pub header: Json,
    pub parent: Json,
    pub content: Json,
}

impl Msg {
    pub fn kind(&self) -> &str {
        self.header.get("msg_type").as_str().unwrap_or("")
    }
}

/// Split and check a message's frames.
pub fn parse_msg(key: &[u8], frames: Vec<Vec<u8>>) -> Option<Msg> {
    let d = frames.iter().position(|f| f == DELIM)?;
    let parts = frames.get(d + 2..d + 6)?;
    let refs: Vec<&[u8]> = parts.iter().map(Vec::as_slice).collect();
    let want = sign(key, &refs);
    let got = &frames[d + 1];
    // Constant time: the key is the kernel's only secret.
    if got.len() != want.len()
        || got
            .iter()
            .zip(want.bytes())
            .fold(0, |a, (x, y)| a | (x ^ y))
            != 0
    {
        return None;
    }
    let json = |b: &[u8]| Json::parse(std::str::from_utf8(b).ok()?).ok();
    Some(Msg {
        ids: frames[..d].to_vec(),
        header: json(&parts[0])?,
        parent: json(&parts[1])?,
        content: json(&parts[3])?,
    })
}

/// The frames of a message of type `kind` in reply to (or caused by)
/// `parent`.
pub fn encode(
    key: &[u8],
    session: &str,
    ids: &[Vec<u8>],
    kind: &str,
    parent: &Json,
    content: &Json,
) -> Vec<Vec<u8>> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let id = format!(
        "{session}-{}",
        SEQ.fetch_add(1, Ordering::Relaxed) // unique within the session
    );
    let header = Json::obj([
        ("msg_id", Json::Str(id)),
        ("session", Json::str(session)),
        ("username", Json::str("rok")),
        (
            "date",
            Json::Str(iso8601(now.as_secs(), now.subsec_micros())),
        ),
        ("msg_type", Json::str(kind)),
        ("version", Json::str(PROTOCOL)),
    ]);
    let parts = [
        header.to_string().into_bytes(),
        parent.to_string().into_bytes(),
        b"{}".to_vec(),
        content.to_string().into_bytes(),
    ];
    let refs: Vec<&[u8]> = parts.iter().map(Vec::as_slice).collect();
    let sig = sign(key, &refs);
    let mut frames = ids.to_vec();
    frames.push(DELIM.to_vec());
    frames.push(sig.into_bytes());
    frames.extend(parts);
    frames
}

/// `YYYY-MM-DDThh:mm:ss.uuuuuuZ` (UTC).
#[allow(clippy::many_single_char_names)]
fn iso8601(secs: u64, micros: u32) -> String {
    // Days to civil date (H. Hinnant's algorithm).
    let z = secs / 86400 + 719_468;
    let (era, doe) = (z / 146_097, z % 146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + u64::from(m <= 2);
    let t = secs % 86400;
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{micros:06}Z",
        t / 3600,
        t / 60 % 60,
        t % 60
    )
}

/// A ROUTER connection's writing end.
type Conn = Arc<Mutex<TcpStream>>;

struct Wire {
    key: Vec<u8>,
    session: String,
    subscribers: Mutex<Vec<TcpStream>>,
}

impl Wire {
    fn reply(&self, conn: &Conn, req: &Msg, kind: &str, content: &Json) {
        let frames = encode(
            &self.key,
            &self.session,
            &req.ids,
            kind,
            &req.header,
            content,
        );
        let _ = write_msg(&mut *conn.lock().unwrap(), &frames);
    }

    /// To every iopub subscriber; one that is gone is dropped.
    fn publish(&self, parent: &Json, kind: &str, content: &Json) {
        let topic = format!("kernel.{}.{kind}", self.session).into_bytes();
        let frames = encode(&self.key, &self.session, &[topic], kind, parent, content);
        let mut subs = self.subscribers.lock().unwrap();
        subs.retain_mut(|s| write_msg(s, &frames).is_ok());
    }

    fn status(&self, parent: &Json, state: &str) {
        self.publish(
            parent,
            "status",
            &Json::obj([("execution_state", Json::str(state))]),
        );
    }
}

/// Accept connections on `l` and run `f` on each, in its own thread.
fn serve_each(l: TcpListener, f: impl Fn(TcpStream) + Send + Sync + 'static) {
    let f = Arc::new(f);
    std::thread::spawn(move || {
        for s in l.incoming().flatten() {
            let f = f.clone();
            std::thread::spawn(move || f(s));
        }
    });
}

/// Send what is written to `fd` from now on to the receiver, one string
/// per `END` mark.
fn capture(fd: i32) -> io::Result<mpsc::Receiver<String>> {
    let mut r = redirect(fd)?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let (mut acc, mut buf) = (Vec::new(), [0u8; 8192]);
        while let Ok(n) = r.read(&mut buf) {
            if n == 0 {
                return;
            }
            acc.extend_from_slice(&buf[..n]);
            while let Some(at) = acc.windows(END.len()).position(|w| w == END) {
                let text = String::from_utf8_lossy(&acc[..at]).into_owned();
                acc.drain(..at + END.len());
                let _ = tx.send(text);
            }
        }
    });
    Ok(rx)
}

/// Point `fd` (1 or 2) at a new pipe, which child processes inherit;
/// its reading end.
#[cfg(unix)]
fn redirect(fd: i32) -> io::Result<std::os::unix::net::UnixStream> {
    use std::os::fd::AsRawFd;
    unsafe extern "C" {
        fn dup2(old: i32, new: i32) -> i32;
    }
    let (r, w) = std::os::unix::net::UnixStream::pair()?;
    // SAFETY: plain fd duplication; `w` stays open until then.
    if unsafe { dup2(w.as_raw_fd(), fd) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(r)
}

/// Windows: the std handle of `fd` (what Rust's own stdout / stderr and
/// spawned children use).
#[cfg(windows)]
fn redirect(fd: i32) -> io::Result<std::fs::File> {
    use std::os::windows::io::FromRawHandle;
    type Handle = *mut std::ffi::c_void;
    unsafe extern "system" {
        fn CreatePipe(r: *mut Handle, w: *mut Handle, security: *mut u8, size: u32) -> i32;
        fn SetStdHandle(which: u32, h: Handle) -> i32;
    }
    let which = if fd == 1 { -11i32 } else { -12i32 } as u32;
    let (mut r, mut w): (Handle, Handle) = (std::ptr::null_mut(), std::ptr::null_mut());
    // SAFETY: out parameters; default security and size.
    if unsafe { CreatePipe(&mut r, &mut w, std::ptr::null_mut(), 0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `r` is ours alone; `w` becomes the std handle for good.
    let r = unsafe { std::fs::File::from_raw_handle(r) };
    if unsafe { SetStdHandle(which, w) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(r)
}

/// `is_complete_reply` status: the REPL's continuation rule.
fn completeness(code: &str) -> &'static str {
    if incomplete(code) {
        "incomplete"
    } else {
        "complete"
    }
}

fn ident(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

/// The byte range of the identifier around the character `pos`.
fn word_at(code: &str, pos: usize) -> (usize, usize) {
    let at = code.char_indices().nth(pos).map_or(code.len(), |(i, _)| i);
    let start = code[..at].trim_end_matches(ident).len();
    let end = code.len() - code[at..].trim_start_matches(ident).len();
    (start, end)
}

struct Kernel {
    wire: Arc<Wire>,
    repl: Repl,
    count: u32,
    /// `(execution count, code)` of the cells stored in the history.
    history: Vec<(u32, String)>,
    out: mpsc::Receiver<String>,
    err: mpsc::Receiver<String>,
    /// The session's check without a cell (path completion), until the
    /// next cell runs.
    checked: Option<(Analysis, String)>,
    /// The output of loading the package, shown with the first cell's.
    startup: (String, String),
}

impl Kernel {
    /// Run `code`: (stdout, stderr, what the last cell shows), `Err` at the
    /// first failing cell (earlier cells keep their effects). As `IPython`,
    /// only the last value is shown.
    fn run(&mut self, code: &str) -> (String, String, Result<Option<String>, ()>) {
        self.checked = None;
        let mut result = Ok(None);
        for cell in split_cells(code) {
            result = if cell.starts_with(':') {
                self.repl.cell(&cell);
                Ok(None)
            } else {
                self.repl.eval(&cell)
            };
            if result.is_err() {
                break;
            }
        }
        let (out, err) = self.output();
        let (o, e) = std::mem::take(&mut self.startup);
        (o + &out, e + &err, result)
    }

    /// What went to stdout and stderr since the last call.
    fn output(&self) -> (String, String) {
        let mut stdout = io::stdout();
        let _ = stdout.write_all(END).and_then(|()| stdout.flush());
        let _ = io::stderr().write_all(END);
        let out = self.out.recv().unwrap_or_default();
        let err = self.err.recv().unwrap_or_default();
        (out, err)
    }

    fn execute(&mut self, conn: &Conn, req: &Msg) {
        let c = &req.content;
        let code = c.get("code").as_str().unwrap_or("");
        let silent = c.get("silent") == &Json::Bool(true);
        if !silent && c.get("store_history") != &Json::Bool(false) {
            self.count += 1;
            self.history.push((self.count, code.to_owned()));
        }
        let n = Json::num(self.count);
        let wire = self.wire.clone();
        let publish = |kind: &str, content: Json| {
            if !silent {
                wire.publish(&req.header, kind, &content);
            }
        };
        publish(
            "execute_input",
            Json::obj([("code", Json::str(code)), ("execution_count", n.clone())]),
        );
        let (out, err, result) = self.run(code);
        let stream = |name: &str, text: String| {
            if !text.is_empty() {
                publish(
                    "stream",
                    Json::obj([("name", Json::str(name)), ("text", Json::Str(text))]),
                );
            }
        };
        stream("stdout", out);
        let reply = if let Ok(shown) = result {
            stream("stderr", err);
            if let Some(v) = shown {
                publish(
                    "execute_result",
                    Json::obj([
                        ("execution_count", n.clone()),
                        ("data", Json::obj([("text/plain", Json::Str(v))])),
                        ("metadata", Json::obj([])),
                    ]),
                );
            }
            Json::obj([
                ("status", Json::str("ok")),
                ("execution_count", n),
                ("user_expressions", Json::obj([])),
                ("payload", Json::Arr(Vec::new())),
            ])
        } else {
            let lines: Vec<&str> = err.lines().filter(|l| !l.trim().is_empty()).collect();
            let evalue = lines.first().map_or("the cell failed", |l| l.trim());
            let error = [
                ("ename", Json::str("Error")),
                ("evalue", Json::str(evalue)),
                (
                    "traceback",
                    Json::Arr(lines.iter().map(|l| Json::str(*l)).collect()),
                ),
            ];
            publish("error", Json::obj(error.clone()));
            let mut reply = vec![
                ("status".to_owned(), Json::str("error")),
                ("execution_count".to_owned(), n),
            ];
            reply.extend(error.map(|(k, v)| (k.to_owned(), v)));
            Json::Obj(reply)
        };
        self.wire.reply(conn, req, "execute_reply", &reply);
    }

    /// Answer one shell / control request; `false` after `shutdown`.
    fn handle(&mut self, conn: &Conn, req: &Msg) -> bool {
        let ok = ("status", Json::str("ok"));
        let c = &req.content;
        self.wire.status(&req.header, "busy");
        let (kind, content) = match req.kind() {
            "kernel_info_request" => (
                "kernel_info_reply",
                Json::obj([
                    ok,
                    ("protocol_version", Json::str(PROTOCOL)),
                    ("implementation", Json::str("rokc")),
                    (
                        "implementation_version",
                        Json::str(env!("CARGO_PKG_VERSION")),
                    ),
                    (
                        "language_info",
                        Json::obj([
                            ("name", Json::str("rok")),
                            ("version", Json::str(env!("CARGO_PKG_VERSION"))),
                            ("mimetype", Json::str("text/x-rok")),
                            ("file_extension", Json::str(".rok")),
                        ]),
                    ),
                    ("banner", Json::str("rok (rokc jupyter)")),
                    ("help_links", Json::Arr(Vec::new())),
                ]),
            ),
            "execute_request" => {
                self.execute(conn, req);
                ("", Json::Null)
            }
            "is_complete_request" => {
                let code = c.get("code").as_str().unwrap_or("");
                let indent = ("indent", Json::str(""));
                (
                    "is_complete_reply",
                    Json::obj([("status", Json::str(completeness(code))), indent]),
                )
            }
            "complete_request" => {
                let code = c.get("code").as_str().unwrap_or("");
                let pos = c.get("cursor_pos").as_u32().unwrap_or(0) as usize;
                let end = code.char_indices().nth(pos).map_or(code.len(), |(i, _)| i);
                let head = code[..end].trim_end_matches(ident);
                let (start, matches) = if head.ends_with('.') || head.ends_with("::") {
                    // Members and paths: the language server's completion
                    // on the session source with this cell in it.
                    let (src, at) = self.repl.source(code);
                    if self.checked.is_none() {
                        self.checked = Some(self.repl.analyze(&self.repl.source("").0));
                    }
                    let (good, key) = self.checked.as_ref().expect("checked above");
                    let compile = |t: &str| self.repl.analyze(t).0;
                    let (start, found) = complete(&src, key, at + end, compile, Some(good));
                    (start - at, found.into_iter().map(|(l, ..)| l).collect())
                } else {
                    // A name: the session's, or a keyword.
                    let word = &code[head.len()..end];
                    let mut names = self.repl.names();
                    names.extend(rokc_lexer::token::RESERVED);
                    let names = names.into_iter().filter(|n| n.starts_with(word));
                    (head.len(), names.map(str::to_owned).collect::<Vec<_>>())
                };
                (
                    "complete_reply",
                    Json::obj([
                        ok,
                        (
                            "matches",
                            Json::Arr(matches.into_iter().map(Json::Str).collect()),
                        ),
                        (
                            "cursor_start",
                            Json::num(code[..start].chars().count() as u32),
                        ),
                        ("cursor_end", Json::num(pos as u32)),
                        ("metadata", Json::obj([])),
                    ]),
                )
            }
            "inspect_request" => {
                let code = c.get("code").as_str().unwrap_or("");
                let pos = c.get("cursor_pos").as_u32().unwrap_or(0) as usize;
                let (start, end) = word_at(code, pos);
                let help = (start < end)
                    .then(|| self.repl.help(code, start, end))
                    .flatten();
                let data = help.as_ref().map_or_else(
                    || Json::obj([]),
                    |t| Json::obj([("text/plain", Json::str(t))]),
                );
                (
                    "inspect_reply",
                    Json::obj([
                        ok,
                        ("found", Json::Bool(help.is_some())),
                        ("data", data),
                        ("metadata", Json::obj([])),
                    ]),
                )
            }
            "history_request" => {
                let n = c.get("n").as_u32().map_or(usize::MAX, |n| n as usize);
                let tail = c.get("hist_access_type").as_str() == Some("tail");
                let skip = if tail {
                    self.history.len().saturating_sub(n)
                } else {
                    0
                };
                let entries = self.history[skip..]
                    .iter()
                    .map(|(i, code)| Json::Arr(vec![Json::num(0), Json::num(*i), Json::str(code)]));
                (
                    "history_reply",
                    Json::obj([ok, ("history", Json::Arr(entries.collect()))]),
                )
            }
            "comm_info_request" => ("comm_info_reply", Json::obj([ok, ("comms", Json::obj([]))])),
            "shutdown_request" => {
                let restart = c.get("restart").clone();
                self.wire.reply(
                    conn,
                    req,
                    "shutdown_reply",
                    &Json::obj([ok, ("restart", restart)]),
                );
                self.wire.status(&req.header, "idle");
                return false;
            }
            _ => ("", Json::Null),
        };
        if !kind.is_empty() {
            self.wire.reply(conn, req, kind, &content);
        }
        self.wire.status(&req.header, "idle");
        true
    }
}

/// Run the kernel for the connection file `file`; the exit code.
pub fn serve(
    file: &Path,
    bootstrap: Vec<PathBuf>,
    externs: Vec<(String, PathBuf)>,
    deps_dir: PathBuf,
    opts: BuildOptions,
    load: Start<'_>,
) -> i32 {
    match start(file, bootstrap, externs, deps_dir, opts, load) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("rokc jupyter: {e}");
            1
        }
    }
}

fn start(
    file: &Path,
    bootstrap: Vec<PathBuf>,
    externs: Vec<(String, PathBuf)>,
    deps_dir: PathBuf,
    opts: BuildOptions,
    load: Start<'_>,
) -> Result<(), String> {
    let text = std::fs::read_to_string(file).map_err(|e| format!("{}: {e}", file.display()))?;
    let cf = Json::parse(&text).map_err(|e| format!("{}: {e}", file.display()))?;
    if cf.get("transport").as_str().is_some_and(|t| t != "tcp") {
        return Err("only the tcp transport is supported".into());
    }
    if cf
        .get("signature_scheme")
        .as_str()
        .is_some_and(|s| !s.is_empty() && s != "hmac-sha256")
    {
        return Err("only hmac-sha256 signatures are supported".into());
    }
    let ip = match cf.get("ip").as_str().unwrap_or("127.0.0.1") {
        "*" => "0.0.0.0",
        ip => ip,
    };
    let bind = |name: &str| {
        let port = cf
            .get(name)
            .as_u32()
            .ok_or_else(|| format!("no `{name}`"))?;
        TcpListener::bind((ip, port as u16)).map_err(|e| format!("{name} {ip}:{port}: {e}"))
    };
    let (shell, control, stdin, iopub, hb) = (
        bind("shell_port")?,
        bind("control_port")?,
        bind("stdin_port")?,
        bind("iopub_port")?,
        bind("hb_port")?,
    );
    let mut repl = Repl::new(bootstrap, opts)?;
    repl.externs = externs;
    repl.deps_dir = deps_dir;
    let running = repl.running.clone();
    let key = cf.get("key").as_str().unwrap_or("").as_bytes().to_vec();
    let wire = Arc::new(Wire {
        key,
        session: format!("rok-{}", std::process::id()),
        subscribers: Mutex::new(Vec::new()),
    });

    serve_each(hb, |mut s| {
        if handshake(&mut s, "REP").is_ok() {
            while let Ok(m) = read_msg(&mut s) {
                if write_msg(&mut s, &m).is_err() {
                    break;
                }
            }
        }
    });
    let w = wire.clone();
    serve_each(iopub, move |mut s| {
        if handshake(&mut s, "PUB").is_ok() {
            if let Ok(c) = s.try_clone() {
                w.subscribers.lock().unwrap().push(c);
            }
            // Subscriptions: every subscriber gets everything.
            while read_msg(&mut s).is_ok() {}
        }
    });
    // No `input_request`: stdin only accepts connections.
    serve_each(stdin, |mut s| {
        if handshake(&mut s, "ROUTER").is_ok() {
            while read_msg(&mut s).is_ok() {}
        }
    });
    let (tx, rx) = mpsc::channel::<(Conn, Msg)>();
    for (l, control) in [(shell, false), (control, true)] {
        let (tx, wire, running) = (tx.clone(), wire.clone(), running.clone());
        serve_each(l, move |mut s| {
            if handshake(&mut s, "ROUTER").is_err() {
                return;
            }
            let Ok(conn) = s.try_clone() else { return };
            let conn = Arc::new(Mutex::new(conn));
            while let Ok(frames) = read_msg(&mut s) {
                let Some(m) = parse_msg(&wire.key, frames) else {
                    continue; // unsigned or malformed: dropped
                };
                // A shutdown also ends a running cell (the kernel answers
                // between cells).
                if control && matches!(m.kind(), "interrupt_request" | "shutdown_request") {
                    interrupt(&running);
                }
                if control && m.kind() == "interrupt_request" {
                    wire.reply(
                        &conn,
                        &m,
                        "interrupt_reply",
                        &Json::obj([("status", Json::str("ok"))]),
                    );
                } else if tx.send((conn.clone(), m)).is_err() {
                    return;
                }
            }
        });
    }

    // Every channel accepts from here on; the last line on the real stderr.
    eprintln!("rokc jupyter: ready");
    let out = capture(1).map_err(|e| format!("stdout: {e}"))?;
    let err = capture(2).map_err(|e| format!("stderr: {e}"))?;
    let mut k = Kernel {
        wire,
        repl,
        count: 0,
        history: Vec::new(),
        out,
        err,
        checked: None,
        startup: (String::new(), String::new()),
    };
    // The package the kernel starts in, as `rok repl .` loads it. After
    // the capture: the session host it starts inherits the streams.
    k.repl.start(load);
    k.startup = k.output();
    k.wire.status(&Json::obj([]), "starting");
    while let Ok((conn, m)) = rx.recv() {
        if !k.handle(&conn, &m) {
            break;
        }
    }
    Ok(())
}

/// End the running cell, if any (`interrupt_request`; `repl::interrupt`).
// A cell still compiling is not interrupted.
fn interrupt(running: &AtomicU32) {
    let pid = running.load(Ordering::Relaxed);
    if pid != 0 {
        crate::repl::interrupt(pid);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsigned_without_a_key() {
        assert_eq!(sign(b"", &[b"x"]), "");
    }

    #[test]
    fn frames_and_messages_round_trip() {
        // Short and long frames, `more` flags, commands skipped.
        let mut wire = frame(b"\x04PING", false, true);
        let long = vec![7u8; 300];
        let mut msg = Vec::new();
        write_msg(&mut msg, &[b"a".to_vec(), long.clone()]).unwrap();
        assert_eq!(&msg[..3], b"\x01\x01a");
        assert_eq!(msg[3], 2);
        wire.extend(msg);
        assert_eq!(
            read_msg(&mut wire.as_slice()).unwrap(),
            [b"a".to_vec(), long]
        );

        let parent = Json::obj([("msg_id", Json::str("p"))]);
        let content = Json::obj([("code", Json::str("1 + 1"))]);
        let frames = encode(b"k", "s", &[b"id".to_vec()], "t", &parent, &content);
        let m = parse_msg(b"k", frames.clone()).unwrap();
        assert_eq!(m.ids, [b"id".to_vec()]);
        assert_eq!(m.kind(), "t");
        assert_eq!(m.content, content);
        assert!(parse_msg(b"other", frames).is_none());
        assert_eq!(iso8601(0, 5), "1970-01-01T00:00:00.000005Z");
        assert_eq!(iso8601(1_709_210_096, 0), "2024-02-29T12:34:56.000000Z");
    }

    #[test]
    fn jupyter_cells_split_like_the_repl() {
        assert_eq!(
            split_cells("// note\nlet x =\n  40i32;\n\nfn f() {\n}\nx + 2"),
            ["let x =\n  40i32;", "fn f() {\n}", "x + 2"]
        );
        assert_eq!(
            split_cells("'('\n'\"'\n/* ( */ 1\n1 + 1 /* c */\n/* a\n b */\nlet x = 1\nx"),
            [
                "'('",
                "'\"'",
                "/* ( */ 1",
                "1 + 1 /* c */",
                "let x = 1",
                "x"
            ]
        );
        assert_eq!(
            split_cells("fn g(x: i32)\n-> i32 {\n  x\n}\ng(1) +\n  2"),
            ["fn g(x: i32)\n-> i32 {\n  x\n}", "g(1) +\n  2"]
        );
    }

    #[test]
    fn is_complete_follows_the_repl() {
        assert_eq!(completeness("'('"), "complete");
        assert_eq!(completeness("1 + 1 /* c */"), "complete");
        assert_eq!(completeness("let x = 1"), "complete");
        assert_eq!(completeness("/* ("), "incomplete");
        assert_eq!(completeness("fn g(x: i32)"), "incomplete");
        assert_eq!(completeness("let x =\n"), "incomplete");
    }
}
