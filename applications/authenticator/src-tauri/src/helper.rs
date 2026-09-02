#![cfg(target_os = "linux")]

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant};

const PROTOCOL_VERSION: u8 = 1;
const MAX_REQUEST_BYTES: u64 = 16 * 1024;
const MAX_ENTRIES: usize = 200;
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const CLIPBOARD_TTL: Duration = Duration::from_secs(20);
// Codes expire with their TOTP period, and the publisher lives in a webview
// that `--background` keeps hidden, where timers are throttled. A stalled
// publisher would otherwise leave its last snapshot on the socket long after
// those codes stopped being valid, so anything older than this is served as if
// nothing had been published at all.
const SNAPSHOT_TTL: Duration = Duration::from_secs(5);
const CLIENT_TIMEOUT: Duration = Duration::from_secs(2);
// One request/response exchange is a few kilobytes; anything that takes longer
// than this end to end is a client dripping bytes to hold a connection slot.
const CLIENT_DEADLINE: Duration = Duration::from_secs(5);
const MAX_CONNECTIONS: usize = 16;
const WL_COPY: &str = "/usr/bin/wl-copy";
// How long the clipboard child gets to prove it took ownership. wl-copy in
// foreground mode exits immediately if it cannot connect to the compositor or
// another client refuses the offer; a child still alive after this window
// has bound the selection.
const CLIPBOARD_OWNERSHIP_GRACE: Duration = Duration::from_millis(150);
// Source commit the binary was built from, embedded by build.rs so `status`
// can identify the running artifact.
const SOURCE_COMMIT: &str = env!("OMARCHY_HELPER_SOURCE_COMMIT");

#[derive(Clone, Debug, Deserialize, Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct HelperEntry {
    pub id: String,
    pub name: String,
    pub issuer: String,
    #[serde(rename = "type")]
    pub entry_type: String,
    pub code: String,
    pub next_code: String,
    pub period: u32,
    pub valid_until: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct HelperSnapshot {
    pub state: String,
    pub locked: bool,
    pub synced: bool,
    pub account: String,
    pub generation: u64,
    pub now: u64,
    pub entries: Vec<HelperEntry>,
}

impl Default for HelperSnapshot {
    fn default() -> Self {
        Self {
            state: "unavailable".into(),
            locked: false,
            synced: false,
            account: String::new(),
            generation: 0,
            now: 0,
            entries: Vec::new(),
        }
    }
}

trait ClipboardProcess: Send {
    fn terminate(&mut self);
}

impl ClipboardProcess for Child {
    fn terminate(&mut self) {
        let _ = self.kill();
        let deadline = Instant::now() + Duration::from_millis(500);
        loop {
            match self.try_wait() {
                Ok(Some(_)) | Err(_) => return,
                Ok(None) if Instant::now() >= deadline => return,
                Ok(None) => thread::sleep(Duration::from_millis(10)),
            }
        }
    }
}

struct ClipboardOwner {
    generation: u64,
    process: Box<dyn ClipboardProcess>,
}

impl Drop for ClipboardOwner {
    fn drop(&mut self) {
        self.process.terminate();
    }
}

#[derive(Clone, Default)]
pub struct HelperState {
    snapshot: Arc<RwLock<HelperSnapshot>>,
    published_at: Arc<Mutex<Option<Instant>>>,
    manual_locked: Arc<AtomicBool>,
    login_requested: Arc<AtomicBool>,
    clipboard_generation: Arc<AtomicU64>,
    clipboard_owner: Arc<Mutex<Option<ClipboardOwner>>>,
    // Random per-process identifier. Generations restart at 1 with every
    // helper process, so a client keeping a monotonic floor across a helper
    // restart would reject every fresh snapshot until it caught up. Sending the
    // instance lets the client reset its floor only when the process changed.
    instance: Arc<str>,
}

fn new_instance_id() -> Arc<str> {
    let mut bytes = [0u8; 16];
    let mut file = fs::File::open("/dev/urandom").expect("urandom unavailable");
    file.read_exact(&mut bytes).expect("urandom read failed");
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    hex.into()
}

impl HelperState {
    pub fn new() -> Self {
        Self {
            instance: new_instance_id(),
            ..Self::default()
        }
    }

    pub fn with_login_requested(login_requested: bool) -> Self {
        Self {
            login_requested: Arc::new(AtomicBool::new(login_requested)),
            ..Self::new()
        }
    }

    fn instance(&self) -> &str {
        if self.instance.is_empty() {
            "0"
        } else {
            &self.instance
        }
    }

