//! Shared helpers for the integration tests: a fake HTTP server and a
//! builder that runs the real `hunch` binary in an isolated environment.
//!
//! # Why `tests/support/mod.rs` and not `tests/support.rs`?
//!
//! Cargo treats every *file* directly inside `tests/` as a separate
//! integration-test crate. A `tests/support.rs` would therefore be compiled
//! and run as its own (empty) test binary, showing up as "running 0 tests"
//! in every `cargo test`. Files in a *subdirectory* are not picked up on
//! their own; they only get compiled when a test crate includes them with
//! `mod support;`. That is the conventional way to share code between test
//! files.
//!
//! # Why `allow(dead_code)`?
//!
//! Each test crate (`cli.rs`, `live.rs`) compiles its *own copy* of this
//! module, and none of them uses every helper (`live.rs` never starts a fake
//! server). Without the allow, each crate would warn about the parts it does
//! not use, and `clippy -D warnings` would fail.
//!
//! # What the fake server is (and is not)
//!
//! Just enough HTTP/1.1, built on `std::net`, to stand in for the provider
//! APIs: one scripted response per connection, `Connection: close`, no
//! keep-alive, no TLS, no compression. Crates like `wiremock` or `httpmock`
//! do this (and much more); writing it by hand shows there is no magic.
#![allow(dead_code)]

use std::collections::VecDeque;
use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;

// --- fake HTTP server --------------------------------------------------------

/// A response the fake server will send, in the order they were queued.
#[derive(Debug, Clone)]
pub struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Reply {
    /// A response with a JSON body (the body is sent exactly as given, so
    /// tests can check that `--json` prints it verbatim).
    pub fn json(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            headers: vec![("Content-Type".into(), "application/json".into())],
            body: body.into(),
        }
    }

    /// Adds a header, builder style: `Reply::json(429, "{}").header("Retry-After", "0")`.
    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }
}

/// One request as the fake server saw it.
#[derive(Debug, Clone)]
pub struct Captured {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Captured {
    /// Header lookup, case-insensitive like HTTP itself: ureq may send
    /// `authorization` or `Authorization`, both mean the same.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// The body parsed as JSON. Comparing `Value`s (instead of strings)
    /// makes the assertion independent of key order and whitespace.
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|err| {
            panic!(
                "request body is not JSON ({err}): {}",
                String::from_utf8_lossy(&self.body)
            )
        })
    }
}

/// A fake provider API on `127.0.0.1:<random port>`.
///
/// Binding to port 0 lets the OS pick a free port, so every test gets its
/// own server and tests can run in parallel without clashing.
pub struct FakeServer {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<Captured>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl FakeServer {
    /// Starts a server that answers the n-th connection with `replies[n]`.
    /// Connections beyond the script get a 500, so an unexpected extra
    /// request shows up as a clear failure instead of a hang.
    pub fn start(replies: impl IntoIterator<Item = Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake server");
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let mut replies: VecDeque<Reply> = replies.into_iter().collect();

        // The thread gets its own handles (`Arc::clone`) to the shared state;
        // the test keeps the originals to read captured requests later.
        let thread = thread::spawn({
            let requests = Arc::clone(&requests);
            let stop = Arc::clone(&stop);
            move || {
                for stream in listener.incoming() {
                    if stop.load(Ordering::SeqCst) {
                        break;
                    }
                    let Ok(stream) = stream else { continue };
                    let reply = replies.pop_front().unwrap_or_else(|| {
                        Reply::json(500, r#"{"error":"fake server: no scripted reply left"}"#)
                    });
                    // A broken connection is the client's problem (and the
                    // test will notice); keep serving the next one.
                    let _ = serve(stream, &reply, &requests);
                }
            }
        });

        Self {
            addr,
            requests,
            stop,
            thread: Some(thread),
        }
    }

    /// Base URL to hand to hunch, e.g. `http://127.0.0.1:54321`.
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Every request received so far, in order.
    ///
    /// No waiting needed: the server records a request *before* it writes
    /// the response, and hunch only exits after reading the response, so
    /// once the binary has finished, its requests are all in here.
    pub fn requests(&self) -> Vec<Captured> {
        self.requests.lock().unwrap().clone()
    }

    /// The only request received; panics if there were zero or several.
    pub fn single_request(&self) -> Captured {
        let mut requests = self.requests();
        assert_eq!(requests.len(), 1, "expected exactly one request");
        requests.remove(0)
    }
}

impl Drop for FakeServer {
    /// Stops the background thread. It is blocked in `accept()`, so after
    /// raising the flag we connect once ourselves to wake it up.
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.addr);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Handles one connection: read the request, record it, send the reply.
fn serve(stream: TcpStream, reply: &Reply, requests: &Mutex<Vec<Captured>>) -> io::Result<()> {
    // A client that never finishes its request must not hang the test run.
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let request = read_request(&mut reader)?;
    requests.lock().unwrap().push(request);
    write_reply(stream, reply)
}

/// Parses an HTTP/1.1 request: request line, headers, then a body framed by
/// either `Content-Length` or `Transfer-Encoding: chunked`.
fn read_request(reader: &mut impl BufRead) -> io::Result<Captured> {
    let request_line = read_line(reader)?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();

    let mut headers = Vec::new();
    loop {
        let line = read_line(reader)?;
        if line.is_empty() {
            break; // blank line: end of headers
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_string(), value.trim().to_string()));
        }
    }

