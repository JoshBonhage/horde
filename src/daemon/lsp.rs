//! Language servers, supervised.
//!
//! horde spawns programs; it does not embed intelligence. A language server is a child
//! process that reads JSON-RPC on stdin and writes it on stdout, and everything horde knows
//! about a language comes from one of them being installed and configured. Nothing starts
//! unless you asked for it in `config.toml`.
//!
//! **The lifecycle is the design.** A rust-analyzer is gigabytes of resident memory that
//! outlives every client, in a daemon you cannot see when you are detached — so lazy start,
//! restart backoff, idle shutdown and visibility are all in the first cut rather than in a
//! later one. "Lifecycle later" is how a multiplexer becomes the thing people `kill -9`.
//!
//! Hand-rolled rather than built on a protocol crate: horde speaks six messages, phase 0
//! proved the framing is forty lines against two unrelated servers, and a spec crate would be
//! a version to track for shapes that have not changed since 2016. What phase 0 actually
//! found were the failure paths, and each one is a requirement here rather than a surprise:
//!
//! - A server can answer a request with `{id, error}` and **no `result`**. Waiting only for
//!   `result` hangs forever.
//! - A server can send **notifications before the initialize response**, so reading has to be
//!   a dispatch loop rather than request-response ping-pong.
//! - A server can **die at startup** — a wrong command, a rustup proxy whose component is not
//!   installed — with the reason on stderr and nothing at all on stdout. So stderr is kept,
//!   and the death is what surfaces rather than a wait that never ends.
//!
//! One more that the servers themselves insist on: a server may send *us* requests
//! (`client/registerCapability`, `workspace/configuration`). An unanswered request stalls
//! rust-analyzer, so every one gets a reply even when the answer is nothing.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

/// How long a server may go untouched before it is shut down.
///
/// Measured from the last document that passed through rather than from a count of open
/// files, deliberately: a client that closes without saying so — or a horde that forgets to
/// pass the message on — must not be able to strand a gigabyte of language server. Long
/// enough to survive closing one file and opening another, short enough that a project you
/// wandered away from does not hold memory all afternoon.
const IDLE: Duration = Duration::from_secs(300);

/// How long to wait before restarting a server that died, doubling each time.
const BACKOFF: Duration = Duration::from_secs(2);

/// Restarts before horde stops trying and says so.
///
/// A server that has died five times is misconfigured, not unlucky, and restarting it forever
/// is how a daemon comes to spend its life spawning a program that exits.
const MAX_RESTARTS: u32 = 5;

/// Lines of stderr kept, so a death has a reason attached to it.
const STDERR_KEEP: usize = 20;

/// A language server, as identified by the project it serves and the language it speaks.
pub type Key = (PathBuf, String);

pub use crate::proto::{Diag, Severity};

/// LSP numbers its severities, error-first, and a server may leave the number out.
fn severity_from_lsp(n: u64) -> Severity {
    match n {
        1 => Severity::Error,
        2 => Severity::Warning,
        3 => Severity::Info,
        _ => Severity::Hint,
    }
}

/// What a supervised server is doing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// Spawned, `initialize` sent, no answer yet.
    Starting,
    /// Answered `initialize` and told `initialized`. Ready for documents.
    Ready,
    /// Waiting out a backoff before trying again, with the reason it needs to.
    Waiting(String),
    /// Given up on, with the reason. Nothing restarts from here without a config change.
    Failed(String),
}

/// Something a server did that the engine needs to know about.
#[derive(Debug, Clone)]
pub enum Event {
    /// The server answered `initialize`.
    Ready(Key),
    /// A fresh set of diagnostics for one file. Replaces whatever was there — LSP publishes
    /// the whole list for a file every time, including an empty one to mean "all clear".
    Diagnostics { key: Key, path: PathBuf, diags: Vec<Diag> },
    /// The child is gone, with whatever it said on the way out.
    Exited { key: Key, why: String },
}

// -- framing -----------------------------------------------------------------

/// Reassembles LSP messages from a byte stream.
///
/// Headers to a blank line, `Content-Length`, then exactly that many bytes. Both servers
/// phase 0 tried sent only `Content-Length` and always CRLF — but bare LF costs one line to
/// accept and a server that used it would otherwise look like a hang.
#[derive(Debug, Default)]
pub struct Framer {
    buf: Vec<u8>,
}

impl Framer {
    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// The next complete message, if one has arrived.
    pub fn next(&mut self) -> Option<Value> {
        loop {
            let (head_len, body_len) = self.header()?;
            if self.buf.len() < head_len + body_len {
                return None;
            }
            let body = self.buf[head_len..head_len + body_len].to_vec();
            self.buf.drain(..head_len + body_len);
            // A message that is not JSON is a framing error, not a message. Dropping it and
            // carrying on beats stopping: the stream is still aligned, because the length
            // header said where the next one starts.
            if let Ok(v) = serde_json::from_slice(&body) {
                return Some(v);
            }
        }
    }