    pub fn request_login(&self) {
        self.login_requested.store(true, Ordering::Release);
    }

    pub fn take_login_request(&self) -> bool {
        self.login_requested.swap(false, Ordering::AcqRel)
    }

    fn next_clipboard_generation(&self) -> u64 {
        self.clipboard_generation
            .fetch_add(1, Ordering::AcqRel)
            .saturating_add(1)
    }

    // Every path that retires an owner takes it out from under the mutex and
    // lets it drop afterwards: `terminate()` can block for up to 500 ms, and
    // holding the clipboard lock across that stalls every concurrent copy and
    // lock request behind one slow child.
    fn replace_clipboard_owner(&self, process: Box<dyn ClipboardProcess>) -> u64 {
        let generation = self.next_clipboard_generation();
        let previous = {
            let mut owner = self
                .clipboard_owner
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            owner.replace(ClipboardOwner {
                generation,
                process,
            })
        };
        drop(previous);
        generation
    }

    fn expire_clipboard_owner(&self, generation: u64) {
        let expired = {
            let mut owner = self
                .clipboard_owner
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if owner
                .as_ref()
                .is_some_and(|current| current.generation == generation)
            {
                owner.take()
            } else {
                None
            }
        };
        drop(expired);
    }

    fn clear_clipboard_owner(&self) {
        self.clipboard_generation.fetch_add(1, Ordering::AcqRel);
        let cleared = self
            .clipboard_owner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        drop(cleared);
    }

    fn publish(&self, mut snapshot: HelperSnapshot) -> Result<(), String> {
        validate_snapshot(&snapshot)?;
        let mut current = self
            .snapshot
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if self.manual_locked.load(Ordering::Acquire) {
            return Ok(());
        }
        snapshot.generation = snapshot
            .generation
            .max(current.generation.saturating_add(1));
        let encoded = serde_json::to_vec(&snapshot).map_err(|_| "snapshot_encode".to_string())?;
        if encoded.len() > MAX_RESPONSE_BYTES {
            return Err("snapshot_too_large".into());
        }
        *current = snapshot;
        *self
            .published_at
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Instant::now());
        Ok(())
    }

    // A snapshot whose publisher has stalled is served as if nothing had been
    // published. Only `ready` carries codes, so staleness degrades that state to
    // `unavailable` and drops the rows; states such as `locked` or `needs_login`
    // carry none and stay true until the app republishes.
    fn current_snapshot(&self) -> (HelperSnapshot, bool) {
        let snapshot = self
            .snapshot
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let fresh = self
            .published_at
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_some_and(|published| published.elapsed() < SNAPSHOT_TTL);
        if fresh || snapshot.state != "ready" {
            return (snapshot, false);
        }
        (
            HelperSnapshot {
                state: "unavailable".into(),
                entries: Vec::new(),
                ..snapshot
            },
            true,
        )
    }

    /// Latches the helper locked and clears everything it was serving. Returns
    /// the new generation. The write guard is released before the clipboard
    /// owner is terminated so a slow `wl-copy` cannot stall `publish`.
    fn lock(&self) -> u64 {
        let generation = {
            let mut snapshot = self
                .snapshot
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            self.manual_locked.store(true, Ordering::Release);
            snapshot.locked = true;
            snapshot.state = "locked".into();
            snapshot.entries.clear();
            snapshot.generation = snapshot.generation.saturating_add(1);
            snapshot.generation
        };
        self.clear_clipboard_owner();
        generation
    }

    /// Clears the manual lock latch and drops the published snapshot.
    ///
    /// Neither the socket nor the `--login` argv path may clear the latch, so
    /// there is no production caller: a manual lock now persists for the life of
    /// the process and is released by restarting the app. Retained for the tests
    /// that cover the suppress-then-resume publication semantics.
    #[cfg(test)]
    pub fn unlock(&self) {
        if let Ok(mut snapshot) = self.snapshot.write() {
            self.manual_locked.store(false, Ordering::Release);
            snapshot.state = "unavailable".into();
            snapshot.locked = false;
            snapshot.entries.clear();
            snapshot.generation = snapshot.generation.saturating_add(1);
        }
    }
}

/// Path of the socket this process bound, kept in Tauri managed state so the
/// exit hook can unlink exactly that node.
pub struct SocketPath(pub PathBuf);

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HelperRequest {
    v: u8,
    id: String,
    op: String,
    item_id: Option<String>,
}

