//! Test-only same-origin HTTP fixture for browser/profile storage checks.
//!
//! The fixture serves one real HTML page and receives a same-origin `fetch`
//! result from that page. Its socket tests prove the HTTP protocol only; they
//! are not CEF evidence. Actual cookie/localStorage isolation needs a real
//! Browser loading the fixture URL.

use std::{
    collections::{BTreeMap, HashMap},
    fmt,
    io::{Read, Write},
    net::{Shutdown, TcpListener, TcpStream},
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use serde::Deserialize;

const PAGE_PATH: &str = "/profile-test";
const RESULT_PATH: &str = "/result";
const MAX_TOKENS: usize = 256;
const MAX_HEADER_BYTES: usize = 16 * 1024;
const MAX_BODY_BYTES: usize = 16 * 1024;
const MAX_ERRORS: usize = 64;
const MAX_TOKEN_BYTES: usize = 128;
const MAX_VALUE_BYTES: usize = 1024;
const READ_POLL: Duration = Duration::from_millis(50);
const REQUEST_DEADLINE: Duration = Duration::from_millis(250);
const WRITE_TIMEOUT: Duration = Duration::from_millis(100);

#[cfg(test)]
const COOKIE_NAME: &str = "gpui_cef_profile_test";
#[cfg(test)]
const STORAGE_KEY: &str = "gpui-cef-profile-test";

const PAGE_HTML: &str = include_str!("../../../assets/profile-test.html");

/// One page report. Cookie/localStorage values are intentionally hidden from
/// `Debug`; exact equality is still available for assertions.
#[derive(Clone, PartialEq, Eq)]
pub struct Observation {
    pub token: String,
    pub cookie: Option<String>,
    pub local_storage: Option<String>,
}

impl fmt::Debug for Observation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Observation")
            .field("token", &self.token)
            .field("cookie", &redacted(&self.cookie))
            .field("local_storage", &redacted(&self.local_storage))
            .finish()
    }
}

fn redacted(value: &Option<String>) -> &'static str {
    if value.is_some() {
        "<redacted>"
    } else {
        "<none>"
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Operation {
    Write,
    Read,
}

impl Operation {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "write" => Some(Self::Write),
            "read" => Some(Self::Read),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Write => "write",
            Self::Read => "read",
        }
    }
}

struct Entry {
    operation: Operation,
    served: bool,
    result: Option<Observation>,
    result_received: bool,
}

#[derive(Default)]
struct Shared {
    entries: HashMap<String, Entry>,
    errors: Vec<String>,
}

impl Shared {
    fn record_error(&mut self, message: String) {
        if self.errors.len() < MAX_ERRORS {
            self.errors.push(message);
        } else if self.errors.last().map(String::as_str) != Some("additional errors omitted") {
            self.errors.push("additional errors omitted".to_string());
        }
    }
}