    /// Where the headers end and how long the body is, once both are known.
    fn header(&self) -> Option<(usize, usize)> {
        let end = find(&self.buf, b"\r\n\r\n")
            .map(|i| (i + 4, i))
            .or_else(|| find(&self.buf, b"\n\n").map(|i| (i + 2, i)))?;
        let (head_len, head_end) = end;
        let head = std::str::from_utf8(&self.buf[..head_end]).ok()?;
        for line in head.split(['\r', '\n']).filter(|l| !l.is_empty()) {
            let Some((name, value)) = line.split_once(':') else { continue };
            if name.eq_ignore_ascii_case("content-length") {
                return value.trim().parse().ok().map(|n| (head_len, n));
            }
        }
        None
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Frame a message for sending.
pub fn encode(msg: &Value) -> Vec<u8> {
    let body = serde_json::to_vec(msg).unwrap_or_default();
    let mut out = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    out.extend_from_slice(&body);
    out
}

// -- file URIs ---------------------------------------------------------------

/// A path as LSP wants it: `file:///` and percent-encoded.
///
/// Worth doing properly rather than formatting a string, because the paths horde is pointed
/// at have spaces in them — `20 Areas/TAW/Dev` is a real directory in the vault this was
/// built against, and a raw space in a URI is one a server is entitled to reject.
pub fn to_uri(path: &Path) -> String {
    let mut out = String::from("file://");
    for byte in path.to_string_lossy().as_bytes() {
        match byte {
            b'/' | b'-' | b'_' | b'.' | b'~' => out.push(*byte as char),
            b if b.is_ascii_alphanumeric() => out.push(*b as char),
            b => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Back again, for diagnostics arriving about a file.
pub fn from_uri(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let mut out: Vec<u8> = Vec::with_capacity(rest.len());
    let bytes = rest.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?;
            if let Ok(b) = u8::from_str_radix(hex, 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    Some(PathBuf::from(String::from_utf8(out).ok()?))
}

// -- which language a file is --------------------------------------------------

/// The usual name for the language a file is written in.
///
/// Deliberately wider than the set of grammars horde was compiled with: wanting diagnostics
/// for C++ has nothing to do with whether this build can colour it. Anything missing is one
/// `extensions = [...]` line away, which is also the answer for a language nobody here has
/// heard of.
fn guess(ext: &str) -> Option<&'static str> {
    Some(match ext {
        "rs" => "rust",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" => "cpp",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "typescriptreact",
        "js" | "mjs" | "cjs" => "javascript",
        "jsx" => "javascriptreact",
        "py" | "pyi" => "python",
        "go" => "go",
        "rb" => "ruby",
        "java" => "java",
        "kt" | "kts" => "kotlin",
        "lua" => "lua",
        "zig" => "zig",
        "swift" => "swift",
        "cs" => "csharp",
        "php" => "php",
        "hs" => "haskell",
        "ml" | "mli" => "ocaml",
        "ex" | "exs" => "elixir",
        "scala" | "sc" => "scala",
        "html" | "htm" => "html",
        "css" => "css",
        "scss" | "sass" => "scss",
        "json" | "jsonc" => "json",
        "yaml" | "yml" => "yaml",
        "toml" => "toml",
        "md" | "markdown" => "markdown",
        "sh" | "bash" | "zsh" => "shellscript",
        "sql" => "sql",
        "nix" => "nix",
        _ => return None,
    })
}

/// The configured language this file belongs to, if any server has claimed it.
///
/// An explicit `extensions` list wins over the guess, so pointing one server at a language it
/// was not written for is a config change rather than a horde change.
pub fn language_for(cfg: &crate::config::Config, path: &Path) -> Option<String> {
    if cfg.lsp.is_empty() {
        return None;
    }
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    for (name, spec) in &cfg.lsp {
        if spec.extensions.contains(&ext) {
            return Some(name.clone());
        }
    }
    let guessed = guess(&ext)?;
    cfg.lsp.contains_key(guessed).then(|| guessed.to_string())
}

// -- one server --------------------------------------------------------------

pub struct Server {
    pub lang: String,
    pub state: State,
    /// What was spawned, for the sidebar and for a notice when it dies.
    pub command: String,
    /// Bytes on their way to the child. A task owns stdin, so sending is not an await —
    /// the same shape as a pane's outbound queue, and for the same reason.
    tx: Option<mpsc::UnboundedSender<Vec<u8>>>,
    child: Option<tokio::process::Child>,
    /// The last lines the child wrote to stderr, which is where a server that refuses to
    /// start explains itself.
    stderr: Arc<Mutex<Vec<String>>>,
    next_id: i64,
    /// Documents this server has been told about, and the version each is at.
    open: HashMap<String, i64>,
    /// The diagnostics it last published, per file.
    pub diags: HashMap<PathBuf, Vec<Diag>>,
    /// When a document last passed through. What idle shutdown measures.
    pub last_used: Instant,
    pub restarts: u32,
    /// When a `Waiting` server may be tried again.
    retry_at: Option<Instant>,
}

impl Server {
    /// How many files this server is watching. Zero for long enough is what idle means.
    pub fn open_count(&self) -> usize {
        self.open.len()
    }

    /// The last thing the child said on stderr.
    ///
    /// Which is where a server explains itself when it will not start — a wrong path, a
    /// missing rustup component, a Node version it refuses to run under. Without this the
    /// only symptom is a server that is never ready.
    pub fn last_error(&self) -> Option<String> {
        self.stderr.lock().ok().and_then(|t| t.last().cloned())
    }

    /// Every diagnostic it has published, counted by severity, for the chrome.
    pub fn counts(&self) -> (usize, usize) {
        let mut errors = 0;
        let mut warnings = 0;
        for list in self.diags.values() {
            for d in list {
                match d.severity {
                    Severity::Error => errors += 1,
                    Severity::Warning => warnings += 1,
                    _ => {}
                }
            }
        }
        (errors, warnings)
    }

    fn send(&mut self, msg: Value) {
        if let Some(tx) = self.tx.as_ref() {
            let _ = tx.send(encode(&msg));
        }
    }

    fn notify(&mut self, method: &str, params: Value) {
        self.send(json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }

    /// Send a request and hand back the id its answer will carry.
    fn request(&mut self, method: &str, params: Value) -> i64 {
        self.next_id += 1;
        let id = self.next_id;
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        id
    }
}

/// The capabilities horde actually has. Claiming more would invite messages it would drop.
fn capabilities() -> Value {
    json!({
        "textDocument": {
            "synchronization": { "dynamicRegistration": false, "didSave": false },
            "publishDiagnostics": { "relatedInformation": false, "versionSupport": false },
            "completion": {
                "dynamicRegistration": false,
                "completionItem": { "snippetSupport": false, "documentationFormat": ["plaintext"] }
            },
            "hover": { "dynamicRegistration": false, "contentFormat": ["plaintext"] }
        },
        "workspace": { "workspaceFolders": true, "configuration": true },
        "window": { "workDoneProgress": true }
    })
}

// -- the supervisor ----------------------------------------------------------

pub struct Registry {
    servers: HashMap<Key, Server>,
    tx: mpsc::UnboundedSender<Event>,
    rx: mpsc::UnboundedReceiver<Event>,
}

impl Default for Registry {
    fn default() -> Registry {
        Registry::new()
    }
}

impl Registry {
    pub fn new() -> Registry {
        let (tx, rx) = mpsc::unbounded_channel();
        Registry { servers: HashMap::new(), tx, rx }
    }

    pub fn get(&self, key: &Key) -> Option<&Server> {
        self.servers.get(key)
    }

    /// Everything running, for the sidebar. Nothing horde starts may be invisible.
    pub fn serving(&self) -> Vec<(&Key, &Server)> {
        let mut out: Vec<(&Key, &Server)> = self.servers.iter().collect();
        out.sort_by(|a, b| a.0.cmp(b.0));
        out
    }

    /// What is already known about a file, from whichever server is watching it.
    ///
    /// Needed because a language server publishes when something *changes*. Handing it a
    /// document it already has, at text it has already seen, is answered with silence — so a
    /// client that opens a file a moment after another one did would otherwise see a clean
    /// editor over a file full of errors.
    pub fn diags_for(&self, path: &Path) -> Option<&Vec<Diag>> {
        self.servers.values().find_map(|s| s.diags.get(path))
    }

    /// Start a server for this project and language, unless one is already up or the last
    /// attempt is still cooling off.
    ///
    /// Lazy on purpose: opening horde must not spawn anything, and a project whose files you
    /// never open must not either.
    pub fn ensure(&mut self, cfg: &crate::config::Config, root: &Path, lang: &str) -> bool {
        let key = (root.to_path_buf(), lang.to_string());
        if let Some(s) = self.servers.get(&key) {
            match &s.state {
                State::Failed(_) => return false,
                State::Waiting(_) if s.retry_at.is_some_and(|t| Instant::now() < t) => {
                    return false
                }
                State::Waiting(_) => {}
                _ => return true,
            }
        }
        let Some(spec) = cfg.lsp.get(lang) else { return false };
        let restarts = self.servers.get(&key).map(|s| s.restarts).unwrap_or(0);
        match spawn(spec, root, lang, self.tx.clone(), key.clone()) {
            Ok(mut server) => {
                server.restarts = restarts;
                let id = server.request(
                    "initialize",
                    json!({
                        "processId": std::process::id(),
                        "clientInfo": { "name": "horde", "version": env!("CARGO_PKG_VERSION") },
                        "rootUri": to_uri(root),
                        "workspaceFolders": [{
                            "uri": to_uri(root),
                            "name": root.file_name().map(|n| n.to_string_lossy().to_string())
                                .unwrap_or_else(|| "workspace".into()),
                        }],
                        "capabilities": capabilities(),
                    }),
                );
                debug_assert_eq!(id, 1, "initialize is always a server's first request");
                self.servers.insert(key, server);
                true
            }
            Err(e) => {
                // A command that will not even spawn is a config error, not a flaky server.
                // Nothing is gained by trying it again on a timer.
                let mut s = dead(lang, &spec.command);
                s.state = State::Failed(format!("{}: {e}", spec.command));
                self.servers.insert(key, s);
                false
            }
        }
    }

    /// Tell the right server about a document, starting one if this is the first sight of it.
    pub fn did_open(
        &mut self,
        cfg: &crate::config::Config,
        root: &Path,
        lang: &str,
        path: &Path,
        text: &str,
    ) {
        if !self.ensure(cfg, root, lang) {
            return;
        }
        let key = (root.to_path_buf(), lang.to_string());
        let uri = to_uri(path);
        // Already open: this is a re-open, and the server wants a change rather than a second
        // didOpen. Servers differ on how rude the duplicate is; none of them want it.
        if self.servers.get(&key).is_some_and(|s| s.open.contains_key(&uri)) {
            self.did_change(root, lang, path, text);
            return;
        }
        let Some(s) = self.servers.get_mut(&key) else { return };
        s.last_used = Instant::now();
        s.open.insert(uri.clone(), 1);
        let lang_id = lang.to_string();
        s.notify(
            "textDocument/didOpen",
            json!({ "textDocument": {
                "uri": uri, "languageId": lang_id, "version": 1, "text": text,
            }}),
        );
    }

    /// The whole document, every time.
    ///
    /// Incremental sync would send only what changed, which matters for a file being typed
    /// into at speed — but it means horde and the server keeping two copies in step, and a
    /// drift between them is a server confidently reporting errors on text nobody wrote. The
    /// spec allows a whole-document change whatever sync mode a server prefers.
    pub fn did_change(&mut self, root: &Path, lang: &str, path: &Path, text: &str) {
        let key = (root.to_path_buf(), lang.to_string());
        let uri = to_uri(path);
        let Some(s) = self.servers.get_mut(&key) else { return };
        let Some(v) = s.open.get_mut(&uri) else { return };
        *v += 1;
        let version = *v;
        s.last_used = Instant::now();
        s.notify(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": uri, "version": version },
                "contentChanges": [{ "text": text }],
            }),
        );
    }

    pub fn did_close(&mut self, root: &Path, lang: &str, path: &Path) {
        let key = (root.to_path_buf(), lang.to_string());
        let uri = to_uri(path);
        let Some(s) = self.servers.get_mut(&key) else { return };
        if s.open.remove(&uri).is_none() {
            return;
        }
        s.notify("textDocument/didClose", json!({ "textDocument": { "uri": uri } }));
        // The file is gone from the screen, so its diagnostics are gone from the chrome.
        s.diags.remove(path);
    }

    /// Everything the servers have said since the last look.
    ///
    /// Drained on the tick rather than pushed into the engine's own channel: diagnostics are
    /// not worth waking a detached daemon for, and a tick is sixteen milliseconds away.
    pub fn drain(&mut self) -> Vec<Event> {
        let mut out = Vec::new();
        while let Ok(ev) = self.rx.try_recv() {
            self.apply(&ev);
            out.push(ev);
        }
        out
    }

    /// Fold an event into the server it came from, so the registry stays the one place that
    /// knows what a server currently thinks.
    fn apply(&mut self, ev: &Event) {
        match ev {
            Event::Ready(key) => {
                if let Some(s) = self.servers.get_mut(key) {
                    s.state = State::Ready;
                    s.notify("initialized", json!({}));
                    // Anything opened while it was starting has to be replayed: notifications
                    // sent before `initialized` are ones a server is entitled to ignore.
                    let pending: Vec<String> = s.open.keys().cloned().collect();
                    s.open.clear();
                    for uri in pending {
                        if let Some(path) = from_uri(&uri) {
                            if let Ok(text) = std::fs::read_to_string(&path) {
                                let lang = s.lang.clone();
                                s.open.insert(uri.clone(), 1);
                                s.notify(
                                    "textDocument/didOpen",
                                    json!({ "textDocument": {
                                        "uri": uri, "languageId": lang, "version": 1, "text": text,
                                    }}),
                                );
                            }
                        }
                    }
                }
            }
            Event::Diagnostics { key, path, diags } => {
                if let Some(s) = self.servers.get_mut(key) {
                    if diags.is_empty() {
                        s.diags.remove(path);
                    } else {
                        s.diags.insert(path.clone(), diags.clone());
                    }
                }
            }
            Event::Exited { key, why } => {
                if let Some(s) = self.servers.get_mut(key) {
                    s.tx = None;
                    s.child = None;
                    s.open.clear();
                    s.diags.clear();
                    s.restarts += 1;
                    if s.restarts > MAX_RESTARTS {
                        s.state = State::Failed(format!("{why} (gave up after {MAX_RESTARTS})"));
                    } else {
                        s.state = State::Waiting(why.clone());
                        s.retry_at = Some(Instant::now() + BACKOFF * 2u32.pow(s.restarts - 1));
                    }
                }
            }
        }
    }

    /// Give a server that died its next attempt, once its backoff has run out.
    ///
    /// Restarting is on a timer rather than on the next keystroke because the common cause of
    /// a death is a server that will die again — and a language server relaunched on every
    /// edit is a fork bomb with a spinner.
    pub fn retry(&mut self, cfg: &crate::config::Config) -> Vec<Key> {
        let now = Instant::now();
        let due: Vec<Key> = self
            .servers
            .iter()
            .filter(|(_, s)| {
                matches!(s.state, State::Waiting(_)) && s.retry_at.is_some_and(|t| now >= t)
            })
            .map(|(k, _)| k.clone())
            .collect();
        for key in &due {
            let (root, lang) = key.clone();
            self.ensure(cfg, &root, &lang);
        }
        due
    }

    /// Stop servers nobody is using, and forget projects that have closed.
    ///
    /// Called on the slow cadence. This is the half of the lifecycle that is easy to leave
    /// for later and expensive to have left.
    pub fn sweep(&mut self, live: impl Fn(&Path) -> bool) -> Vec<Key> {
        let now = Instant::now();
        let mut stopped = Vec::new();
        let keys: Vec<Key> = self.servers.keys().cloned().collect();
        for key in keys {
            let Some(s) = self.servers.get(&key) else { continue };
            let idle = now.duration_since(s.last_used) > IDLE;
            let gone = !live(&key.0);
            if idle || gone {
                self.stop(&key);
                stopped.push(key);
            }
        }
        stopped
    }

    /// Ask a server to stop, then make sure it did.
    ///
    /// `shutdown` then `exit` is the polite sequence and the one that lets a server flush its
    /// caches. The kill is not a fallback so much as an admission: a language server that has
    /// wedged will not read the request telling it to leave.
    pub fn stop(&mut self, key: &Key) {
        let Some(mut s) = self.servers.remove(key) else { return };
        if matches!(s.state, State::Ready | State::Starting) {
            s.request("shutdown", Value::Null);
            s.notify("exit", Value::Null);
        }
        s.tx = None;
        if let Some(mut child) = s.child.take() {
            tokio::spawn(async move {
                // A moment to go on its own terms, then not.
                tokio::time::sleep(Duration::from_millis(400)).await;
                let _ = child.kill().await;
            });
        }
    }

    /// Stop everything. The daemon is going away and the children must not outlive it.
    pub fn shutdown(&mut self) {
        for key in self.servers.keys().cloned().collect::<Vec<_>>() {
            self.stop(&key);
        }
    }
}

/// A placeholder for a server that never ran, so the failure has somewhere to live.
fn dead(lang: &str, command: &str) -> Server {
    Server {
        lang: lang.to_string(),
        state: State::Starting,
        command: command.to_string(),
        tx: None,
        child: None,
        stderr: Arc::new(Mutex::new(Vec::new())),
        next_id: 0,
        open: HashMap::new(),
        diags: HashMap::new(),
        last_used: Instant::now(),
        restarts: 0,
        retry_at: None,
    }
}

/// Spawn the child and the three tasks that talk to it.
fn spawn(
    spec: &crate::config::LspServer,
    root: &Path,
    lang: &str,
    events: mpsc::UnboundedSender<Event>,
    key: Key,
) -> anyhow::Result<Server> {
    let mut cmd = tokio::process::Command::new(&spec.command);
    cmd.args(&spec.args)
        .current_dir(root)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn()?;

    let mut stdin = child.stdin.take().ok_or_else(|| anyhow::anyhow!("no stdin"))?;
    let mut stdout = child.stdout.take().ok_or_else(|| anyhow::anyhow!("no stdout"))?;
    let mut stderr = child.stderr.take().ok_or_else(|| anyhow::anyhow!("no stderr"))?;

    let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
    tokio::spawn(async move {
        while let Some(bytes) = rx.recv().await {
            if stdin.write_all(&bytes).await.is_err() {
                break;
            }
            let _ = stdin.flush().await;
        }
    });

    // Kept rather than inherited: a server that refuses to start says why here and nowhere
    // else, and inheriting it would paint the reason over whatever pane horde is drawing.
    let tail = Arc::new(Mutex::new(Vec::<String>::new()));
    let tail2 = tail.clone();
    tokio::spawn(async move {
        let mut buf = [0u8; 4096];
        let mut line = String::new();
        while let Ok(n) = stderr.read(&mut buf).await {
            if n == 0 {
                break;
            }
            line.push_str(&String::from_utf8_lossy(&buf[..n]));
            while let Some(i) = line.find('\n') {
                let one: String = line.drain(..=i).collect();
                let one = one.trim_end().to_string();
                if one.is_empty() {
                    continue;
                }
                if let Ok(mut t) = tail2.lock() {
                    t.push(one);
                    if t.len() > STDERR_KEEP {
                        t.remove(0);
                    }
                }
            }
        }
    });

    let tail3 = tail.clone();
    let out_key = key.clone();
    // The reader answers the server's own requests directly. Routing them through the engine
    // would mean a stalled server every time the daemon is busy, for replies that carry no
    // information and only exist to unblock the other side.
    let back = tx.clone();
    tokio::spawn(async move {
        let mut framer = Framer::default();
        let mut buf = [0u8; 16384];
        loop {
            let n = match stdout.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            framer.push(&buf[..n]);
            while let Some(msg) = framer.next() {
                if !dispatch(&msg, &out_key, &events, &back) {
                    break;
                }
            }
        }
        // Stdout closed: the child is gone, or on its way. Whatever it last said on stderr is
        // the only explanation there will be.
        let why = tail3
            .lock()
            .ok()
            .and_then(|t| t.last().cloned())
            .unwrap_or_else(|| "exited".to_string());
        let _ = events.send(Event::Exited { key: out_key, why });
    });

    Ok(Server {
        lang: lang.to_string(),
        state: State::Starting,
        command: spec.command.clone(),
        tx: Some(tx.clone()),
        child: Some(child),
        stderr: tail,
        next_id: 0,
        open: HashMap::new(),
        diags: HashMap::new(),
        last_used: Instant::now(),
        restarts: 0,
        retry_at: None,
    })
}

/// Turn one incoming message into an event, or answer it on the spot.
///
/// Returns false when the reader should stop.
fn dispatch(
    msg: &Value,
    key: &Key,
    events: &mpsc::UnboundedSender<Event>,
    writer: &mpsc::UnboundedSender<Vec<u8>>,
) -> bool {
    let method = msg.get("method").and_then(|m| m.as_str());
    let id = msg.get("id").and_then(|i| i.as_i64());

    match (method, id) {
        // A notification from the server.
        (Some("textDocument/publishDiagnostics"), None) => {
            if let Some(params) = msg.get("params") {
                if let Some(path) = params.get("uri").and_then(|u| u.as_str()).and_then(from_uri) {
                    let diags = params
                        .get("diagnostics")
                        .and_then(|d| d.as_array())
                        .map(|a| a.iter().filter_map(parse_diag).collect())
                        .unwrap_or_default();
                    let _ =
                        events.send(Event::Diagnostics { key: key.clone(), path, diags });
                }
            }
            true
        }
        (Some(_), None) => true,
        // A reply to something horde asked. `error` counts: a server that refuses answers the
        // request rather than the intent, and waiting only for `result` waits forever.
        (None, Some(id)) => {
            if id == 1 {
                let _ = events.send(Event::Ready(key.clone()));
            }
            if let Some(err) = msg.get("error") {
                let why = err
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("request refused")
                    .to_string();
                if id == 1 {
                    let _ = events.send(Event::Exited { key: key.clone(), why });
                    return false;
                }
            }
            true
        }
        // A request *from* the server. Every one gets an answer even when the answer is
        // nothing: rust-analyzer stalls on an unanswered `client/registerCapability`, and a
        // stalled server looks exactly like a broken one.
        (Some(m), Some(id)) => {
            let _ = writer.send(encode(&json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": server_request_result(m, msg),
            })));
            true
        }
        (None, None) => true,
    }
}

/// What to answer a server that asked horde something.
///
/// Nothing, in every case — but the *shape* of nothing matters. `workspace/configuration`
/// asks about several settings at once and expects one answer per item, so a bare null there
/// is a reply the server cannot read.
fn server_request_result(method: &str, msg: &Value) -> Value {
    if method == "workspace/configuration" {
        let n = msg
            .get("params")
            .and_then(|p| p.get("items"))
            .and_then(|i| i.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        return Value::Array(vec![Value::Null; n]);
    }
    Value::Null
}

fn parse_diag(v: &Value) -> Option<Diag> {
    let range = v.get("range")?;
    let start = range.get("start")?;
    let end = range.get("end")?;
    Some(Diag {
        line: start.get("line")?.as_u64().unwrap_or(0) as u32,
        col: start.get("character")?.as_u64().unwrap_or(0) as u32,
        end_line: end.get("line")?.as_u64().unwrap_or(0) as u32,
        end_col: end.get("character")?.as_u64().unwrap_or(0) as u32,
        severity: severity_from_lsp(v.get("severity").and_then(|s| s.as_u64()).unwrap_or(1)),
        message: v.get("message")?.as_str()?.trim().to_string(),
        source: v.get("source").and_then(|s| s.as_str()).map(|s| s.to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The framer is the one piece that sees every byte, and it has to hold together across
    /// arbitrary splits — a language server writing a big diagnostic set does not arrange for
    /// it to arrive in one read.
    #[test]
    fn messages_reassemble_however_the_bytes_arrive() {
        let a = encode(&json!({ "method": "one" }));
        let b = encode(&json!({ "method": "two", "params": { "x": 1 } }));
        let stream: Vec<u8> = a.iter().chain(b.iter()).copied().collect();

        for chunk in [1usize, 3, 7, 64, 4096] {
            let mut f = Framer::default();
            let mut got = Vec::new();
            for part in stream.chunks(chunk) {
                f.push(part);
                while let Some(m) = f.next() {
                    got.push(m);
                }
            }
            assert_eq!(got.len(), 2, "split every {chunk} bytes");
            assert_eq!(got[0]["method"], "one");
            assert_eq!(got[1]["params"]["x"], 1);
        }
    }

    /// Both servers phase 0 tried used CRLF, but accepting bare LF costs one line and a
    /// server that used it would present as a hang rather than as an error.
    #[test]
    fn headers_may_use_either_line_ending_and_any_case() {
        let body = br#"{"method":"hi"}"#;
        let mut f = Framer::default();
        // Lowercase header, bare LF, and the body one byte short of complete.
        f.push(format!("content-length: {}\n\n", body.len()).as_bytes());
        f.push(&body[..body.len() - 1]);
        assert!(f.next().is_none(), "a body one byte short is not a message yet");
        f.push(&body[body.len() - 1..]);
        assert_eq!(f.next().expect("now it is one")["method"], "hi");
    }

    /// A message that is not JSON is a framing error, not a reason to stop reading: the
    /// length header already said where the next one starts.
    #[test]
    fn a_broken_message_is_skipped_rather_than_stopping_the_stream() {
        let mut f = Framer::default();
        f.push(b"Content-Length: 3\r\n\r\nnot");
        f.push(&encode(&json!({ "method": "after" })));
        let msg = f.next().expect("the good one still arrives");
        assert_eq!(msg["method"], "after");
    }

    /// The vault this was built against has `20 Areas/TAW/Dev` in it. A raw space in a URI is
    /// one a server is entitled to reject, and a path that does not round-trip is diagnostics
    /// attached to a file nobody has open.
    #[test]
    fn paths_survive_the_round_trip_through_a_uri() {
        for p in [
            "/tmp/plain.rs",
            "/Users/josh/Documents/Brain/20 Areas/TAW/Dev/note.md",
            "/tmp/a+b/c#d/e?f.rs",
            "/tmp/café/naïve.rs",
        ] {
            let uri = to_uri(Path::new(p));
            assert!(!uri.contains(' '), "no raw spaces: {uri}");
            assert_eq!(from_uri(&uri).as_deref(), Some(Path::new(p)), "round trip of {p}");
        }
        assert_eq!(from_uri("http://example.com"), None, "and only file URIs");
    }

    fn cfg_with(lang: &str, command: &str, args: &[&str]) -> crate::config::Config {
        let mut cfg = crate::config::Config::default();
        cfg.lsp.insert(
            lang.to_string(),
            crate::config::LspServer {
                command: command.to_string(),
                args: args.iter().map(|a| a.to_string()).collect(),
                env: HashMap::new(),
                extensions: Vec::new(),
            },
        );
        cfg
    }

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("horde-lsp-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A filename finds a server through the config's own list first, so a language horde has
    /// never heard of is a config line rather than a code change.
    #[test]
    fn a_file_finds_its_server_by_extension() {
        let cfg = cfg_with("rust", "rust-analyzer", &[]);
        assert_eq!(language_for(&cfg, Path::new("/p/src/main.rs")).as_deref(), Some("rust"));
        assert_eq!(language_for(&cfg, Path::new("/p/notes.md")), None, "nothing claims it");

        let mut cfg = cfg_with("cpp", "clangd", &[]);
        cfg.lsp.get_mut("cpp").unwrap().extensions = vec!["ino".into()];
        assert_eq!(
            language_for(&cfg, Path::new("/p/sketch.ino")).as_deref(),
            Some("cpp"),
            "an explicit list beats the built-in guess"
        );
        assert_eq!(
            language_for(&cfg, Path::new("/p/a.cpp")).as_deref(),
            Some("cpp"),
            "and the guess still works for the obvious ones"
        );
        assert_eq!(
            language_for(&crate::config::Config::default(), Path::new("/p/main.rs")),
            None,
            "with nothing configured, nothing is claimed and nothing will spawn"
        );
    }

    /// The failure phase 0 hit first: a command that is not there. Waiting for a reply that
    /// is never coming is the one outcome that must not happen, because it presents as horde
    /// being broken rather than as the config being wrong.
    #[tokio::test]
    async fn a_server_that_cannot_start_fails_immediately_rather_than_hanging() {
        let root = tmp("nocmd");
        let cfg = cfg_with("rust", "definitely-not-a-language-server-8f3a", &[]);
        let mut reg = Registry::new();
        assert!(!reg.ensure(&cfg, &root, "rust"), "it says so at the call site");

        let key = (root.clone(), "rust".to_string());
        let state = reg.get(&key).map(|s| s.state.clone());
        assert!(matches!(state, Some(State::Failed(_))), "{state:?}");
        // And it stays failed: a command that does not exist will not exist in two seconds.
        assert!(reg.retry(&cfg).is_empty());
        assert!(!reg.ensure(&cfg, &root, "rust"), "no second spawn attempt");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The other one: a server that starts and dies. There is no reply and no error message —
    /// only stdout closing — so the death has to be noticed from the pipe rather than waited
    /// out, and it has to come back with whatever was on stderr.
    #[tokio::test]
    async fn a_server_that_dies_at_startup_is_noticed_and_backed_off() {
        let root = tmp("dies");
        let cfg = cfg_with("rust", "sh", &["-c", "echo bad config >&2; exit 1"]);
        let mut reg = Registry::new();
        assert!(reg.ensure(&cfg, &root, "rust"), "it spawned");

        let key = (root.clone(), "rust".to_string());
        let mut events = Vec::new();
        for _ in 0..100 {
            events.extend(reg.drain());
            if events.iter().any(|e| matches!(e, Event::Exited { .. })) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let why = events
            .iter()
            .find_map(|e| match e {
                Event::Exited { why, .. } => Some(why.clone()),
                _ => None,
            })
            .expect("the death was noticed");
        assert!(why.contains("bad config"), "with what it said on stderr: {why}");

        let state = reg.get(&key).map(|s| s.state.clone());
        assert!(matches!(state, Some(State::Waiting(_))), "waiting to try again: {state:?}");
        assert!(reg.retry(&cfg).is_empty(), "and not this instant — that is what backoff is");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The whole thing against a real language server, if one is installed: start it, open a
    /// file with a mistake in it, get told about the mistake, fix it, get told it is fixed.
    ///
    /// Skipped rather than failed when `clangd` is absent, because whether this machine has a
    /// C++ toolchain is not something horde's correctness depends on.
    #[tokio::test]
    async fn a_real_server_reports_a_mistake_and_then_clears_it() {
        let Ok(out) = std::process::Command::new("which").arg("clangd").output() else { return };
        if !out.status.success() {
            eprintln!("skipping: clangd is not installed");
            return;
        }
        let root = tmp("clangd");
        let file = root.join("main.c");
        std::fs::write(&file, "int main(void) { return notdeclared; }\n").unwrap();
        let cfg = cfg_with("c", "clangd", &[]);

        let mut reg = Registry::new();
        reg.did_open(&cfg, &root, "c", &file, &std::fs::read_to_string(&file).unwrap());
        assert_eq!(reg.serving().len(), 1, "one server, and it is visible");

        let found = wait_for(&mut reg, |r| {
            r.diags_for(&file).is_some_and(|d| d.iter().any(|d| d.severity == Severity::Error))
        })
        .await;
        assert!(found, "clangd should have objected to an undeclared identifier");

        let diags = reg.diags_for(&file).unwrap().clone();
        let d = diags.iter().find(|d| d.severity == Severity::Error).unwrap();
        assert_eq!(d.line, 0, "on the line the mistake is on");
        assert!(!d.message.is_empty());

        let key = (root.clone(), "c".to_string());
        assert_eq!(reg.get(&key).unwrap().counts().0, diags.iter().filter(|d| d.severity == Severity::Error).count());
        assert_eq!(reg.get(&key).unwrap().open_count(), 1);

        // Fix it in the buffer only — the file on disk is not touched, which is the point:
        // diagnostics have to follow what you are typing, not what you last saved.
        reg.did_change(&root, "c", &file, "int main(void) { return 0; }\n");
        let cleared = wait_for(&mut reg, |r| r.diags_for(&file).is_none()).await;
        assert!(cleared, "and they clear when the mistake goes");

        reg.did_close(&root, "c", &file);
        assert_eq!(reg.get(&key).unwrap().open_count(), 0);
        reg.shutdown();
        assert!(reg.serving().is_empty(), "and it stops when told to");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Drain and wait until something is true, or give up. Language servers are allowed to be
    /// slow — phase 0 clocked rust-analyzer at eleven seconds to first diagnostics.
    async fn wait_for(reg: &mut Registry, done: impl Fn(&Registry) -> bool) -> bool {
        for _ in 0..300 {
            reg.drain();
            if done(reg) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        false
    }

    #[test]
    fn diagnostics_parse_into_what_the_editor_needs() {
        let v = json!({
            "range": { "start": { "line": 3, "character": 8 }, "end": { "line": 3, "character": 12 } },
            "severity": 2,
            "source": "rustc",
            "message": "  unused variable: `x`\n",
        });
        let d = parse_diag(&v).expect("parsed");
        assert_eq!((d.line, d.col, d.end_line, d.end_col), (3, 8, 3, 12));
        assert_eq!(d.severity, Severity::Warning);
        assert_eq!(d.message, "unused variable: `x`", "trimmed, because it is drawn on a line");
        assert_eq!(d.source.as_deref(), Some("rustc"));

        // No severity means error: LSP says a server may omit it, and the safe reading of an
        // unlabelled complaint is the loud one.
        let v = json!({
            "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 1 } },
            "message": "bad",
        });
        assert_eq!(parse_diag(&v).unwrap().severity, Severity::Error);
    }
}