fn bounded_text(value: &str, max: usize) -> bool {
    value.len() <= max
        && !value.chars().any(|character| {
            character.is_control()
                || matches!(
                    character,
                    '\u{061c}'
                        | '\u{200b}'..='\u{200f}'
                        | '\u{202a}'..='\u{202e}'
                        | '\u{2060}'
                        | '\u{2066}'..='\u{2069}'
                        | '\u{feff}'
                )
        })
}

fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b':' | b'-'))
}

fn valid_code(code: &str, entry_type: &str) -> bool {
    if entry_type == "Steam" {
        code.len() == 5
            && code
                .bytes()
                .all(|c| b"23456789BCDFGHJKMNPQRTVWXY".contains(&c))
    } else {
        (6..=10).contains(&code.len()) && code.bytes().all(|c| c.is_ascii_digit())
    }
}

fn validate_snapshot(snapshot: &HelperSnapshot) -> Result<(), String> {
    if !matches!(
        snapshot.state.as_str(),
        "ready" | "locked" | "needs_login" | "unavailable" | "error"
    ) {
        return Err("invalid_state".into());
    }
    if !bounded_text(&snapshot.account, 120) || snapshot.entries.len() > MAX_ENTRIES {
        return Err("snapshot_bounds".into());
    }
    if snapshot.locked && !snapshot.entries.is_empty() {
        return Err("locked_snapshot_has_entries".into());
    }
    for entry in &snapshot.entries {
        if !valid_id(&entry.id)
            || !bounded_text(&entry.name, 80)
            || !bounded_text(&entry.issuer, 80)
            || !matches!(entry.entry_type.as_str(), "Totp" | "Steam")
            || !valid_code(&entry.code, &entry.entry_type)
            || !valid_code(&entry.next_code, &entry.entry_type)
            || !(15..=120).contains(&entry.period)
        {
            return Err("invalid_entry".into());
        }
    }
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn publish_helper_snapshot(
    state: tauri::State<'_, HelperState>,
    snapshot: HelperSnapshot,
) -> Result<(), String> {
    state.inner().publish(snapshot)
}

#[tauri::command]
#[specta::specta]
pub fn take_helper_login_request(state: tauri::State<'_, HelperState>) -> bool {
    state.inner().take_login_request()
}

fn socket_path() -> io::Result<PathBuf> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|value| !value.is_empty())
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "XDG_RUNTIME_DIR is not set"))?;
    Ok(PathBuf::from(runtime)
        .join("proton-authenticator-omarchy")
        .join("helper.sock"))
}

fn prepare_socket(path: &Path) -> io::Result<UnixListener> {
    let uid = unsafe { libc::geteuid() };
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "socket has no parent"))?;
    match fs::symlink_metadata(parent) {
        Ok(metadata) => {
            if metadata.uid() != uid || !metadata.file_type().is_dir() {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "unsafe runtime directory",
                ));
            }
            if metadata.permissions().mode() & 0o077 != 0 {
                fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::DirBuilder::new().mode(0o700).create(parent)?;
        }
        Err(error) => return Err(error),
    }
    let parent_meta = fs::symlink_metadata(parent)?;
    if parent_meta.uid() != uid
        || !parent_meta.file_type().is_dir()
        || parent_meta.permissions().mode() & 0o077 != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe runtime directory",
        ));
    }

    if let Ok(metadata) = fs::symlink_metadata(path) {
        if metadata.uid() != uid || !metadata.file_type().is_socket() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "unsafe existing socket path",
            ));
        }
        fs::remove_file(path)?;
    }

    // `bind` creates the socket node with `0777 & ~umask`, and the systemd unit's
    // UMask is not something an ad-hoc launch inherits. Force the umask for the
    // bind itself so there is no window where the node is group/other-reachable
    // before the explicit chmod below.
    let previous_umask = unsafe { libc::umask(0o077) };
    let bound = UnixListener::bind(path);
    unsafe { libc::umask(previous_umask) };
    let listener = bound?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

/// Removes the socket node this process created, if it is still ours. Called
/// from the app exit hook; a stale node left behind would otherwise trip the
/// next start's safety check when combined with a planted symlink.
pub fn remove_socket(path: &Path) {
    let uid = unsafe { libc::geteuid() };
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if metadata.uid() == uid && metadata.file_type().is_socket() {
            let _ = fs::remove_file(path);
        }
    }
}

fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
    let fd = stream.as_raw_fd();
    let mut credentials = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut length,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(credentials.uid)
}

fn envelope(id: &str, body: Value) -> Value {
    let mut map = match body {
        Value::Object(map) => map,
        _ => serde_json::Map::new(),
    };
    map.insert("v".into(), json!(PROTOCOL_VERSION));
    map.insert("id".into(), json!(id));
    Value::Object(map)
}