fn lock(shared: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
    shared
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A loopback HTTP server with one dynamic origin. Every URL generation
/// registers a token exactly once; the page result must come back with that
/// same token.
pub struct ProfileFixture {
    origin: String,
    shared: Arc<Mutex<Shared>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl ProfileFixture {
    pub fn new() -> std::io::Result<Self> {
        Self::on_port(0)
    }

    /// Bind a caller-chosen loopback port. Reusing a port keeps the same origin
    /// across process restarts, which localStorage scopes to host and port.
    pub fn on_port(port: u16) -> std::io::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", port))?;
        listener.set_nonblocking(true)?;
        let origin = format!("http://{}", listener.local_addr()?);
        let shared = Arc::new(Mutex::new(Shared::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let worker_shared = Arc::clone(&shared);
        let worker_stop = Arc::clone(&stop);
        let worker = thread::Builder::new()
            .name("profile-test-http".to_string())
            .spawn(move || serve(listener, worker_shared, worker_stop))?;
        Ok(Self {
            origin,
            shared,
            stop,
            worker: Some(worker),
        })
    }

    pub fn origin(&self) -> &str {
        &self.origin
    }

    /// Register a single-use token and build a same-origin page URL. For
    /// `read`, `value` is ignored by the page and no storage is written.
    pub fn url(&self, token: &str, operation: &str, value: &str) -> Result<String, String> {
        if !valid_token(token) {
            return Err(format!(
                "token must be 1..={MAX_TOKEN_BYTES} characters from [A-Za-z0-9._:-]"
            ));
        }
        if value.len() > MAX_VALUE_BYTES {
            return Err(format!("value exceeds {MAX_VALUE_BYTES} bytes"));
        }
        let operation = Operation::parse(operation)
            .ok_or_else(|| "operation must be write or read".to_string())?;
        {
            let mut shared = lock(&self.shared);
            if shared.entries.contains_key(token) {
                let message = format!("token already registered: {token}");
                shared.record_error(message.clone());
                return Err(message);
            }
            if shared.entries.len() >= MAX_TOKENS {
                let message = format!("token limit reached ({MAX_TOKENS})");
                shared.record_error(message.clone());
                return Err(message);
            }
            shared.entries.insert(
                token.to_string(),
                Entry {
                    operation,
                    served: false,
                    result: None,
                    result_received: false,
                },
            );
        }
        Ok(format!(
            "{}{}?token={}&op={}&value={}",
            self.origin,
            PAGE_PATH,
            percent_encode(token),
            operation.as_str(),
            percent_encode(value)
        ))
    }

    /// Take the single result for `token`. A repeated call returns `None`, but
    /// the token stays known so a duplicate page POST is still rejected.
    pub fn take_result(&self, token: &str) -> Option<Observation> {
        lock(&self.shared)
            .entries
            .get_mut(token)
            .and_then(|entry| entry.result.take())
    }

    /// Protocol errors, with tokens but never cookie/localStorage values.
    pub fn errors(&self) -> Vec<String> {
        lock(&self.shared).errors.clone()
    }

    /// Stop and join the HTTP worker before taking the final error snapshot.
    /// Call after Browser shutdown: no worker can append a late error afterwards.
    pub fn finish(&mut self) -> Vec<String> {
        self.stop_worker();
        self.errors()
    }

    fn stop_worker(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for ProfileFixture {
    fn drop(&mut self) {
        self.stop_worker();
    }
}

fn valid_token(token: &str) -> bool {
    !token.is_empty()
        && token.len() <= MAX_TOKEN_BYTES
        && token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
}

fn percent_encode(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    for byte in input.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            output.push(byte as char);
        } else {
            output.push_str(&format!("%{byte:02X}"));
        }
    }
    output
}

fn percent_decode(input: &str) -> Result<String, String> {
    let bytes = input.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' => {
                if index + 2 >= bytes.len() {
                    return Err("truncated percent escape".to_string());
                }
                let high = hex_digit(bytes[index + 1])?;
                let low = hex_digit(bytes[index + 2])?;
                output.push((high << 4) | low);
                index += 3;
            }
            b'+' => {
                output.push(b' ');
                index += 1;
            }
            byte => {
                output.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8(output).map_err(|_| "query value is not UTF-8".to_string())
}

fn hex_digit(byte: u8) -> Result<u8, String> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err("invalid percent escape".to_string()),
    }
}

fn serve(listener: TcpListener, shared: Arc<Mutex<Shared>>, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((stream, _)) => handle_connection(stream, &shared, &stop),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(2));
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
}

fn handle_connection(mut stream: TcpStream, shared: &Mutex<Shared>, stop: &AtomicBool) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(READ_POLL));
    let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
    let deadline = Instant::now() + REQUEST_DEADLINE;
    let response = match read_request(&mut stream, deadline, stop) {
        Ok(request) => match route(request, shared) {
            Ok(response) => response,
            Err(error) => {
                if stop.load(Ordering::Acquire) {
                    return;
                }
                lock(shared).record_error(error.message.clone());
                Response::text(error.status, &error.message)
            }
        },
        Err(error) => {
            // A speculative browser preconnect sends nothing and is not a
            // protocol error.
            if stop.load(Ordering::Acquire) || error.silent {
                return;
            }
            lock(shared).record_error(error.message.clone());
            Response::text(error.status, &error.message)
        }
    };
    let _ = write_response(&mut stream, &response);
    let _ = stream.shutdown(Shutdown::Write);
}

struct Request {
    method: Method,
    target: String,
    content_type: Option<String>,
    body: Vec<u8>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Method {
    Get,
    Post,
}

struct HttpError {
    status: u16,
    message: String,
    silent: bool,
}

impl HttpError {
    fn new(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            silent: false,
        }
    }

    fn silent(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            silent: true,
        }
    }

    fn bad(message: impl Into<String>) -> Self {
        Self::new(400, message)
    }
}

