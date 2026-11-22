//! Test-only same-origin HTTP fixture for navigation probes.
//!
//! One listener, one stable origin. Every request is counted by normalized
//! path only; query strings and bodies are never stored, echoed or logged.
//! `/main-error` and `/child-error` close the connection without an HTTP
//! response so CEF reports a real network load failure instead of a 404.
//! `/slow` advertises that the request arrived and deliberately withholds
//! headers until released (or until its bounded wait expires).
//!
//! These socket tests are HTTP protocol evidence only. They do not execute a
//! page and are not CEF or browser evidence.

use std::{
    collections::BTreeMap,
    io::{Read, Write},
    net::{Shutdown, TcpListener, TcpStream},
    sync::{
        Arc, Condvar, Mutex, MutexGuard,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

pub const PATH_A: &str = "/a";
pub const PATH_B: &str = "/b";
pub const PATH_REDIRECT: &str = "/redirect";
pub const PATH_MAIN_ERROR: &str = "/main-error";
pub const PATH_CHILD_ERROR: &str = "/child-error";
pub const PATH_WITH_CHILD: &str = "/with-child";
pub const PATH_SLOW: &str = "/slow";
pub const PATH_FAVICON: &str = "/favicon.ico";

const ROUTES: [&str; 8] = [
    PATH_A,
    PATH_B,
    PATH_REDIRECT,
    PATH_MAIN_ERROR,
    PATH_CHILD_ERROR,
    PATH_WITH_CHILD,
    PATH_SLOW,
    PATH_FAVICON,
];

const MAX_HEADER_BYTES: usize = 16 * 1024;
const MAX_CONNECTIONS: usize = 32;
const MAX_TOTAL_CONNECTIONS: usize = 256;
const MAX_ERRORS: usize = 32;
const READ_POLL: Duration = Duration::from_millis(50);
const REQUEST_DEADLINE: Duration = Duration::from_secs(2);
const WRITE_TIMEOUT: Duration = Duration::from_millis(200);
const SLOW_RELEASE_TIMEOUT: Duration = Duration::from_secs(20);

const PAGE_A: &str = r#"<!doctype html><html><head><meta charset="utf-8"><title>Nav A</title><style>html,body{margin:0;width:100%;height:100%;background:#1a4c7e;overflow:hidden}</style></head><body></body></html>"#;
const PAGE_B: &str = r#"<!doctype html><html><head><meta charset="utf-8"><title>Nav B</title><style>html,body{margin:0;width:100%;height:100%;background:#1a4c7e;overflow:hidden}</style></head><body></body></html>"#;
const PAGE_PARENT: &str = r#"<!doctype html><html><head><meta charset="utf-8"><title>Nav Parent</title><style>html,body{margin:0;width:100%;height:100%;background:#1a4c7e;overflow:hidden}iframe{width:160px;height:120px;border:0}</style></head><body><iframe src="/child-error"></iframe></body></html>"#;
const PAGE_SLOW: &str = r#"<!doctype html><html><head><meta charset="utf-8"><title>Nav Slow</title><style>html,body{margin:0;width:100%;height:100%;background:#1a4c7e;overflow:hidden}</style></head><body></body></html>"#;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn canonical_route(path: &str) -> Option<&'static str> {
    ROUTES.iter().copied().find(|route| *route == path)
}

#[derive(Default)]
struct Shared {
    counts: BTreeMap<&'static str, u64>,
    errors: Vec<String>,
}

impl Shared {
    fn count(&mut self, route: &'static str) {
        *self.counts.entry(route).or_insert(0) += 1;
    }

    fn record_error(&mut self, message: impl Into<String>) {
        if self.errors.len() < MAX_ERRORS {
            self.errors.push(message.into());
        } else if self.errors.last().map(String::as_str) != Some("additional errors omitted") {
            self.errors.push("additional errors omitted".to_string());
        }
    }
}

/// Server-side write completions only. Socket success does not mean CEF
/// received, parsed, or dispatched a response.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SlowWrites {
    pub released: u64,
    pub expired: u64,
    pub socket_write_ok: u64,
    pub peer_gone: u64,
    pub error: u64,
}