fn snapshot_response(id: &str, snapshot: &HelperSnapshot, stale: bool, instance: &str) -> Value {
    let mut body = serde_json::to_value(snapshot).unwrap_or_else(|_| json!({ "state": "error" }));
    if let Value::Object(map) = &mut body {
        map.insert("ok".into(), json!(true));
        map.insert("stale".into(), json!(stale));
        map.insert("instance".into(), json!(instance));
    }
    envelope(id, body)
}

fn copy_to_clipboard(state: &HelperState, code: &str) -> io::Result<()> {
    let mut child = Command::new(WL_COPY)
        .args(["--foreground", "--sensitive"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "clipboard stdin unavailable"))?;
    if let Err(error) = stdin.write_all(code.as_bytes()) {
        child.terminate();
        return Err(error);
    }
    drop(stdin);
    // A foreground wl-copy that fails to bind the selection exits at once. One
    // immediate `try_wait` races that exit, so poll through a short grace period
    // before reporting `copied: true` to the caller.
    let deadline = Instant::now() + CLIPBOARD_OWNERSHIP_GRACE;
    loop {
        match child.try_wait() {
            Ok(None) if Instant::now() >= deadline => break,
            Ok(None) => thread::sleep(Duration::from_millis(10)),
            Ok(Some(_)) => return Err(io::Error::other("wl-copy exited before owning clipboard")),
            Err(error) => {
                child.terminate();
                return Err(error);
            }
        }
    }

    let expiration = state.replace_clipboard_owner(Box::new(child));
    let state = state.clone();
    // One sleeper per successful copy. It only touches the owner if its own
    // generation is still current, so a replaced copy's sleeper is a no-op.
    let _ = thread::Builder::new()
        .name("omarchy-authenticator-clipboard-ttl".into())
        .spawn(move || {
            thread::sleep(CLIPBOARD_TTL);
            state.expire_clipboard_owner(expiration);
        });
    Ok(())
}

fn handle_request(state: &HelperState, request: HelperRequest) -> Value {
    if request.v != PROTOCOL_VERSION || !valid_id(&request.id) {
        return envelope(
            &request.id,
            json!({ "ok": false, "error": "invalid_request" }),
        );
    }

    match request.op.as_str() {
        // `status` deliberately omits the account: it is the only op a client
        // is expected to call before deciding whether to trust the socket, so
        // it must not disclose which Proton identity is signed in.
        "status" => {
            let (snapshot, stale) = state.current_snapshot();
            envelope(
                &request.id,
                json!({
                    "ok": true,
                    "state": snapshot.state,
                    "locked": snapshot.locked,
                    "synced": snapshot.synced,
                    "signedIn": !snapshot.account.is_empty(),
                    "generation": snapshot.generation,
                    "instance": state.instance(),
                    "sourceCommit": SOURCE_COMMIT,
                    "count": snapshot.entries.len(),
                    "stale": stale,
                }),
            )
        }
        "snapshot" => {
            let (snapshot, stale) = state.current_snapshot();
            snapshot_response(&request.id, &snapshot, stale, state.instance())
        }
        "copy" => {
            let Some(item_id) = request.item_id.filter(|id| valid_id(id)) else {
                return envelope(
                    &request.id,
                    json!({ "ok": false, "error": "invalid_item_id" }),
                );
            };
            let (snapshot, stale) = state.current_snapshot();
            if stale {
                return envelope(&request.id, json!({ "ok": false, "error": "stale" }));
            }
            if snapshot.locked || snapshot.state != "ready" {
                return envelope(&request.id, json!({ "ok": false, "error": "locked" }));
            }
            let Some(entry) = snapshot.entries.iter().find(|entry| entry.id == item_id) else {
                return envelope(&request.id, json!({ "ok": false, "error": "not_found" }));
            };
            match copy_to_clipboard(state, &entry.code) {
                Ok(()) => envelope(
                    &request.id,
                    json!({ "ok": true, "copied": true, "generation": snapshot.generation }),
                ),
                Err(_) => envelope(
                    &request.id,
                    json!({ "ok": false, "error": "clipboard_failed" }),
                ),
            }
        }
        "lock" => {
            let generation = state.lock();
            envelope(
                &request.id,
                json!({ "ok": true, "state": "locked", "locked": true, "generation": generation, "instance": state.instance() }),
            )
        }
        // `unlock` is deliberately not exposed over the socket: the lock
        // direction is fail-safe, but a remotely triggerable unlock would let any
        // same-uid process clear the manual lock latch without confirmation.
        _ => envelope(
            &request.id,
            json!({ "ok": false, "error": "unsupported_operation" }),
        ),
    }
}