    let mut captured = Captured {
        method,
        path,
        headers,
        body: Vec::new(),
    };
    let chunked = captured
        .header("Transfer-Encoding")
        .is_some_and(|value| value.eq_ignore_ascii_case("chunked"));
    captured.body = if chunked {
        read_chunked_body(reader)?
    } else {
        let length = captured
            .header("Content-Length")
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);
        let mut body = vec![0; length];
        reader.read_exact(&mut body)?;
        body
    };
    Ok(captured)
}

/// Chunked encoding: `<hex size>\r\n<data>\r\n` repeated, ending with a
/// zero-size chunk and an (optionally empty) trailer section.
fn read_chunked_body(reader: &mut impl BufRead) -> io::Result<Vec<u8>> {
    let mut body = Vec::new();
    loop {
        let size_line = read_line(reader)?;
        // Chunk extensions (`;name=value`) are allowed after the size.
        let size_hex = size_line.split(';').next().unwrap_or_default().trim();
        let size = usize::from_str_radix(size_hex, 16)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        if size == 0 {
            while !read_line(reader)?.is_empty() {} // skip trailers
            return Ok(body);
        }
        let start = body.len();
        body.resize(start + size, 0);
        reader.read_exact(&mut body[start..])?;
        read_line(reader)?; // the CRLF after each chunk
    }
}

/// One line without its `\r\n`. EOF before a full request is an error.
fn read_line(reader: &mut impl BufRead) -> io::Result<String> {
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    Ok(line.trim_end_matches(['\r', '\n']).to_string())
}

/// Writes the reply with an exact `Content-Length` and `Connection: close`:
/// one request per connection keeps the server trivial. The body is never
/// compressed, whatever `Accept-Encoding` the client sent.
fn write_reply(mut stream: TcpStream, reply: &Reply) -> io::Result<()> {
    let mut head = format!("HTTP/1.1 {} {}\r\n", reply.status, reason(reply.status));
    for (name, value) in &reply.headers {
        head += &format!("{name}: {value}\r\n");
    }
    head += &format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n",
        reply.body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(reply.body.as_bytes())?;
    stream.flush()
}

/// The reason phrase is informational only; clients go by the number.
fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        401 => "Unauthorized",
        422 => "Unprocessable Entity",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Unknown",
    }
}

/// A port on which nothing is listening: bind to get a free port from the
/// OS, then drop the listener so connections to it are refused.
pub fn unused_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

// --- temporary directory -----------------------------------------------------

/// A fresh directory under the system temp dir, deleted again on drop
/// (the `tempfile` crate's `TempDir`, in a dozen lines).
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    pub fn new() -> Self {
        // Process id + per-process counter + time: unique across parallel
        // tests in one run and across concurrent `cargo test` runs.
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .subsec_nanos();
        let name = format!(
            "hunch-test-{}-{}-{nanos}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        );
        let path = std::env::temp_dir().join(name);
        fs::create_dir_all(&path).expect("create temp dir");
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        // Best effort: a leftover temp dir must not turn a pass into a panic.
        let _ = fs::remove_dir_all(&self.path);
    }
}

// --- running the binary ------------------------------------------------------