fn read_request(
    stream: &mut TcpStream,
    deadline: Instant,
    stop: &AtomicBool,
) -> Result<Request, HttpError> {
    let mut buffer = Vec::new();
    let header_end = loop {
        if stop.load(Ordering::Acquire) {
            return Err(HttpError::new(503, "fixture is stopping"));
        }
        if let Some(position) = find_bytes(&buffer, b"\r\n\r\n") {
            break position + 4;
        }
        if buffer.len() > MAX_HEADER_BYTES {
            return Err(HttpError::new(431, "request headers too large"));
        }
        if Instant::now() >= deadline {
            return Err(HttpError::new(408, "request timeout"));
        }
        let mut chunk = [0u8; 1024];
        match stream.read(&mut chunk) {
            Ok(0) if buffer.is_empty() => {
                return Err(HttpError::silent(
                    408,
                    "connection closed without a request",
                ));
            }
            Ok(0) => return Err(HttpError::bad("incomplete request headers")),
            Ok(count) => buffer.extend_from_slice(&chunk[..count]),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                if buffer.is_empty() && Instant::now() >= deadline {
                    return Err(HttpError::silent(408, "idle connection"));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return Err(HttpError::bad("request read failed")),
        }
    };
    if header_end > MAX_HEADER_BYTES {
        return Err(HttpError::new(431, "request headers too large"));
    }
    let header = std::str::from_utf8(&buffer[..header_end - 4])
        .map_err(|_| HttpError::bad("request headers are not UTF-8"))?;
    let mut lines = header.split("\r\n");
    let request_line = lines
        .next()
        .ok_or_else(|| HttpError::bad("missing request line"))?;
    let mut parts = request_line.split_whitespace();
    let method_text = parts
        .next()
        .ok_or_else(|| HttpError::bad("missing request method"))?;
    let target = parts
        .next()
        .ok_or_else(|| HttpError::bad("missing request target"))?;
    let version = parts
        .next()
        .ok_or_else(|| HttpError::bad("missing HTTP version"))?;
    if parts.next().is_some() {
        return Err(HttpError::bad("malformed request line"));
    }
    if version != "HTTP/1.1" && version != "HTTP/1.0" {
        return Err(HttpError::bad("unsupported HTTP version"));
    }
    if !target.starts_with('/') {
        return Err(HttpError::bad("request target must be origin-form"));
    }
    let method = match method_text {
        "GET" => Method::Get,
        "POST" => Method::Post,
        _ => return Err(HttpError::new(405, "unsupported HTTP method")),
    };

    let mut content_length: Option<usize> = None;
    let mut content_type: Option<String> = None;
    let mut transfer_encoding = false;
    let mut expect_continue = false;
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if line.starts_with(' ') || line.starts_with('\t') {
            return Err(HttpError::bad("folded request headers are not supported"));
        }
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| HttpError::bad("malformed request header"))?;
        let name = name.to_ascii_lowercase();
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            return Err(HttpError::bad("invalid request header name"));
        }
        let value = value.trim();
        match name.as_str() {
            "content-length" => {
                if content_length.is_some() {
                    return Err(HttpError::bad("duplicate content-length"));
                }
                if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                    return Err(HttpError::bad("invalid content-length"));
                }
                let length = value
                    .parse::<usize>()
                    .map_err(|_| HttpError::bad("invalid content-length"))?;
                content_length = Some(length);
            }
            "transfer-encoding" => transfer_encoding = true,
            "expect" => {
                if !value.eq_ignore_ascii_case("100-continue") {
                    return Err(HttpError::new(417, "unsupported expect header"));
                }
                expect_continue = true;
            }
            "content-type" => {
                if content_type.is_some() {
                    return Err(HttpError::bad("duplicate content-type"));
                }
                content_type = Some(value.to_ascii_lowercase());
            }
            // Header values such as Cookie are intentionally not stored,
            // logged, or echoed.
            _ => {}
        }
    }
    if transfer_encoding {
        return Err(HttpError::bad(
            "transfer-encoding/chunked requests are not supported",
        ));
    }

    let body_length = match method {
        Method::Get => {
            if content_length.unwrap_or(0) != 0 {
                return Err(HttpError::bad("GET with a request body is not supported"));
            }
            0
        }
        Method::Post => {
            let length =
                content_length.ok_or_else(|| HttpError::new(411, "content-length is required"))?;
            if length > MAX_BODY_BYTES {
                return Err(HttpError::new(413, "request body too large"));
            }
            length
        }
    };
    let mut body = buffer[header_end..].to_vec();
    if body.len() > body_length {
        return Err(HttpError::bad("unexpected bytes after request body"));
    }
    if expect_continue && body_length > 0 && body.is_empty() {
        stream
            .write_all(b"HTTP/1.1 100 Continue\r\n\r\n")
            .and_then(|()| stream.flush())
            .map_err(|_| HttpError::new(408, "failed to acknowledge request body"))?;
    }
    while body.len() < body_length {
        if stop.load(Ordering::Acquire) {
            return Err(HttpError::new(503, "fixture is stopping"));
        }
        if Instant::now() >= deadline {
            return Err(HttpError::new(408, "request timeout"));
        }
        let mut chunk = [0u8; 1024];
        match stream.read(&mut chunk) {
            Ok(0) => return Err(HttpError::bad("incomplete request body")),
            Ok(count) => {
                body.extend_from_slice(&chunk[..count]);
                if body.len() > body_length {
                    return Err(HttpError::bad("request body exceeds content-length"));
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return Err(HttpError::bad("request read failed")),
        }
    }
    Ok(Request {
        method,
        target: target.to_string(),
        content_type,
        body,
    })
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn route(request: Request, shared: &Mutex<Shared>) -> Result<Response, HttpError> {
    match request.method {
        Method::Get => route_page(&request.target, shared),
        Method::Post => route_result(
            &request.target,
            request.content_type.as_deref(),
            &request.body,
            shared,
        ),
    }
}

fn route_page(target: &str, shared: &Mutex<Shared>) -> Result<Response, HttpError> {
    let (path, query) = split_target(target)?;
    if path == RESULT_PATH {
        return Err(HttpError::new(405, "GET /result is not supported"));
    }
    // Browser chrome may request the favicon on its own. It is not part of the
    // probe protocol and must not be recorded as a protocol error.
    if path == "/favicon.ico" && query.is_empty() {
        return Ok(Response::no_content());
    }
    if path != PAGE_PATH {
        return Err(HttpError::new(404, "unknown path"));
    }
    let params = parse_query(query).map_err(|_| HttpError::bad("invalid page query"))?;
    if params.len() != 3 {
        return Err(HttpError::bad(
            "page query must contain token, op, and value",
        ));
    }
    let token = params
        .get("token")
        .ok_or_else(|| HttpError::bad("missing token"))?;
    if !valid_token(token) {
        return Err(HttpError::bad("invalid token parameter"));
    }
    let operation = Operation::parse(
        params
            .get("op")
            .map(String::as_str)
            .ok_or_else(|| HttpError::bad("missing op"))?,
    )
    .ok_or_else(|| HttpError::bad("op must be write or read"))?;
    let value = params
        .get("value")
        .ok_or_else(|| HttpError::bad("missing value"))?;
    if value.len() > MAX_VALUE_BYTES {
        return Err(HttpError::bad("value parameter too large"));
    }

    enum Check {
        Ready,
        Unknown,
        Mismatch,
        AlreadyServed,
    }
    let check = {
        let mut shared = lock(shared);
        match shared.entries.get_mut(token) {
            None => Check::Unknown,
            Some(entry) if entry.operation != operation => Check::Mismatch,
            Some(entry) if entry.served => Check::AlreadyServed,
            Some(entry) => {
                entry.served = true;
                Check::Ready
            }
        }
    };
    match check {
        Check::Ready => Ok(Response::html(PAGE_HTML)),
        Check::Unknown => Err(HttpError::new(404, format!("unknown token: {token}"))),
        Check::Mismatch => Err(HttpError::new(
            400,
            format!("operation does not match registered token: {token}"),
        )),
        Check::AlreadyServed => Err(HttpError::new(
            409,
            format!("token already served: {token}"),
        )),
    }
}

fn split_target(target: &str) -> Result<(&str, &str), HttpError> {
    let mut parts = target.splitn(2, '?');
    let path = parts.next().unwrap_or("");
    let query = parts.next().unwrap_or("");
    if path.is_empty() || !path.starts_with('/') {
        return Err(HttpError::bad("invalid request path"));
    }
    Ok((path, query))
}

fn parse_query(query: &str) -> Result<BTreeMap<String, String>, ()> {
    if query.is_empty() {
        return Err(());
    }
    let mut params = BTreeMap::new();
    for pair in query.split('&') {
        if pair.is_empty() {
            return Err(());
        }
        let (raw_key, raw_value) = pair.split_once('=').ok_or(())?;
        if raw_key.is_empty() {
            return Err(());
        }
        let key = percent_decode(raw_key).map_err(|_| ())?;
        let value = percent_decode(raw_value).map_err(|_| ())?;
        if params.insert(key, value).is_some() {
            return Err(());
        }
    }
    Ok(params)
}

#[derive(Deserialize)]
#[serde(untagged)]
enum PagePayload {
    Failure(PageFailure),
    Observation(PageObservation),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PageFailure {
    token: String,
    error: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PageObservation {
    token: String,
    // Both fields are required by the page protocol. An explicit JSON null is
    // a real observation ("no cookie/localStorage"), while a missing field is
    // a malformed report and must not be silently read as null.
    #[serde(deserialize_with = "nullable_string")]
    cookie: Option<String>,
    #[serde(deserialize_with = "nullable_string")]
    local_storage: Option<String>,
}

fn nullable_string<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<String>::deserialize(deserializer)
}

enum PageOutcome {
    Values {
        cookie: Option<String>,
        local_storage: Option<String>,
    },
    Error,
}

fn route_result(
    target: &str,
    content_type: Option<&str>,
    body: &[u8],
    shared: &Mutex<Shared>,
) -> Result<Response, HttpError> {
    if target != RESULT_PATH {
        return Err(HttpError::new(404, "POST path must be /result"));
    }
    let is_json = content_type.is_some_and(|value| {
        value
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .eq_ignore_ascii_case("application/json")
    });
    if !is_json {
        return Err(HttpError::new(
            415,
            "POST /result requires application/json",
        ));
    }
    let payload: PagePayload =
        serde_json::from_slice(body).map_err(|_| HttpError::bad("invalid result JSON"))?;
    let (token, outcome) = match payload {
        PagePayload::Failure(failure) => {
            let _ = failure.error;
            (failure.token, PageOutcome::Error)
        }
        PagePayload::Observation(observation) => (
            observation.token,
            PageOutcome::Values {
                cookie: observation.cookie,
                local_storage: observation.local_storage,
            },
        ),
    };
    if !valid_token(&token) {
        return Err(HttpError::bad("invalid result token"));
    }

    enum Submit {
        Accepted,
        Unknown,
        NotServed,
        Duplicate,
    }
    let submit = {
        let mut shared = lock(shared);
        match shared.entries.get_mut(&token) {
            None => Submit::Unknown,
            Some(entry) if !entry.served => Submit::NotServed,
            Some(entry) if entry.result_received => Submit::Duplicate,
            Some(entry) => {
                entry.result_received = true;
                if let PageOutcome::Values {
                    cookie,
                    local_storage,
                } = &outcome
                {
                    entry.result = Some(Observation {
                        token: token.clone(),
                        cookie: cookie.clone(),
                        local_storage: local_storage.clone(),
                    });
                }
                Submit::Accepted
            }
        }
    };
    match submit {
        Submit::Accepted => match outcome {
            PageOutcome::Values { .. } => Ok(Response::text(200, "ok")),
            PageOutcome::Error => Err(HttpError::new(
                422,
                format!("page reported an error for token {token}"),
            )),
        },
        Submit::Unknown => Err(HttpError::new(
            404,
            format!("unknown result token: {token}"),
        )),
        Submit::NotServed => Err(HttpError::new(
            409,
            format!("result before page GET for token: {token}"),
        )),
        Submit::Duplicate => Err(HttpError::new(
            409,
            format!("duplicate result for token: {token}"),
        )),
    }
}

struct Response {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
}

impl Response {
    fn html(body: &'static str) -> Self {
        Self {
            status: 200,
            content_type: "text/html; charset=utf-8",
            body: body.as_bytes().to_vec(),
        }
    }

    fn text(status: u16, body: impl AsRef<[u8]>) -> Self {
        Self {
            status,
            content_type: "text/plain; charset=utf-8",
            body: body.as_ref().to_vec(),
        }
    }

    fn no_content() -> Self {
        Self {
            status: 204,
            content_type: "text/plain; charset=utf-8",
            body: Vec::new(),
        }
    }
}

fn write_response(stream: &mut TcpStream, response: &Response) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\nX-Content-Type-Options: nosniff\r\n\r\n",
        response.status,
        reason_phrase(response.status),
        response.content_type,
        response.body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(&response.body)?;
    stream.flush()
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        409 => "Conflict",
        411 => "Length Required",
        413 => "Payload Too Large",
        415 => "Unsupported Media Type",
        417 => "Expectation Failed",
        422 => "Unprocessable Entity",
        431 => "Request Header Fields Too Large",
        503 => "Service Unavailable",
        _ => "Error",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::{Shutdown, TcpStream},
        thread,
        time::{Duration, Instant},
    };

    // These tests drive the fixture with real loopback sockets. They do not
    // execute the page JS and are not CEF/browser evidence.

    fn address(fixture: &ProfileFixture) -> String {
        fixture.origin().trim_start_matches("http://").to_string()
    }

    fn target(url: &str) -> &str {
        let after_scheme = url
            .split_once("://")
            .map(|(_, rest)| rest)
            .expect("fixture URL has a scheme");
        let slash = after_scheme.find('/').expect("fixture URL has a path");
        &after_scheme[slash..]
    }

    fn send(fixture: &ProfileFixture, head: &str, body: &[u8]) -> (u16, String, String) {
        let mut stream = TcpStream::connect(address(fixture)).expect("connect to fixture");
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("set read timeout");
        stream
            .set_write_timeout(Some(Duration::from_secs(2)))
            .expect("set write timeout");
        stream
            .write_all(head.as_bytes())
            .expect("write request head");
        stream.write_all(body).expect("write request body");
        stream.flush().expect("flush request");
        let _ = stream.shutdown(Shutdown::Write);
        read_response(&mut stream)
    }

    fn read_response(stream: &mut TcpStream) -> (u16, String, String) {
        let mut raw = Vec::new();
        stream
            .read_to_end(&mut raw)
            .expect("read complete response");
        let split = raw
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .expect("response has header terminator");
        let head = std::str::from_utf8(&raw[..split]).expect("response headers are UTF-8");
        let status = head
            .lines()
            .next()
            .and_then(|line| line.split(' ').nth(1))
            .and_then(|value| value.parse::<u16>().ok())
            .expect("response status");
        let headers = head.to_string();
        let body = String::from_utf8(raw[split + 4..].to_vec()).expect("response body is UTF-8");
        (status, headers, body)
    }

    fn get(fixture: &ProfileFixture, url: &str) -> (u16, String, String) {
        let head = format!(
            "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
            target(url),
            address(fixture)
        );
        send(fixture, &head, b"")
    }

    #[test]
    fn cookie_request_header_is_neither_recorded_nor_echoed() {
        let fixture = ProfileFixture::new().expect("start fixture");
        let url = fixture
            .url("cookie-header-read", "read", "")
            .expect("register URL");
        let marker = "header-cookie-marker";
        let head = format!(
            "GET {} HTTP/1.1\r\nHost: {}\r\nCookie: gpui_cef_profile_test={marker}\r\nConnection: close\r\n\r\n",
            target(&url),
            address(&fixture)
        );
        let (status, headers, page) = send(&fixture, &head, b"");
        assert_eq!(status, 200);
        assert!(
            !headers.contains(marker),
            "response echoed the Cookie header"
        );
        assert!(
            !page.contains(marker),
            "page body contains the Cookie header"
        );
        assert!(
            fixture.errors().iter().all(|error| !error.contains(marker)),
            "errors recorded the Cookie header"
        );
    }

    fn post_json(fixture: &ProfileFixture, body: &str) -> (u16, String, String) {
        let head = format!(
            "POST /result HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
            address(fixture),
            body.len()
        );
        send(fixture, &head, body.as_bytes())
    }

    #[test]
    fn percent_encoding_round_trips_query_reserved_characters() {
        let raw = "value 42 & x=y+z%/é";
        let encoded = percent_encode(raw);
        assert!(
            encoded.bytes().all(|byte| byte.is_ascii_alphanumeric()
                || matches!(byte, b'-' | b'_' | b'.' | b'~' | b'%')),
            "encoding left a raw reserved character"
        );
        assert_eq!(percent_decode(&encoded).expect("decode"), raw);
        assert_eq!(
            percent_decode("token=a%2Fb%3Dc").expect("decode literal escapes"),
            "token=a/b=c"
        );
        assert!(percent_decode("%zz").is_err());
    }

    #[test]
    fn page_keeps_the_center_pixel_on_the_probe_background() {
        // The G1 probe validates rendering by the center BGRA pixel
        // [126, 76, 26, 255]. Text must stay in the top strip so the 320x240
        // viewport center remains the solid RGB(26,76,126) background.
        assert!(PAGE_HTML.contains("background: rgb(26, 76, 126)"));
        assert!(PAGE_HTML.contains("margin:0;background:rgb(26,76,126)"));
        assert!(PAGE_HTML.contains("html, body { margin: 0"));
        let panel = PAGE_HTML
            .find("#status-panel {")
            .expect("page lacks the status panel style");
        let limit = PAGE_HTML[panel..]
            .find("max-height: 72px")
            .expect("status panel has no height limit");
        assert!(limit > 0);
        assert!(
            PAGE_HTML.contains("left: 8px; top: 0;") && PAGE_HTML.contains("width: 304px"),
            "status panel is not confined to the top strip"
        );
    }

    #[test]
    fn fixed_port_reuses_the_same_origin_for_restart_checks() {
        let first = ProfileFixture::new().expect("start fixture");
        let port = first
            .origin()
            .rsplit(':')
            .next()
            .expect("origin has a port")
            .parse::<u16>()
            .expect("origin port is a number");
        // Exercise a completed request first: the listener must still be
        // rebindable despite the closed connection's socket state.
        let url = first
            .url("restart-origin", "read", "")
            .expect("register URL");
        assert_eq!(get(&first, &url).0, 200);
        drop(first);
        let deadline = Instant::now() + Duration::from_millis(500);
        let second = loop {
            match ProfileFixture::on_port(port) {
                Ok(fixture) => break fixture,
                Err(_) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
                Err(error) => panic!("rebind same port failed: {error}"),
            }
        };
        assert_eq!(
            second.origin(),
            format!("http://127.0.0.1:{port}"),
            "rebound origin differs"
        );
    }

    #[test]
    fn split_page_get_and_result_post_produce_one_exact_observation() {
        let fixture = ProfileFixture::new().expect("start fixture");
        let token = "default-A-phase1-write";
        let url = fixture
            .url(token, "write", "value-42")
            .expect("register URL");
        let mut stream = TcpStream::connect(address(&fixture)).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("set read timeout");
        let request = format!(
            "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
            target(&url),
            address(&fixture)
        );
        let split = request.len() / 2;
        stream
            .write_all(&request.as_bytes()[..split])
            .expect("write first header half");
        stream.flush().expect("flush first header half");
        thread::sleep(Duration::from_millis(10));
        stream
            .write_all(&request.as_bytes()[split..])
            .expect("write second header half");
        stream.flush().expect("flush second header half");
        let (status, headers, page) = read_response(&mut stream);
        assert_eq!(status, 200);
        assert!(
            page.contains(COOKIE_NAME),
            "page does not mention the cookie"
        );
        assert!(
            page.contains(STORAGE_KEY),
            "page does not mention storage key"
        );
        assert!(
            headers.contains("Cache-Control: no-store"),
            "missing no-store header in page response"
        );
        assert_eq!(fixture.take_result(token), None);

        let result =
            format!(r#"{{"token":"{token}","cookie":"value-42","local_storage":"value-42"}}"#);
        let mut stream = TcpStream::connect(address(&fixture)).expect("connect for result");
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("set read timeout");
        let head = format!(
            "POST /result HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
            address(&fixture),
            result.len()
        );
        stream
            .write_all(head.as_bytes())
            .expect("write result head");
        stream.flush().expect("flush result head");
        let split = result.len() / 2;
        stream
            .write_all(&result.as_bytes()[..split])
            .expect("write first body half");
        stream.flush().expect("flush first body half");
        thread::sleep(Duration::from_millis(10));
        stream
            .write_all(&result.as_bytes()[split..])
            .expect("write second body half");
        stream.flush().expect("flush second body half");
        let (status, _, _) = read_response(&mut stream);
        assert_eq!(status, 200);
        let observation = fixture.take_result(token).expect("one result");
        assert_eq!(observation.token, token);
        assert!(
            observation.cookie.as_deref() == Some("value-42"),
            "cookie value mismatch"
        );
        assert!(
            observation.local_storage.as_deref() == Some("value-42"),
            "localStorage value mismatch"
        );
        assert_eq!(fixture.take_result(token), None);
        assert!(fixture.errors().is_empty(), "unexpected fixture errors");
    }

    #[test]
    fn duplicate_registration_duplicate_post_and_unknown_token_are_rejected() {
        let fixture = ProfileFixture::new().expect("start fixture");
        let token = "P1-A-write";
        fixture
            .url(token, "write", "cookie-secret")
            .expect("register URL");
        assert!(
            fixture.url(token, "write", "other").is_err(),
            "duplicate URL registration was accepted"
        );
        let unknown_get =
            fixture.origin().to_string() + "/profile-test?token=not-registered&op=read&value=";
        assert_eq!(
            get(&fixture, &unknown_get).0,
            404,
            "unknown page token was served"
        );

        let url = fixture
            .url("P1-A-second-write", "write", "ignored")
            .expect("register second URL");
        let (status, _, _) = get(&fixture, &url);
        assert_eq!(status, 200);
        assert_eq!(
            get(&fixture, &url).0,
            409,
            "a second page GET for one token was served"
        );

        let first = format!(
            r#"{{"token":"P1-A-second-write","cookie":"cookie-secret","local_storage":"local-secret"}}"#
        );
        assert_eq!(post_json(&fixture, &first).0, 200);
        assert_eq!(
            post_json(&fixture, &first).0,
            409,
            "duplicate POST was accepted"
        );
        let observation = fixture
            .take_result("P1-A-second-write")
            .expect("result still available");
        assert!(
            observation.cookie.as_deref() == Some("cookie-secret"),
            "cookie value mismatch"
        );
        assert!(
            observation.local_storage.as_deref() == Some("local-secret"),
            "localStorage value mismatch"
        );
        let debug = format!("{observation:?}");
        assert!(
            !debug.contains("cookie-secret") && !debug.contains("local-secret"),
            "Debug leaked a stored value"
        );
        assert!(debug.contains("<redacted>"), "Debug did not mark values");

        // A second token keeps its own values: results never alias across
        // tokens even though they share one fixture.
        let second_url = fixture
            .url("P2-write", "write", "second-secret")
            .expect("register P2 URL");
        assert_eq!(get(&fixture, &second_url).0, 200);
        let second =
            r#"{"token":"P2-write","cookie":"second-cookie","local_storage":"second-local"}"#;
        assert_eq!(post_json(&fixture, second).0, 200);
        let second_observation = fixture.take_result("P2-write").expect("P2 result");
        assert_eq!(second_observation.token, "P2-write");
        assert!(second_observation.cookie.as_deref() == Some("second-cookie"));
        assert!(second_observation.local_storage.as_deref() == Some("second-local"));
        assert!(
            second_observation.cookie.as_deref() != Some("cookie-secret"),
            "P2 observed the P1 cookie"
        );
        assert!(
            second_observation.local_storage.as_deref() != Some("local-secret"),
            "P2 observed the P1 localStorage"
        );

        let unknown = r#"{"token":"not-registered","cookie":null,"local_storage":null}"#;
        assert_eq!(
            post_json(&fixture, unknown).0,
            404,
            "unknown token POST was accepted"
        );
        let errors = fixture.errors();
        assert!(
            errors
                .iter()
                .any(|error| error.contains("unknown result token")),
            "unknown token was not reported"
        );
        assert!(
            errors.iter().all(|error| !error.contains("cookie-secret")),
            "fixture error leaked a cookie value"
        );
    }

    #[test]
    fn oversized_body_chunked_transfer_and_unknown_method_are_rejected() {
        let fixture = ProfileFixture::new().expect("start fixture");
        let url = fixture
            .url("limits-read", "read", "")
            .expect("register URL");
        assert_eq!(get(&fixture, &url).0, 200);

        let oversized = format!(
            "POST /result HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
            address(&fixture),
            MAX_BODY_BYTES + 1
        );
        assert_eq!(send(&fixture, &oversized, b"").0, 413);

        let chunked = format!(
            "POST /result HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
            address(&fixture)
        );
        assert_eq!(send(&fixture, &chunked, b"").0, 400);

        let method = format!(
            "PUT /profile-test HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
            address(&fixture)
        );
        assert_eq!(send(&fixture, &method, b"").0, 405);

        let errors = fixture.errors();
        assert!(
            errors.iter().any(|error| error.contains("too large")),
            "oversized body was not reported"
        );
        assert!(
            errors
                .iter()
                .any(|error| error.contains("transfer-encoding")),
            "chunked request was not reported"
        );
        assert!(
            errors
                .iter()
                .any(|error| error.contains("unsupported HTTP method")),
            "unknown method was not reported"
        );
    }

    #[test]
    fn read_operation_serves_a_page_without_storage_writes() {
        let fixture = ProfileFixture::new().expect("start fixture");
        let token = "default-B-read";
        let url = fixture
            .url(token, "read", "must-not-overwrite")
            .expect("register read URL");
        assert!(url.contains("op=read"), "read URL does not carry op=read");
        let (status, _, page) = get(&fixture, &url);
        assert_eq!(status, 200);

        let condition = page
            .find("if (operation === \"write\")")
            .expect("page lacks the write branch");
        let cookie_write = page
            .find("document.cookie =")
            .expect("page lacks cookie assignment");
        let storage_write = page
            .find("localStorage.setItem")
            .expect("page lacks localStorage assignment");
        let read_branch = page
            .find("} else if (operation !== \"read\")")
            .expect("page lacks the read branch");
        assert!(
            condition < cookie_write && cookie_write < read_branch,
            "cookie assignment is not guarded by the write branch"
        );
        assert!(
            condition < storage_write && storage_write < read_branch,
            "localStorage assignment is not guarded by the write branch"
        );

        let result = r#"{"token":"default-B-read","cookie":"existing-cookie","local_storage":"existing-local"}"#;
        assert_eq!(post_json(&fixture, result).0, 200);
        let observation = fixture.take_result(token).expect("read observation");
        assert!(
            observation.cookie.as_deref() == Some("existing-cookie"),
            "cookie value mismatch"
        );
        assert!(
            observation.local_storage.as_deref() == Some("existing-local"),
            "localStorage value mismatch"
        );
    }

    #[test]
    fn malformed_or_type_mismatched_result_json_is_rejected_then_valid_post_is_accepted() {
        let fixture = ProfileFixture::new().expect("start fixture");
        let token = "default-C-json";
        let url = fixture
            .url(token, "write", "json-value")
            .expect("register URL");
        assert_eq!(get(&fixture, &url).0, 200);

        let wrong_type = format!(r#"{{"token":"{token}","cookie":7,"local_storage":null}}"#);
        assert_eq!(post_json(&fixture, &wrong_type).0, 400);
        assert_eq!(fixture.take_result(token), None);

        // Missing fields must be rejected rather than defaulted to null: an
        // empty read that never reported a value is not an observed null.
        let missing_cookie = format!(r#"{{"token":"{token}","local_storage":null}}"#);
        assert_eq!(post_json(&fixture, &missing_cookie).0, 400);
        assert_eq!(fixture.take_result(token), None);
        let missing_storage = format!(r#"{{"token":"{token}","cookie":null}}"#);
        assert_eq!(post_json(&fixture, &missing_storage).0, 400);
        assert_eq!(fixture.take_result(token), None);
        let missing_both = format!(r#"{{"token":"{token}"}}"#);
        assert_eq!(post_json(&fixture, &missing_both).0, 400);
        assert_eq!(fixture.take_result(token), None);

        let malformed = format!(r#"{{"token":"{token}","cookie":"#);
        assert_eq!(post_json(&fixture, &malformed).0, 400);
        assert_eq!(fixture.take_result(token), None);
        assert!(
            fixture
                .errors()
                .iter()
                .any(|error| error.contains("invalid result JSON")),
            "invalid JSON was not reported"
        );

        // A page-side exception reports `error`; the fixture records a clear
        // failure and still consumes that token.
        let failing = "default-C-error";
        let failing_url = fixture
            .url(failing, "write", "error-value")
            .expect("register failing URL");
        assert_eq!(get(&fixture, &failing_url).0, 200);
        let reported = format!(r#"{{"token":"{failing}","error":"page failure"}}"#);
        assert_eq!(post_json(&fixture, &reported).0, 422);
        assert_eq!(fixture.take_result(failing), None);
        assert_eq!(post_json(&fixture, &reported).0, 409);
        assert!(
            fixture
                .errors()
                .iter()
                .any(|error| error.contains("page reported an error")),
            "page error report was not recorded"
        );

        let valid = format!(r#"{{"token":"{token}","cookie":null,"local_storage":null}}"#);
        assert_eq!(post_json(&fixture, &valid).0, 200);
        let observation = fixture.take_result(token).expect("valid result");
        assert_eq!(observation.token, token);
        assert_eq!(observation.cookie, None);
        assert_eq!(observation.local_storage, None);
    }

    #[test]
    fn drop_stops_the_worker_with_bounded_wait_during_a_half_open_request() {
        let fixture = ProfileFixture::new().expect("start fixture");
        let mut stream = TcpStream::connect(address(&fixture)).expect("connect");
        stream
            .set_write_timeout(Some(Duration::from_secs(1)))
            .expect("set write timeout");
        stream
            .write_all(b"GET /profile-test?token=half HTTP/1.1\r\nHost: fixture\r\n")
            .expect("write partial request");
        stream.flush().expect("flush partial request");

        let started = Instant::now();
        drop(fixture);
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(2),
            "fixture drop blocked for {elapsed:?}"
        );
    }
}