/// Reads one newline-terminated request line with both a per-read timeout and
/// a wall-clock deadline. `set_read_timeout` alone restarts on every byte, so a
/// client dripping one byte per second could hold a connection slot forever.
fn read_request_line(stream: &UnixStream, deadline: Instant) -> io::Result<String> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = Vec::new();
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "request deadline exceeded"))?;
        stream.set_read_timeout(Some(remaining.min(CLIENT_TIMEOUT)))?;
        let buffer = reader.fill_buf()?;
        if buffer.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "connection closed before newline",
            ));
        }
        let (chunk, done) = match buffer.iter().position(|byte| *byte == b'\n') {
            Some(index) => (&buffer[..index], true),
            None => (buffer, false),
        };
        if line.len() + chunk.len() > MAX_REQUEST_BYTES as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "request too large",
            ));
        }
        line.extend_from_slice(chunk);
        let consumed = if done { chunk.len() + 1 } else { chunk.len() };
        reader.consume(consumed);
        if done {
            break;
        }
    }
    String::from_utf8(line)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "request is not utf-8"))
}

fn handle_client(state: HelperState, mut stream: UnixStream) -> io::Result<()> {
    let deadline = Instant::now() + CLIENT_DEADLINE;
    stream.set_write_timeout(Some(CLIENT_TIMEOUT))?;
    if peer_uid(&stream)? != unsafe { libc::geteuid() } {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "peer uid mismatch",
        ));
    }

    let line = read_request_line(&stream, deadline)?;
    if line.trim().is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "empty request"));
    }
    let request: HelperRequest = serde_json::from_str(line.trim_end())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid json"))?;
    let mut encoded = serde_json::to_vec(&handle_request(&state, request))?;
    encoded.push(b'\n');
    if encoded.len() > MAX_RESPONSE_BYTES {
        encoded = b"{\"v\":1,\"id\":\"\",\"ok\":false,\"error\":\"response_too_large\"}\n".to_vec();
    }
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "request deadline exceeded"))?;
    stream.set_write_timeout(Some(remaining.min(CLIENT_TIMEOUT)))?;
    stream.write_all(&encoded)
}

struct ConnectionGuard(Arc<AtomicUsize>);

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