/// One release gate shared by every `/slow` request. A repeated release is
/// harmless; waiting is bounded even when the caller never releases.
#[derive(Default)]
struct SlowGate {
    seen: AtomicU64,
    released: Mutex<bool>,
    signal: Condvar,
    writes: Mutex<SlowWrites>,
}

impl SlowGate {
    fn mark_seen(&self) {
        self.seen.fetch_add(1, Ordering::AcqRel);
    }

    fn wait(&self, stop: &AtomicBool) -> bool {
        let deadline = Instant::now() + SLOW_RELEASE_TIMEOUT;
        let mut released = lock(&self.released);
        while !*released && !stop.load(Ordering::Acquire) {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let slice = (deadline - now).min(Duration::from_millis(50));
            let (next, _) = self
                .signal
                .wait_timeout(released, slice)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            released = next;
        }
        *released
    }

    fn record_write(&self, explicitly_released: bool, result: &Result<bool, String>) {
        let mut s = lock(&self.writes);
        if explicitly_released {
            s.released += 1;
        } else {
            s.expired += 1;
        }
        match result {
            Ok(true) => s.socket_write_ok += 1,
            Ok(false) => s.peer_gone += 1,
            Err(_) => s.error += 1,
        }
    }

    fn release(&self) {
        *lock(&self.released) = true;
        self.signal.notify_all();
    }
}

/// A single-host fixture for the navigation probe. `finish` stops accepting,
/// releases the slow gate and joins every server thread before returning.
pub struct NavigationFixture {
    origin: String,
    shared: Arc<Mutex<Shared>>,
    stop: Arc<AtomicBool>,
    slow: Arc<SlowGate>,
    accept: Option<JoinHandle<()>>,
    workers: Arc<Mutex<Vec<JoinHandle<()>>>>,
    /// Owned by the accept loop; retained here only to join all workers.
    #[allow(dead_code)]
    active: Arc<AtomicU64>,
}