/// Path of the compiled `hunch` binary. For integration tests, Cargo builds
/// the package's binaries first and exposes each one's location as
/// `CARGO_BIN_EXE_<name>` at compile time, hence `env!` rather than
/// `std::env::var`.
pub const HUNCH: &str = env!("CARGO_BIN_EXE_hunch");

/// Builder for one `hunch` invocation (what `assert_cmd::Command` offers).
///
/// The child process starts from an *empty* environment plus a private
/// `HOME` and `XDG_CONFIG_HOME`, so neither the developer's shell variables
/// (`TYPESAFE_API_KEY`, `HUNCH_DRIVER`, ...) nor their real
/// `~/.config/hunch/config.toml` can change a test's outcome.
pub struct Hunch {
    home: TempDir,
    args: Vec<String>,
    env: Vec<(String, String)>,
    stdin: Option<String>,
}

impl Hunch {
    pub fn new() -> Self {
        Self {
            home: TempDir::new(),
            args: Vec::new(),
            env: Vec::new(),
            stdin: None,
        }
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    pub fn env(mut self, key: &str, value: &str) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    /// Points both drivers at the fake server.
    pub fn server(self, server: &FakeServer) -> Self {
        let url = server.url();
        self.env("TYPESAFE_BASE_URL", &url)
            .env("OPENROUTER_BASE_URL", &url)
    }

    /// Text to pipe into the child's stdin. Without it, stdin is
    /// `/dev/null`: not a terminal, and empty.
    pub fn stdin(mut self, text: &str) -> Self {
        self.stdin = Some(text.into());
        self
    }

    /// Writes `$XDG_CONFIG_HOME/hunch/config.toml`, the default location.
    pub fn config_file(self, contents: &str) -> Self {
        let dir = self.config_home().join("hunch");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("config.toml"), contents).unwrap();
        self
    }

    /// Writes a file into this run's temp dir and returns its path, e.g.
    /// for `--state-file`.
    pub fn write_file(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.home.path().join(name);
        fs::write(&path, contents).unwrap();
        path
    }

    fn config_home(&self) -> PathBuf {
        self.home.path().join(".config")
    }

    /// Runs the binary to completion and captures what it printed.
    pub fn run(self) -> Outcome {
        let mut command = Command::new(HUNCH);
        command
            .args(&self.args)
            .env_clear()
            .env("HOME", self.home.path())
            .env("XDG_CONFIG_HOME", self.config_home())
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .stdin(if self.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // PATH is kept so the child behaves like a normal process; nothing
        // in hunch reads it, but the OS loader or libc may.
        if let Some(path) = std::env::var_os("PATH") {
            command.env("PATH", path);
        }

        let mut child = command.spawn().expect("spawn hunch");
        // Feed stdin from a separate thread: if the child filled its stdout
        // pipe while we were still writing, both sides would wait on each
        // other forever. Dropping `pipe` at the end closes it (EOF).
        let writer = self.stdin.map(|text| {
            let mut pipe = child.stdin.take().unwrap();
            thread::spawn(move || pipe.write_all(text.as_bytes()))
        });
        let output = child.wait_with_output().expect("wait for hunch");
        if let Some(writer) = writer {
            // The child may exit without reading stdin (e.g. on a usage
            // error); a broken pipe then is expected, not a failure.
            let _ = writer.join().unwrap();
        }

        Outcome {
            code: output.status.code(),
            stdout: String::from_utf8(output.stdout).expect("stdout is UTF-8"),
            stderr: String::from_utf8(output.stderr).expect("stderr is UTF-8"),
        }
    }
}

/// What a finished `hunch` run produced.
#[derive(Debug)]
pub struct Outcome {
    /// `None` if the process was killed by a signal.
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl Outcome {
    /// Asserts exit code 0 and returns stdout. On failure the message shows
    /// stderr, which is usually where the explanation is.
    #[track_caller]
    pub fn success(&self) -> &str {
        assert_eq!(self.code, Some(0), "hunch failed: {self:#?}");
        &self.stdout
    }

    /// Asserts the given non-zero exit code and returns stderr.
    #[track_caller]
    pub fn failure(&self, code: i32) -> &str {
        assert_eq!(self.code, Some(code), "unexpected exit code: {self:#?}");
        assert!(
            self.stderr.starts_with("hunch: error: "),
            "errors go to stderr with a prefix: {self:#?}"
        );
        &self.stderr
    }
}