pub fn start_socket_server(state: HelperState) -> io::Result<PathBuf> {
    let path = socket_path()?;
    let listener = prepare_socket(&path)?;
    let active_connections = Arc::new(AtomicUsize::new(0));
    thread::Builder::new()
        .name("omarchy-authenticator-helper".into())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                if active_connections
                    .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                        (count < MAX_CONNECTIONS).then_some(count + 1)
                    })
                    .is_err()
                {
                    continue;
                }
                let state = state.clone();
                let guard = ConnectionGuard(active_connections.clone());
                let _ = thread::Builder::new()
                    .name("omarchy-authenticator-client".into())
                    .spawn(move || {
                        let _guard = guard;
                        let _ = handle_client(state, stream);
                    });
            }
        })?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeClipboardProcess(Arc<AtomicBool>);

    impl ClipboardProcess for FakeClipboardProcess {
        fn terminate(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }

    fn valid_snapshot() -> HelperSnapshot {
        HelperSnapshot {
            state: "ready".into(),
            locked: false,
            synced: true,
            account: "test@example.test".into(),
            generation: 1,
            now: 59,
            entries: vec![HelperEntry {
                id: "fixture-rfc6238".into(),
                name: "test".into(),
                issuer: "RFC6238".into(),
                entry_type: "Totp".into(),
                code: "94287082".into(),
                next_code: "37359152".into(),
                period: 30,
                valid_until: 60,
            }],
        }
    }

    fn backdate_publication(state: &HelperState, age: Duration) {
        let mut published = state.published_at.lock().unwrap();
        *published = published.map(|instant| instant - age);
    }

    #[test]
    fn login_request_latch_is_consumed_once() {
        let state = HelperState::with_login_requested(true);
        assert!(state.take_login_request());
        assert!(!state.take_login_request());
        state.request_login();
        assert!(state.take_login_request());
    }

    #[test]
    fn clipboard_ownership_replacement_and_expiry_are_generation_safe() {
        let state = HelperState::default();
        let first_stopped = Arc::new(AtomicBool::new(false));
        let second_stopped = Arc::new(AtomicBool::new(false));
        let first =
            state.replace_clipboard_owner(Box::new(FakeClipboardProcess(first_stopped.clone())));
        let second =
            state.replace_clipboard_owner(Box::new(FakeClipboardProcess(second_stopped.clone())));
        assert!(first_stopped.load(Ordering::Acquire));
        assert!(!second_stopped.load(Ordering::Acquire));
        state.expire_clipboard_owner(first);
        assert!(!second_stopped.load(Ordering::Acquire));
        state.expire_clipboard_owner(second);
        assert!(second_stopped.load(Ordering::Acquire));

        let lock_stopped = Arc::new(AtomicBool::new(false));
        state.replace_clipboard_owner(Box::new(FakeClipboardProcess(lock_stopped.clone())));
        let _ = handle_request(
            &state,
            HelperRequest {
                v: 1,
                id: "lock-owner".into(),
                op: "lock".into(),
                item_id: None,
            },
        );
        assert!(lock_stopped.load(Ordering::Acquire));
    }

    #[test]
    fn validates_bounded_snapshot() {
        assert!(validate_snapshot(&valid_snapshot()).is_ok());
    }

    #[test]
    fn rejects_locked_snapshot_with_codes() {
        let mut snapshot = valid_snapshot();
        snapshot.locked = true;
        snapshot.state = "locked".into();
        assert_eq!(
            validate_snapshot(&snapshot),
            Err("locked_snapshot_has_entries".into())
        );
    }

    #[test]
    fn rejects_invalid_code_and_id() {
        let mut snapshot = valid_snapshot();
        snapshot.entries[0].id = "../bad".into();
        snapshot.entries[0].code = "12 3456".into();
        assert_eq!(validate_snapshot(&snapshot), Err("invalid_entry".into()));
    }

    #[test]
    fn rejects_bidi_and_zero_width_labels() {
        for character in [
            '\u{061c}', '\u{200b}', '\u{200e}', '\u{200f}', '\u{202e}', '\u{2060}',
        ] {
            let mut snapshot = valid_snapshot();
            snapshot.entries[0].name = format!("safe{character}spoof");
            assert_eq!(validate_snapshot(&snapshot), Err("invalid_entry".into()));
        }
    }

    #[test]
    fn rejects_labels_over_utf8_byte_limit() {
        let mut snapshot = valid_snapshot();
        snapshot.entries[0].name = "ü".repeat(80);
        assert_eq!(validate_snapshot(&snapshot), Err("invalid_entry".into()));
    }

    #[test]
    fn rejects_symlinked_runtime_directory() {
        use std::os::unix::fs::symlink;

        let base = std::env::temp_dir().join(format!(
            "proton-authenticator-helper-test-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("unnamed")
        ));
        let target = base.join("target");
        let link = base.join("runtime-link");
        fs::create_dir_all(&target).unwrap();
        symlink(&target, &link).unwrap();
        let error = prepare_socket(&link.join("helper.sock")).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn lock_clears_rows_and_advances_generation() {
        let state = HelperState::default();
        *state.snapshot.write().unwrap() = valid_snapshot();
        let response = handle_request(
            &state,
            HelperRequest {
                v: 1,
                id: "abcd".into(),
                op: "lock".into(),
                item_id: None,
            },
        );
        assert_eq!(response["ok"], true);
        {
            let snapshot = state.snapshot.read().unwrap();
            assert!(snapshot.locked);
            assert!(snapshot.entries.is_empty());
            assert_eq!(snapshot.generation, 2);
        }

        let mut republished = valid_snapshot();
        republished.generation = 3;
        state.publish(republished.clone()).unwrap();
        assert!(state.snapshot.read().unwrap().locked);
        assert!(state.snapshot.read().unwrap().entries.is_empty());

        let unlock_response = handle_request(
            &state,
            HelperRequest {
                v: 1,
                id: "efgh".into(),
                op: "unlock".into(),
                item_id: None,
            },
        );
        assert_eq!(unlock_response["ok"], false);
        assert_eq!(unlock_response["error"], "unsupported_operation");
        assert!(state.snapshot.read().unwrap().locked);

        state.unlock();
        state.publish(republished).unwrap();
        {
            let snapshot = state.snapshot.read().unwrap();
            assert!(!snapshot.locked);
            assert_eq!(snapshot.state, "ready");
            assert_eq!(snapshot.entries.len(), 1);
            assert_eq!(snapshot.generation, 4);
        }
    }

    #[test]
    fn stale_ready_snapshot_is_served_as_unavailable() {
        let state = HelperState::default();
        state.publish(valid_snapshot()).unwrap();

        let (fresh, stale) = state.current_snapshot();
        assert!(!stale);
        assert_eq!(fresh.state, "ready");
        assert_eq!(fresh.entries.len(), 1);

        backdate_publication(&state, SNAPSHOT_TTL + Duration::from_secs(1));
        let (expired, stale) = state.current_snapshot();
        assert!(stale);
        assert_eq!(expired.state, "unavailable");
        assert!(expired.entries.is_empty());
        // the underlying snapshot is untouched, so a republish recovers instantly
        assert_eq!(state.snapshot.read().unwrap().entries.len(), 1);
        state.publish(valid_snapshot()).unwrap();
        assert!(!state.current_snapshot().1);
    }

    #[test]
    fn snapshot_op_reports_staleness_and_withholds_codes() {
        let state = HelperState::default();
        state.publish(valid_snapshot()).unwrap();
        backdate_publication(&state, SNAPSHOT_TTL + Duration::from_secs(1));

        let response = handle_request(
            &state,
            HelperRequest {
                v: 1,
                id: "stale-snapshot".into(),
                op: "snapshot".into(),
                item_id: None,
            },
        );
        assert_eq!(response["ok"], true);
        assert_eq!(response["stale"], true);
        assert_eq!(response["state"], "unavailable");
        assert_eq!(response["entries"].as_array().unwrap().len(), 0);

        let status = handle_request(
            &state,
            HelperRequest {
                v: 1,
                id: "stale-status".into(),
                op: "status".into(),
                item_id: None,
            },
        );
        assert_eq!(status["stale"], true);
        assert_eq!(status["state"], "unavailable");
        assert_eq!(status["count"], 0);
    }

    #[test]
    fn copy_refuses_a_stale_snapshot() {
        let state = HelperState::default();
        state.publish(valid_snapshot()).unwrap();
        backdate_publication(&state, SNAPSHOT_TTL + Duration::from_secs(1));

        let response = handle_request(
            &state,
            HelperRequest {
                v: 1,
                id: "stale-copy".into(),
                op: "copy".into(),
                item_id: Some("fixture-rfc6238".into()),
            },
        );
        assert_eq!(response["ok"], false);
        assert_eq!(response["error"], "stale");
    }

    #[test]
    fn never_published_state_is_not_reported_as_ready() {
        let state = HelperState::default();
        let (snapshot, stale) = state.current_snapshot();
        assert!(!stale);
        assert_eq!(snapshot.state, "unavailable");

        // a snapshot forced into the lock without going through `publish` has no
        // publication instant, so it must not be served as live
        *state.snapshot.write().unwrap() = valid_snapshot();
        let (snapshot, stale) = state.current_snapshot();
        assert!(stale);
        assert_eq!(snapshot.state, "unavailable");
        assert!(snapshot.entries.is_empty());
    }

    #[test]
    fn stale_non_ready_states_are_preserved() {
        let state = HelperState::default();
        let mut locked = valid_snapshot();
        locked.state = "locked".into();
        locked.locked = true;
        locked.entries.clear();
        state.publish(locked).unwrap();
        backdate_publication(&state, SNAPSHOT_TTL + Duration::from_secs(1));

        let (snapshot, stale) = state.current_snapshot();
        assert!(!stale);
        assert_eq!(snapshot.state, "locked");
        assert!(snapshot.locked);
    }

    fn request(op: &str, item_id: Option<&str>) -> HelperRequest {
        HelperRequest {
            v: 1,
            id: "req".into(),
            op: op.into(),
            item_id: item_id.map(Into::into),
        }
    }

    #[test]
    fn every_process_gets_a_distinct_instance_that_all_ops_report() {
        let first = HelperState::new();
        let second = HelperState::new();
        assert_eq!(first.instance().len(), 32);
        assert_ne!(first.instance(), second.instance());
        assert!(first.instance().bytes().all(|c| c.is_ascii_hexdigit()));

        first.publish(valid_snapshot()).unwrap();
        for op in ["status", "snapshot", "lock"] {
            let response = handle_request(&first, request(op, None));
            assert_eq!(response["ok"], true, "{op}");
            assert_eq!(response["instance"], first.instance(), "{op}");
        }
        // A default-constructed state (tests) still reports a non-empty value.
        assert_eq!(HelperState::default().instance(), "0");
    }

    #[test]
    fn status_reports_provenance_and_sign_in_but_never_the_account() {
        let state = HelperState::new();
        state.publish(valid_snapshot()).unwrap();
        let status = handle_request(&state, request("status", None));
        assert_eq!(status["ok"], true);
        assert_eq!(status["signedIn"], true);
        assert!(status.get("account").is_none());
        assert_eq!(status["sourceCommit"], SOURCE_COMMIT);
        assert!(!SOURCE_COMMIT.is_empty());
        let text = status.to_string();
        assert!(!text.contains("test@example.test"));
        assert!(!text.contains("94287082"));
    }

    struct SlowClipboardProcess {
        stopped: Arc<AtomicBool>,
        delay: Duration,
    }

    impl ClipboardProcess for SlowClipboardProcess {
        fn terminate(&mut self) {
            thread::sleep(self.delay);
            self.stopped.store(true, Ordering::Release);
        }
    }

    #[test]
    fn lock_does_not_hold_the_snapshot_lock_while_terminating_the_clipboard_owner() {
        let state = HelperState::new();
        state.publish(valid_snapshot()).unwrap();
        let stopped = Arc::new(AtomicBool::new(false));
        state.replace_clipboard_owner(Box::new(SlowClipboardProcess {
            stopped: stopped.clone(),
            delay: Duration::from_millis(300),
        }));

        let locker = state.clone();
        let handle = thread::spawn(move || locker.lock());
        // Give `lock` time to enter the slow terminate.
        thread::sleep(Duration::from_millis(50));
        let started = Instant::now();
        let (snapshot, _) = state.current_snapshot();
        assert!(
            started.elapsed() < Duration::from_millis(150),
            "snapshot read blocked behind clipboard termination"
        );
        assert!(snapshot.locked);
        handle.join().unwrap();
        assert!(stopped.load(Ordering::Acquire));
    }

    #[test]
    fn replacing_an_owner_terminates_it_outside_the_clipboard_lock() {
        let state = HelperState::new();
        let stopped = Arc::new(AtomicBool::new(false));
        state.replace_clipboard_owner(Box::new(SlowClipboardProcess {
            stopped: stopped.clone(),
            delay: Duration::from_millis(300),
        }));
        let replacer = state.clone();
        let handle = thread::spawn(move || {
            replacer.replace_clipboard_owner(Box::new(FakeClipboardProcess(Arc::new(
                AtomicBool::new(false),
            ))));
        });
        thread::sleep(Duration::from_millis(50));
        let started = Instant::now();
        // A concurrent expiry must not queue behind the slow terminate.
        state.expire_clipboard_owner(u64::MAX);
        assert!(started.elapsed() < Duration::from_millis(150));
        handle.join().unwrap();
        assert!(stopped.load(Ordering::Acquire));
    }

    #[test]
    fn a_dripping_client_is_cut_off_at_the_wall_clock_deadline() {
        let (mut client, server) = UnixStream::pair().unwrap();
        let started = Instant::now();
        let reader = thread::spawn(move || {
            read_request_line(&server, Instant::now() + Duration::from_millis(600))
        });
        // Drip one byte every 100 ms: each read succeeds inside CLIENT_TIMEOUT,
        // so only the wall-clock deadline can end this.
        for _ in 0..20 {
            if client.write_all(b"{").is_err() {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
        let error = reader.join().unwrap().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_millis(1500));
    }

    #[test]
    fn oversized_and_well_formed_request_lines_are_handled() {
        let (mut client, server) = UnixStream::pair().unwrap();
        let reader = thread::spawn(move || {
            read_request_line(&server, Instant::now() + Duration::from_secs(2))
        });
        client.write_all(b"{\"v\":1}\nignored").unwrap();
        assert_eq!(reader.join().unwrap().unwrap(), "{\"v\":1}");

        let (mut client, server) = UnixStream::pair().unwrap();
        let reader = thread::spawn(move || {
            read_request_line(&server, Instant::now() + Duration::from_secs(2))
        });
        let oversized = vec![b'a'; MAX_REQUEST_BYTES as usize + 1];
        let _ = client.write_all(&oversized);
        let error = reader.join().unwrap().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn remove_socket_only_unlinks_an_owned_socket_node() {
        let base = std::env::temp_dir().join(format!(
            "proton-authenticator-helper-remove-{}",
            std::process::id()
        ));
        fs::create_dir_all(&base).unwrap();
        let regular = base.join("regular");
        fs::write(&regular, b"x").unwrap();
        remove_socket(&regular);
        assert!(regular.exists(), "a regular file must not be unlinked");

        let socket = base.join("helper.sock");
        let listener = prepare_socket(&socket).unwrap();
        assert_eq!(
            fs::symlink_metadata(&socket).unwrap().permissions().mode() & 0o777,
            0o600
        );
        drop(listener);
        remove_socket(&socket);
        assert!(!socket.exists());
        fs::remove_dir_all(&base).unwrap();
    }
}