impl NavigationFixture {
    pub fn new() -> std::io::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        listener.set_nonblocking(true)?;
        let origin = format!("http://{}", listener.local_addr()?);
        let shared = Arc::new(Mutex::new(Shared::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let slow = Arc::new(SlowGate::default());
        let workers = Arc::new(Mutex::new(Vec::new()));
        let active = Arc::new(AtomicU64::new(0));
        let accept = {
            let shared = Arc::clone(&shared);
            let stop = Arc::clone(&stop);
            let slow = Arc::clone(&slow);
            let workers = Arc::clone(&workers);
            let active = Arc::clone(&active);
            thread::Builder::new()
                .name("navigation-test-http".to_string())
                .spawn(move || serve(listener, shared, stop, slow, workers, active))?
        };
        Ok(Self {
            origin,
            shared,
            stop,
            slow,
            accept: Some(accept),
            workers,
            active,
        })
    }

    pub fn origin(&self) -> &str {
        &self.origin
    }

    /// Build a URL for one of the fixed routes. No query or fragment is built.
    pub fn url(&self, path: &str) -> Result<String, String> {
        if canonical_route(path).is_none() {
            return Err(format!("unknown navigation fixture path: {path}"));
        }
        Ok(format!("{}{path}", self.origin))
    }

    /// Count of accepted requests for a known normalized path.
    pub fn request_count(&self, path: &str) -> Option<u64> {
        let route = canonical_route(path)?;
        Some(lock(&self.shared).counts.get(route).copied().unwrap_or(0))
    }

    pub fn slow_request_count(&self) -> u64 {
        self.slow.seen.load(Ordering::Acquire)
    }

    /// Counts actual worker writes, not gate releases or renderer acknowledgements.
    pub fn slow_writes(&self) -> SlowWrites {
        *lock(&self.slow.writes)
    }

    /// Let every `/slow` worker attempt its bounded response. The original
    /// connection may already have been closed by browser teardown.
    pub fn release_slow(&self) {
        self.slow.release();
    }

    /// Protocol errors, with no query/body/path echoes.
    pub fn errors(&self) -> Vec<String> {
        lock(&self.shared).errors.clone()
    }

    /// Stop and join every fixture thread, then return the final error list.
    pub fn finish(&mut self) -> Vec<String> {
        self.stop_worker();
        self.errors()
    }

    fn stop_worker(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.slow.release();
        if let Some(accept) = self.accept.take()
            && accept.join().is_err()
        {
            lock(&self.shared).record_error("fixture accept loop panicked");
        }
        let workers: Vec<JoinHandle<()>> = {
            let mut workers = lock(&self.workers);
            std::mem::take(&mut *workers)
        };
        for worker in workers {
            if worker.join().is_err() {
                lock(&self.shared).record_error("fixture worker panicked");
            }
        }
    }
}

impl Drop for NavigationFixture {
    fn drop(&mut self) {
        self.stop_worker();
    }
}

fn serve(
    listener: TcpListener,
    shared: Arc<Mutex<Shared>>,
    stop: Arc<AtomicBool>,
    slow: Arc<SlowGate>,
    workers: Arc<Mutex<Vec<JoinHandle<()>>>>,
    active: Arc<AtomicU64>,
) {
    let mut accepted = 0_usize;
    while !stop.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((stream, _)) => {
                if accepted >= MAX_TOTAL_CONNECTIONS {
                    lock(&shared).record_error("fixture total connection budget exceeded");
                    drop(stream);
                    continue;
                }
                accepted += 1;
                if active.load(Ordering::Acquire) >= MAX_CONNECTIONS as u64 {
                    lock(&shared).record_error("fixture connection capacity exceeded");
                    drop(stream);
                    continue;
                }
                active.fetch_add(1, Ordering::AcqRel);
                let connection_shared = Arc::clone(&shared);
                let stop = Arc::clone(&stop);
                let slow = Arc::clone(&slow);
                let active_worker = Arc::clone(&active);
                let result = thread::Builder::new()
                    .name("navigation-test-http-conn".to_string())
                    .spawn(move || {
                        handle_connection(stream, &connection_shared, &stop, &slow);
                        active_worker.fetch_sub(1, Ordering::AcqRel);
                    });
                match result {
                    Ok(worker) => lock(&workers).push(worker),
                    Err(_) => {
                        active.fetch_sub(1, Ordering::AcqRel);
                        lock(&shared).record_error("fixture worker spawn failed");
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(2));
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => {
                lock(&shared).record_error("fixture listener failed");
                break;
            }
        }
    }
}

struct RequestHead {
    method: String,
    path: String,
}

struct ReadError {
    message: Option<String>,
}

impl ReadError {
    fn silent() -> Self {
        Self { message: None }
    }

    fn bad(message: impl Into<String>) -> Self {
        Self {
            message: Some(message.into()),
        }
    }
}

fn expected_abort(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::NotConnected
    )
}

