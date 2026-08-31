#![cfg(target_os = "linux")]

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::Duration;

const PROTOCOL_VERSION: u8 = 1;
const MAX_REQUEST_BYTES: u64 = 16 * 1024;
const MAX_ENTRIES: usize = 200;
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const CLIPBOARD_TTL: Duration = Duration::from_secs(20);
const CLIENT_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_CONNECTIONS: usize = 16;
const MAX_CLIPBOARD_BYTES: u64 = 64;
const WL_COPY: &str = "/usr/bin/wl-copy";
const WL_PASTE: &str = "/usr/bin/wl-paste";

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

#[derive(Clone, Default)]
pub struct HelperState {
    snapshot: Arc<RwLock<HelperSnapshot>>,
    manual_locked: Arc<AtomicBool>,
    login_requested: Arc<AtomicBool>,
    clipboard_generation: Arc<AtomicU64>,
}

impl HelperState {
    pub fn with_login_requested(login_requested: bool) -> Self {
        Self {
            login_requested: Arc::new(AtomicBool::new(login_requested)),
            ..Self::default()
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

    fn is_current_clipboard_generation(&self, generation: u64) -> bool {
        self.clipboard_generation.load(Ordering::Acquire) == generation
    }

    fn publish(&self, mut snapshot: HelperSnapshot) -> Result<(), String> {
        validate_snapshot(&snapshot)?;
        let mut current = self
            .snapshot
            .write()
            .map_err(|_| "snapshot_lock".to_string())?;
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
        Ok(())
    }

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

    let listener = UnixListener::bind(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(listener)
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

fn snapshot_response(id: &str, snapshot: &HelperSnapshot) -> Value {
    let mut body = serde_json::to_value(snapshot).unwrap_or_else(|_| json!({ "state": "error" }));
    if let Value::Object(map) = &mut body {
        map.insert("ok".into(), json!(true));
    }
    envelope(id, body)
}

fn read_clipboard_bounded() -> io::Result<Option<Vec<u8>>> {
    let mut child = Command::new(WL_PASTE)
        .arg("--no-newline")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let mut bytes = Vec::new();
    child
        .stdout
        .take()
        .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "clipboard stdout unavailable"))?
        .take(MAX_CLIPBOARD_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_CLIPBOARD_BYTES {
        let _ = child.kill();
        let _ = child.wait();
        return Ok(None);
    }
    if !child.wait()?.success() {
        return Ok(None);
    }
    Ok(Some(bytes))
}

fn copy_to_clipboard(state: &HelperState, code: &str) -> io::Result<()> {
    let mut child = Command::new(WL_COPY).stdin(Stdio::piped()).spawn()?;
    child
        .stdin
        .take()
        .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "clipboard stdin unavailable"))?
        .write_all(code.as_bytes())?;
    if !child.wait()?.success() {
        return Err(io::Error::other("wl-copy failed"));
    }

    let expiration = state.next_clipboard_generation();
    let state = state.clone();
    let code = code.as_bytes().to_vec();
    thread::spawn(move || {
        thread::sleep(CLIPBOARD_TTL);
        if !state.is_current_clipboard_generation(expiration) {
            return;
        }
        if let Ok(Some(current)) = read_clipboard_bounded() {
            if current == code && state.is_current_clipboard_generation(expiration) {
                let _ = Command::new(WL_COPY).arg("--clear").status();
            }
        }
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
        "status" => {
            let snapshot = state
                .snapshot
                .read()
                .expect("snapshot lock poisoned")
                .clone();
            envelope(
                &request.id,
                json!({
                    "ok": true,
                    "state": snapshot.state,
                    "locked": snapshot.locked,
                    "synced": snapshot.synced,
                    "account": snapshot.account,
                    "generation": snapshot.generation,
                    "count": snapshot.entries.len(),
                }),
            )
        }
        "snapshot" => {
            let snapshot = state
                .snapshot
                .read()
                .expect("snapshot lock poisoned")
                .clone();
            snapshot_response(&request.id, &snapshot)
        }
        "copy" => {
            let Some(item_id) = request.item_id.filter(|id| valid_id(id)) else {
                return envelope(
                    &request.id,
                    json!({ "ok": false, "error": "invalid_item_id" }),
                );
            };
            let snapshot = state.snapshot.read().expect("snapshot lock poisoned");
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
            let mut snapshot = state.snapshot.write().expect("snapshot lock poisoned");
            state.manual_locked.store(true, Ordering::Release);
            snapshot.locked = true;
            snapshot.state = "locked".into();
            snapshot.entries.clear();
            snapshot.generation = snapshot.generation.saturating_add(1);
            envelope(
                &request.id,
                json!({ "ok": true, "state": "locked", "locked": true, "generation": snapshot.generation }),
            )
        }
        "unlock" => {
            state.unlock();
            let snapshot = state.snapshot.read().expect("snapshot lock poisoned");
            envelope(
                &request.id,
                json!({
                    "ok": true,
                    "state": snapshot.state,
                    "locked": false,
                    "generation": snapshot.generation,
                }),
            )
        }
        _ => envelope(
            &request.id,
            json!({ "ok": false, "error": "unsupported_operation" }),
        ),
    }
}

fn handle_client(state: HelperState, mut stream: UnixStream) -> io::Result<()> {
    stream.set_read_timeout(Some(CLIENT_TIMEOUT))?;
    stream.set_write_timeout(Some(CLIENT_TIMEOUT))?;
    if peer_uid(&stream)? != unsafe { libc::geteuid() } {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "peer uid mismatch",
        ));
    }

    let mut line = String::new();
    let read = BufReader::new(stream.try_clone()?)
        .take(MAX_REQUEST_BYTES + 1)
        .read_line(&mut line)?;
    if read == 0 || read as u64 > MAX_REQUEST_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "request too large",
        ));
    }
    let request: HelperRequest = serde_json::from_str(line.trim_end())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid json"))?;
    let mut encoded = serde_json::to_vec(&handle_request(&state, request))?;
    encoded.push(b'\n');
    if encoded.len() > MAX_RESPONSE_BYTES {
        encoded = b"{\"v\":1,\"id\":\"\",\"ok\":false,\"error\":\"response_too_large\"}\n".to_vec();
    }
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

    #[test]
    fn login_request_latch_is_consumed_once() {
        let state = HelperState::with_login_requested(true);
        assert!(state.take_login_request());
        assert!(!state.take_login_request());
        state.request_login();
        assert!(state.take_login_request());
    }

    #[test]
    fn clipboard_generation_renews_expiration() {
        let state = HelperState::default();
        let first = state.next_clipboard_generation();
        let second = state.next_clipboard_generation();
        assert!(!state.is_current_clipboard_generation(first));
        assert!(state.is_current_clipboard_generation(second));
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
        assert_eq!(unlock_response["ok"], true);
        state.publish(republished).unwrap();
        let snapshot = state.snapshot.read().unwrap();
        assert!(!snapshot.locked);
        assert_eq!(snapshot.state, "ready");
        assert_eq!(snapshot.entries.len(), 1);
        assert_eq!(snapshot.generation, 4);
    }
}