fn handle_connection(
    mut stream: TcpStream,
    shared: &Mutex<Shared>,
    stop: &AtomicBool,
    slow: &SlowGate,
) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(READ_POLL));
    let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
    let head = match read_request(&mut stream, stop) {
        Ok(head) => head,
        Err(error) => {
            if stop.load(Ordering::Acquire) {
                return;
            }
            if let Some(message) = error.message {
                lock(shared).record_error(message);
            }
            return;
        }
    };
    if stop.load(Ordering::Acquire) {
        return;
    }
    if head.method != "GET" {
        lock(shared).record_error("unsupported HTTP method");
        let _ = write_response(&mut stream, &Response::text(405, "method not allowed"));
        return;
    }
    let Some(route) = canonical_route(&head.path) else {
        lock(shared).record_error("unknown request path");
        let _ = write_response(&mut stream, &Response::text(404, "not found"));
        return;
    };
    lock(shared).count(route);
    if stop.load(Ordering::Acquire) {
        return;
    }
    let mut slow_released = None;
    let response = match route {
        PATH_A => Response::html(200, PAGE_A.as_bytes()),
        PATH_B => Response::html(200, PAGE_B.as_bytes()),
        PATH_WITH_CHILD => Response::html(200, PAGE_PARENT.as_bytes()),
        PATH_REDIRECT => Response::redirect(302, "/b"),
        PATH_FAVICON => Response::no_content(),
        PATH_MAIN_ERROR | PATH_CHILD_ERROR => {
            // Read and counted, then actively closed without any HTTP bytes.
            // Dropping the socket after an unread request body is the intended
            // source of a real network load error, not a protocol failure.
            let _ = stream.shutdown(Shutdown::Both);
            return;
        }
        PATH_SLOW => {
            slow.mark_seen();
            slow_released = Some(slow.wait(stop));
            if stop.load(Ordering::Acquire) {
                return;
            }
            Response::html(200, PAGE_SLOW.as_bytes())
        }
        _ => unreachable!("all fixed routes are handled"),
    };
    let result = write_response(&mut stream, &response);
    if let Some(released) = slow_released {
        slow.record_write(released, &result);
    }
    if let Err(error) = result {
        lock(shared).record_error(error);
    }
}

fn read_request(stream: &mut TcpStream, stop: &AtomicBool) -> Result<RequestHead, ReadError> {
    let deadline = Instant::now() + REQUEST_DEADLINE;
    let mut buffer = Vec::new();
    let header_end = loop {
        if stop.load(Ordering::Acquire) {
            return Err(ReadError::silent());
        }
        if let Some(position) = find_bytes(&buffer, b"\r\n\r\n") {
            break position + 4;
        }
        if buffer.len() > MAX_HEADER_BYTES {
            return Err(ReadError::bad("request headers too large"));
        }
        if Instant::now() >= deadline {
            if buffer.is_empty() {
                return Err(ReadError::silent());
            }
            return Err(ReadError::bad("request header timeout"));
        }
        let mut chunk = [0u8; 1024];
        match stream.read(&mut chunk) {
            Ok(0) if buffer.is_empty() => return Err(ReadError::silent()),
            Ok(0) => return Err(ReadError::bad("incomplete request headers")),
            Ok(count) => buffer.extend_from_slice(&chunk[..count]),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) if expected_abort(&error) => return Err(ReadError::silent()),
            Err(_) => return Err(ReadError::bad("request read failed")),
        }
    };
    if header_end > MAX_HEADER_BYTES {
        return Err(ReadError::bad("request headers too large"));
    }
    let header = std::str::from_utf8(&buffer[..header_end - 4])
        .map_err(|_| ReadError::bad("request headers are not UTF-8"))?;
    let mut lines = header.split("\r\n");
    let request_line = lines
        .next()
        .ok_or_else(|| ReadError::bad("missing request line"))?;
    let mut parts = request_line.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| ReadError::bad("missing request method"))?;
    let target = parts
        .next()
        .ok_or_else(|| ReadError::bad("missing request target"))?;
    let version = parts
        .next()
        .ok_or_else(|| ReadError::bad("missing HTTP version"))?;
    if parts.next().is_some() {
        return Err(ReadError::bad("malformed request line"));
    }
    if version != "HTTP/1.1" && version != "HTTP/1.0" {
        return Err(ReadError::bad("unsupported HTTP version"));
    }
    if !target.starts_with('/') {
        return Err(ReadError::bad("request target must be origin-form"));
    }
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if line.starts_with(' ') || line.starts_with('\t') {
            return Err(ReadError::bad("folded request headers are not supported"));
        }
        if !line.contains(':') {
            return Err(ReadError::bad("malformed request header"));
        }
    }
    // Only the normalized path leaves this function. Query and fragment are
    // intentionally discarded and never stored.
    let path = target.split(['?', '#']).next().unwrap_or("").to_string();
    if path.is_empty() {
        return Err(ReadError::bad("empty request path"));
    }
    Ok(RequestHead {
        method: method.to_string(),
        path,
    })
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

struct Response {
    status: u16,
    reason: &'static str,
    content_type: &'static str,
    body: &'static [u8],
    location: Option<&'static str>,
}

impl Response {
    fn html(status: u16, body: &'static [u8]) -> Self {
        Self {
            status,
            reason: "OK",
            content_type: "text/html; charset=utf-8",
            body,
            location: None,
        }
    }

    fn text(status: u16, body: &'static str) -> Self {
        Self {
            status,
            reason: reason_phrase(status),
            content_type: "text/plain; charset=utf-8",
            body: body.as_bytes(),
            location: None,
        }
    }

    fn redirect(status: u16, location: &'static str) -> Self {
        Self {
            status,
            reason: reason_phrase(status),
            content_type: "text/plain; charset=utf-8",
            body: b"",
            location: Some(location),
        }
    }

    fn no_content() -> Self {
        Self {
            status: 204,
            reason: "No Content",
            content_type: "text/plain; charset=utf-8",
            body: b"",
            location: None,
        }
    }
}

// true: the local socket accepted the writes; false: expected peer abort.
fn write_response(stream: &mut TcpStream, response: &Response) -> Result<bool, String> {
    let mut head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\nX-Content-Type-Options: nosniff\r\n",
        response.status,
        response.reason,
        response.content_type,
        response.body.len()
    );
    if let Some(location) = response.location {
        head.push_str("Location: ");
        head.push_str(location);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    let first = stream.write_all(head.as_bytes()).and_then(|()| {
        stream.write_all(response.body)?;
        stream.flush()
    });
    let result = match first {
        Ok(()) => Ok(true),
        Err(error) if expected_abort(&error) => Ok(false),
        Err(error) => Err(format!("response write failed: {:?}", error.kind())),
    };
    let _ = stream.shutdown(Shutdown::Write);
    result
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        204 => "No Content",
        302 => "Found",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        413 => "Payload Too Large",
        431 => "Request Header Fields Too Large",
        503 => "Service Unavailable",
        _ => "Error",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    fn address(fixture: &NavigationFixture) -> String {
        fixture.origin().trim_start_matches("http://").to_string()
    }

    fn raw_get(fixture: &NavigationFixture, target: &str) -> Vec<u8> {
        let mut stream = TcpStream::connect(address(fixture)).expect("connect to fixture");
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .expect("set read timeout");
        stream
            .write_all(
                format!(
                    "GET {target} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
                    address(fixture)
                )
                .as_bytes(),
            )
            .expect("write request");
        stream.flush().expect("flush request");
        let mut response = Vec::new();
        match stream.read_to_end(&mut response) {
            Ok(_) => {}
            Err(error) if expected_abort(&error) => {}
            Err(error) => panic!("read response: {error}"),
        }
        response
    }

    fn get(fixture: &NavigationFixture, target: &str) -> (String, Vec<u8>) {
        let response = raw_get(fixture, target);
        let split = response
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .expect("response has headers");
        (
            String::from_utf8(response[..split].to_vec()).expect("headers are UTF-8"),
            response[split + 4..].to_vec(),
        )
    }

    #[test]
    fn routes_serve_navigation_pages_without_storing_query_values() {
        let mut fixture = NavigationFixture::new().expect("start fixture");
        let (head, body) = get(&fixture, PATH_A);
        assert!(head.starts_with("HTTP/1.1 200"));
        assert!(head.contains("Cache-Control: no-store"));
        assert!(
            String::from_utf8(body)
                .unwrap()
                .contains("<title>Nav A</title>")
        );
        let (head_b, body_b) = get(&fixture, PATH_B);
        assert!(head_b.starts_with("HTTP/1.1 200"));
        assert!(
            String::from_utf8(body_b)
                .unwrap()
                .contains("<title>Nav B</title>")
        );
        let (head_parent, body_parent) = get(&fixture, PATH_WITH_CHILD);
        assert!(head_parent.starts_with("HTTP/1.1 200"));
        assert!(
            String::from_utf8(body_parent)
                .unwrap()
                .contains("src=\"/child-error\"")
        );
        let (head_redirect, _) = get(&fixture, PATH_REDIRECT);
        assert!(head_redirect.starts_with("HTTP/1.1 302"));
        assert!(head_redirect.contains("Location: /b"));
        let (head_favicon, _) = get(&fixture, PATH_FAVICON);
        assert!(head_favicon.starts_with("HTTP/1.1 204"));

        // A query is routed by path only and must not leak into counters or
        // errors.
        let (head_query, _) = get(&fixture, "/a?value=secret-query");
        assert!(head_query.starts_with("HTTP/1.1 200"));
        assert_eq!(fixture.request_count(PATH_A), Some(2));
        assert!(
            fixture
                .errors()
                .iter()
                .all(|error| !error.contains("secret-query"))
        );

        let (head_unknown, _) = get(&fixture, "/unknown");
        assert!(head_unknown.starts_with("HTTP/1.1 404"));
        assert!(
            fixture
                .errors()
                .iter()
                .any(|error| error.as_str() == "unknown request path")
        );
        assert!(
            fixture
                .finish()
                .iter()
                .all(|error| error.as_str() == "unknown request path")
        );
    }

    #[test]
    fn deliberate_error_routes_close_without_http_response() {
        let mut fixture = NavigationFixture::new().expect("start fixture");
        for (path, title) in [
            (PATH_MAIN_ERROR, "HTTP/1.1 404"),
            (PATH_CHILD_ERROR, "HTTP/1.1 404"),
        ] {
            let response = raw_get(&fixture, path);
            assert!(
                !String::from_utf8_lossy(&response).contains(title),
                "{path} must not fabricate an HTTP failure status"
            );
            assert!(
                !String::from_utf8_lossy(&response).contains("HTTP/1."),
                "{path} must close without HTTP headers"
            );
            assert_eq!(fixture.request_count(path), Some(1));
        }
        assert!(fixture.finish().is_empty());
    }

    #[test]
    fn slow_route_waits_for_release_and_finish_joins_bounded_workers() {
        let mut fixture = NavigationFixture::new().expect("start fixture");
        let mut stream = TcpStream::connect(address(&fixture)).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .expect("set read timeout");
        stream
            .write_all(
                format!(
                    "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
                    PATH_SLOW,
                    address(&fixture)
                )
                .as_bytes(),
            )
            .expect("write request");
        stream.flush().expect("flush request");
        let deadline = Instant::now() + Duration::from_secs(3);
        while fixture.slow_request_count() == 0 {
            assert!(Instant::now() < deadline, "slow request was not observed");
            thread::sleep(Duration::from_millis(5));
        }
        let mut peek = [0u8; 1];
        stream
            .set_read_timeout(Some(Duration::from_millis(100)))
            .expect("set short read timeout");
        assert!(
            matches!(
                stream.read(&mut peek),
                Err(ref error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    )
            ),
            "slow route sent headers before release"
        );
        fixture.release_slow();
        let mut response = Vec::new();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .expect("restore read timeout");
        stream
            .read_to_end(&mut response)
            .expect("read slow response");
        assert!(String::from_utf8_lossy(&response).contains("<title>Nav Slow</title>"));
        assert_eq!(fixture.request_count(PATH_SLOW), Some(1));
        assert!(fixture.finish().is_empty());
    }
}
