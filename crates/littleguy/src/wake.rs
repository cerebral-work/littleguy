//! Native Cortex `lane-wake` Unix socket listener (`specs/0007`, `REQ-LG-WAKE-01..05`).
//!
//! Matches the `cortex-edge` `claude-channel` socket contract:
//! - Path `${CORTEX_STATE_DIR:-$HOME/.cortex}/lane-wake/<safe_pane>.sock` or explicit `--wake-socket`.
//! - Exclusive `<socket>.lock` `flock` during stale-listener probe and socket bind.
//! - Mode `0700` parent directory and mode `0600` Unix socket.
//! - Recorded `st_ino` check before unlinking on drop so a replacement socket is never removed.
//! - `SO_PEERCRED` same-UID verification (`err not your user\n` on mismatch).
//! - Bounded `4_096`-byte / `5 s` newline JSON read (`{"message_id":"<uuid>"}`), replying `ok\n` or `err\n`.

use std::future::Future;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use littleguy_core::WakeMessageId;
use rustix::fs::{FlockOperation, Mode, OFlags, flock, lstat, open, stat};
use serde::Deserialize;
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Notify;

/// Maximum bytes read from one newline-delimited JSON wake frame (`REQ-LG-WAKE-03`).
pub const MAX_WAKE_LINE_BYTES: usize = 4_096;

/// Per-connection read and write timeout on the Unix wake socket.
pub const WAKE_IO_TIMEOUT: Duration = Duration::from_secs(5);

/// Connect probe timeout when checking whether an existing socket has a live listener.
pub const WAKE_CONNECT_PROBE_TIMEOUT: Duration = Duration::from_millis(500);

/// Reply written after accepting a valid [`WakeMessageId`].
pub const WAKE_OK_REPLY: &[u8] = b"ok\n";

/// Reply written when a wake frame fails JSON or UUID validation, or delivery fails.
pub const WAKE_ERR_REPLY: &[u8] = b"err\n";

/// Reply written when `SO_PEERCRED` reports a peer UID other than `expected_uid`.
pub const WAKE_WRONG_USER_REPLY: &[u8] = b"err not your user\n";

/// Errors returned while resolving, binding, or parsing a Cortex wake channel.
#[derive(Debug, Error)]
pub enum WakeError {
    /// Neither an explicit `--wake-socket` nor `HERDR_PANE_ID` was provided when wake is enabled.
    #[error("cortex wake requires --wake-socket or HERDR_PANE_ID in the environment")]
    MissingPaneRef,
    /// Neither `CORTEX_STATE_DIR` nor `HOME` is set to derive the default `lane-wake` directory.
    #[error("cortex wake requires CORTEX_STATE_DIR or HOME to locate lane-wake")]
    MissingStateDir,
    /// The pane identifier is empty or contains non-graphic or traversal characters.
    #[error("invalid pane reference `{0}` for lane-wake socket")]
    InvalidPaneRef(String),
    /// The target socket path has no parent directory or file name.
    #[error("invalid wake socket path `{0}`")]
    InvalidSocketPath(String),
    /// Another live listener already answers on the wake socket.
    #[error("another wake listener already answers on {0}")]
    AlreadyListening(PathBuf),
    /// An existing filesystem entry at the socket path is a symlink or non-socket file.
    #[error("existing path {0} is not a Unix domain socket")]
    NotSocket(PathBuf),
    /// Acquiring the exclusive `<socket>.lock` file lock timed out.
    #[error("timed out acquiring exclusive lock on {0}")]
    LockBusy(PathBuf),
    /// Creating directories, locking, probing, or binding the Unix socket failed.
    #[error("wake socket I/O failed on {path}: {message}")]
    Io {
        /// Affected filesystem path.
        path: PathBuf,
        /// Operating system error summary.
        message: String,
    },
    /// A wake line exceeded [`MAX_WAKE_LINE_BYTES`], failed JSON parsing, or held a non-UUID `message_id`.
    #[error("invalid wake frame: {0}")]
    InvalidFrame(String),
}

#[derive(Debug, Deserialize)]
struct RawWakeFrame {
    message_id: String,
}

/// Sanitizes `pane_ref` (`<session>/<pane_id>`) into the `<safe_pane>` stem used by `cortex-edge`.
///
/// Replaces `'/'` and `':'` with `'-'` (`estate/w5Z:pG` -> `estate-w5Z-pG`) and rejects empty,
/// dot-traversal, or non-graphic ASCII stems.
///
/// # Errors
///
/// Returns [`WakeError::InvalidPaneRef`] when `pane_ref` is empty or holds disallowed bytes.
pub fn safe_pane(pane_ref: &str) -> Result<String, WakeError> {
    let trimmed = pane_ref.trim();
    if trimmed.is_empty() {
        return Err(WakeError::InvalidPaneRef(pane_ref.to_owned()));
    }
    let mapped: String = trimmed
        .chars()
        .map(|ch| if matches!(ch, '/' | ':') { '-' } else { ch })
        .collect();
    let valid = !mapped.is_empty()
        && mapped != "."
        && mapped != ".."
        && !mapped.contains("..")
        && mapped
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
    if !valid {
        return Err(WakeError::InvalidPaneRef(pane_ref.to_owned()));
    }
    Ok(mapped)
}

/// Resolves `${CORTEX_STATE_DIR:-$HOME/.cortex}/lane-wake/<safe_pane>.sock` for a `herdr` lane.
///
/// Missing or empty `session` defaults to `"default"`, matching `cortex-edge` and `cortex-mcp`.
///
/// # Errors
///
/// Returns [`WakeError`] when `pane_id` is missing or invalid, or neither `state_dir` nor `home` is set.
pub fn default_lane_wake_socket(
    state_dir: Option<&Path>,
    home: Option<&Path>,
    session: Option<&str>,
    pane_id: Option<&str>,
) -> Result<PathBuf, WakeError> {
    let pane = pane_id
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or(WakeError::MissingPaneRef)?;
    let sess = session
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("default");
    let stem = safe_pane(&format!("{sess}/{pane}"))?;
    let base = match state_dir.filter(|p| !p.as_os_str().is_empty()) {
        Some(dir) => dir.to_path_buf(),
        None => home
            .filter(|p| !p.as_os_str().is_empty())
            .map(|h| h.join(".cortex"))
            .ok_or(WakeError::MissingStateDir)?,
    };
    Ok(base.join("lane-wake").join(format!("{stem}.sock")))
}

/// Parses and validates one newline-terminated JSON wake frame (`{"message_id":"<uuid>"}`).
///
/// # Errors
///
/// Returns [`WakeError::InvalidFrame`] when `line` exceeds [`MAX_WAKE_LINE_BYTES`], is not valid
/// JSON with a string `message_id`, or `message_id` is not a hyphenated hexadecimal UUID.
pub fn parse_wake_line(line: &str) -> Result<WakeMessageId, WakeError> {
    if line.len() > MAX_WAKE_LINE_BYTES {
        return Err(WakeError::InvalidFrame(format!(
            "line exceeds {MAX_WAKE_LINE_BYTES} bytes"
        )));
    }
    let raw: RawWakeFrame = serde_json::from_str(line.trim())
        .map_err(|err| WakeError::InvalidFrame(err.to_string()))?;
    WakeMessageId::new(raw.message_id).map_err(|err| WakeError::InvalidFrame(err.to_string()))
}

/// Builds the fixed safe assistant prompt for a validated [`WakeMessageId`].
///
/// Zero peer-controlled text enters this string.
#[must_use]
pub fn cortex_wake_notice(message_id: &WakeMessageId) -> String {
    format!("New cortex mesh message {message_id}. Read it with cortex__mesh_read.")
}

/// Exclusive Unix domain socket listener for Cortex `lane-wake` notifications.
#[derive(Debug)]
pub struct WakeListener {
    listener: UnixListener,
    path: PathBuf,
    expected_uid: u32,
    inode: u64,
}

impl WakeListener {
    /// Binds `path` restricted to the current effective UID (`geteuid()`).
    ///
    /// # Errors
    ///
    /// Returns [`WakeError`] when another live listener holds `path`, `path` is not a socket,
    /// or lock/bind I/O fails.
    pub async fn bind(path: &Path) -> Result<Self, WakeError> {
        Self::bind_for_uid(path, rustix::process::geteuid().as_raw()).await
    }

    /// Binds `path` restricted to connections whose `SO_PEERCRED` UID equals `expected_uid`.
    ///
    /// # Errors
    ///
    /// Returns [`WakeError`] when another live listener holds `path`, `path` is not a socket,
    /// or lock/bind I/O fails.
    pub async fn bind_for_uid(path: &Path, expected_uid: u32) -> Result<Self, WakeError> {
        let lock_file = lock_path(path)?;
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        if !parent.exists() {
            std::fs::create_dir_all(parent).map_err(|err| WakeError::Io {
                path: parent.to_path_buf(),
                message: err.to_string(),
            })?;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)).map_err(
                |err| WakeError::Io {
                    path: parent.to_path_buf(),
                    message: err.to_string(),
                },
            )?;
        }

        let _lock_guard = acquire_lock(&lock_file).await?;
        match std::fs::symlink_metadata(path) {
            Ok(meta) => {
                if !meta.file_type().is_socket() {
                    return Err(WakeError::NotSocket(path.to_path_buf()));
                }
                if let Ok(Ok(_live)) =
                    tokio::time::timeout(WAKE_CONNECT_PROBE_TIMEOUT, UnixStream::connect(path))
                        .await
                {
                    return Err(WakeError::AlreadyListening(path.to_path_buf()));
                }
                std::fs::remove_file(path).map_err(|err| WakeError::Io {
                    path: path.to_path_buf(),
                    message: err.to_string(),
                })?;
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(WakeError::Io {
                    path: path.to_path_buf(),
                    message: err.to_string(),
                });
            }
        }

        let listener = UnixListener::bind(path).map_err(|err| WakeError::Io {
            path: path.to_path_buf(),
            message: err.to_string(),
        })?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(|err| {
            let _ = std::fs::remove_file(path);
            WakeError::Io {
                path: path.to_path_buf(),
                message: err.to_string(),
            }
        })?;
        let inode = stat(path)
            .map_err(|err| {
                let _ = std::fs::remove_file(path);
                WakeError::Io {
                    path: path.to_path_buf(),
                    message: err.to_string(),
                }
            })?
            .st_ino;

        Ok(Self {
            listener,
            path: path.to_path_buf(),
            expected_uid,
            inode,
        })
    }

    /// Bound filesystem path of this wake socket.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Expected peer UID enforced on every accepted Unix stream.
    #[must_use]
    pub const fn expected_uid(&self) -> u32 {
        self.expected_uid
    }

    /// Accepts connections until `stop` completes, invoking `deliver` for each valid [`WakeMessageId`].
    pub async fn serve_until<F, Fut>(self, deliver: F, stop: impl Future<Output = ()> + Send)
    where
        F: Fn(WakeMessageId) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = bool> + Send,
    {
        let deliver = Arc::new(deliver);
        let expected_uid = self.expected_uid;
        tokio::pin!(stop);
        loop {
            tokio::select! {
                () = &mut stop => break,
                accepted = self.listener.accept() => match accepted {
                    Ok((stream, _)) => {
                        let deliver = Arc::clone(&deliver);
                        tokio::spawn(async move {
                            let _ = handle_connection(stream, expected_uid, deliver).await;
                        });
                    }
                    Err(error) => {
                        tracing::warn!(%error, path = %self.path.display(), "wake socket accept failed");
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                }
            }
        }
    }

    /// Accepts connections until `stop` completes, coalescing valid wakes onto `notify`.
    pub async fn notify_until(self, notify: Arc<Notify>, stop: impl Future<Output = ()> + Send) {
        self.serve_until(
            move |_id| {
                let notify = Arc::clone(&notify);
                async move {
                    notify.notify_one();
                    true
                }
            },
            stop,
        )
        .await;
    }
}

impl Drop for WakeListener {
    fn drop(&mut self) {
        if let Ok(meta) = lstat(&self.path)
            && meta.st_ino == self.inode
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn lock_path(socket_path: &Path) -> Result<PathBuf, WakeError> {
    let name = socket_path
        .file_name()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| WakeError::InvalidSocketPath(socket_path.display().to_string()))?;
    let parent = socket_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    Ok(parent.join(format!("{name}.lock")))
}

async fn acquire_lock(path: &Path) -> Result<rustix::fd::OwnedFd, WakeError> {
    let fd = open(
        path,
        OFlags::CREATE | OFlags::RDWR | OFlags::NOFOLLOW,
        Mode::from_raw_mode(0o600),
    )
    .map_err(|err| WakeError::Io {
        path: path.to_path_buf(),
        message: err.to_string(),
    })?;
    for _ in 0..50 {
        match flock(&fd, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => return Ok(fd),
            Err(rustix::io::Errno::WOULDBLOCK) => {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Err(err) => {
                return Err(WakeError::Io {
                    path: path.to_path_buf(),
                    message: err.to_string(),
                });
            }
        }
    }
    Err(WakeError::LockBusy(path.to_path_buf()))
}

async fn handle_connection<F, Fut>(
    mut stream: UnixStream,
    expected_uid: u32,
    deliver: Arc<F>,
) -> std::io::Result<()>
where
    F: Fn(WakeMessageId) -> Fut + Send + Sync,
    Fut: Future<Output = bool> + Send,
{
    let uid = stream.peer_cred()?.uid();
    if uid != expected_uid {
        let _ =
            tokio::time::timeout(WAKE_IO_TIMEOUT, stream.write_all(WAKE_WRONG_USER_REPLY)).await;
        let _ = stream.shutdown().await;
        let mut sink = tokio::io::sink();
        let mut tail = (&mut stream).take(8_192);
        let _ = tokio::time::timeout(
            Duration::from_millis(100),
            tokio::io::copy(&mut tail, &mut sink),
        )
        .await;
        return Ok(());
    }
    let (read_half, mut write_half) = stream.into_split();
    let limit = u64::try_from(MAX_WAKE_LINE_BYTES).unwrap_or(u64::MAX);
    let mut reader = BufReader::new(read_half.take(limit.saturating_add(1)));
    let mut line = String::new();
    let read_res = tokio::time::timeout(WAKE_IO_TIMEOUT, reader.read_line(&mut line)).await;
    let reply = match read_res {
        Ok(Ok(n)) if n > 0 => match parse_wake_line(&line) {
            Ok(id) => {
                if deliver(id).await {
                    WAKE_OK_REPLY
                } else {
                    WAKE_ERR_REPLY
                }
            }
            Err(_) => WAKE_ERR_REPLY,
        },
        _ => WAKE_ERR_REPLY,
    };
    let _ = tokio::time::timeout(WAKE_IO_TIMEOUT, write_half.write_all(reply)).await;
    let _ = write_half.shutdown().await;
    let mut raw_read = reader.into_inner().into_inner().take(8_192);
    let mut sink = tokio::io::sink();
    let _ = tokio::time::timeout(
        Duration::from_millis(100),
        tokio::io::copy(&mut raw_read, &mut sink),
    )
    .await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tokio::sync::oneshot;

    const SAMPLE_UUID: &str = "AB570176-0000-4000-8000-000000000004";

    async fn send_raw(path: &Path, payload: &[u8]) -> String {
        let mut stream = UnixStream::connect(path).await.unwrap();
        stream.write_all(payload).await.unwrap();
        stream.shutdown().await.unwrap();
        let mut out = String::new();
        stream.read_to_string(&mut out).await.unwrap();
        out
    }

    #[test]
    fn safe_pane_and_default_socket_match_cortex_edge_convention() {
        assert_eq!(safe_pane("estate/w5Z:pG").unwrap(), "estate-w5Z-pG");
        assert_eq!(
            safe_pane("ceres/littleman-pool-1").unwrap(),
            "ceres-littleman-pool-1"
        );
        assert!(safe_pane("").is_err());
        assert!(safe_pane("..").is_err());
        assert!(safe_pane("estate/../etc").is_err());
        assert!(safe_pane("bad pane").is_err());

        let sock = default_lane_wake_socket(
            Some(Path::new("/var/run/cortex")),
            Some(Path::new("/home/mark")),
            Some("estate"),
            Some("w5Z:pG"),
        )
        .unwrap();
        assert_eq!(
            sock,
            PathBuf::from("/var/run/cortex/lane-wake/estate-w5Z-pG.sock")
        );

        let fallback =
            default_lane_wake_socket(None, Some(Path::new("/home/mark")), None, Some("p1"))
                .unwrap();
        assert_eq!(
            fallback,
            PathBuf::from("/home/mark/.cortex/lane-wake/default-p1.sock")
        );

        assert!(matches!(
            default_lane_wake_socket(None, Some(Path::new("/home/mark")), None, None),
            Err(WakeError::MissingPaneRef)
        ));
        assert!(matches!(
            default_lane_wake_socket(None, None, Some("estate"), Some("p1")),
            Err(WakeError::MissingStateDir)
        ));
    }

    #[test]
    fn valid_wake_line_normalizes_uuid_and_rejects_injection() {
        let id = parse_wake_line(&format!("{{\"message_id\":\"{SAMPLE_UUID}\"}}\n")).unwrap();
        assert_eq!(id.as_str(), "ab570176-0000-4000-8000-000000000004");
        assert_eq!(
            cortex_wake_notice(&id),
            "New cortex mesh message ab570176-0000-4000-8000-000000000004. Read it with cortex__mesh_read."
        );

        // Benign extra fields from future cortex-edge versions do not break wake delivery,
        // and never leak into WakeMessageId.
        let extra = parse_wake_line(
            "{\"message_id\":\"11111111-2222-4333-8444-555555555555\",\"note\":\"ignored\"}\n",
        )
        .unwrap();
        assert_eq!(extra.as_str(), "11111111-2222-4333-8444-555555555555");

        for bad in [
            "",
            "{}",
            "{\"message_id\":\"not-a-uuid\"}",
            "{\"message_id\":\"ab570176-0000-4000-8000-000000000004; rm -rf /\"}",
            &format!(
                "{{\"message_id\":\"11111111-2222-4333-8444-555555555555\",\"pad\":\"{}\"}}",
                "x".repeat(MAX_WAKE_LINE_BYTES)
            ),
        ] {
            assert!(parse_wake_line(bad).is_err(), "accepted bad frame: {bad}");
        }
    }

    #[tokio::test]
    async fn second_listener_refuses_to_steal_live_socket_and_preserves_owner() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("lane-wake").join("estate-w5Z-pG.sock");
        let first = WakeListener::bind(&sock).await.unwrap();
        let mode = std::fs::symlink_metadata(&sock)
            .unwrap()
            .permissions()
            .mode()
            & 0o7777;
        assert_eq!(mode, 0o600);

        let err = WakeListener::bind(&sock).await.unwrap_err();
        assert!(matches!(err, WakeError::AlreadyListening(_)), "{err:?}");
        assert!(
            sock.exists(),
            "contending bind must never unlink live socket"
        );

        let received = Arc::new(Mutex::new(Vec::new()));
        let received_clone = Arc::clone(&received);
        let (stop_tx, stop_rx) = oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            first
                .serve_until(
                    move |id| {
                        received_clone.lock().unwrap().push(id);
                        async { true }
                    },
                    async move {
                        let _ = stop_rx.await;
                    },
                )
                .await;
        });

        let reply = send_raw(
            &sock,
            format!("{{\"message_id\":\"{SAMPLE_UUID}\"}}\n").as_bytes(),
        )
        .await;
        assert_eq!(reply, "ok\n");
        assert_eq!(
            received.lock().unwrap().as_slice(),
            &[WakeMessageId::new(SAMPLE_UUID).unwrap()]
        );

        let _ = stop_tx.send(());
        task.await.unwrap();
        assert!(!sock.exists(), "owner drop must unlink its own socket");
    }

    #[tokio::test]
    async fn stale_dead_socket_is_reclaimed_and_unlinked_on_drop_by_inode() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("stale.sock");
        {
            let raw = std::os::unix::net::UnixListener::bind(&sock).unwrap();
            drop(raw);
        }
        assert!(sock.exists(), "dead socket file remains on disk");

        let rebound = WakeListener::bind(&sock).await.unwrap();
        // Simulate external replacement with a brand-new socket at the same path:
        // dropping `rebound` must NOT unlink the replacement because its inode differs.
        std::fs::remove_file(&sock).unwrap();
        let replacement = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let replacement_ino = lstat(&sock).unwrap().st_ino;
        drop(rebound);
        assert!(
            sock.exists(),
            "drop must not unlink a socket with a newer inode"
        );
        assert_eq!(lstat(&sock).unwrap().st_ino, replacement_ino);
        drop(replacement);
    }

    #[tokio::test]
    async fn non_socket_path_is_refused_without_unlinking() {
        let dir = tempfile::tempdir().unwrap();
        let regular = dir.path().join("important.txt");
        std::fs::write(&regular, b"keep").unwrap();
        let err = WakeListener::bind(&regular).await.unwrap_err();
        assert!(matches!(err, WakeError::NotSocket(_)), "{err:?}");
        assert_eq!(std::fs::read(&regular).unwrap(), b"keep");
    }

    #[tokio::test]
    async fn wrong_uid_and_malformed_or_oversized_lines_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let wrong_uid_sock = dir.path().join("wrong-uid.sock");
        let my_uid = rustix::process::geteuid().as_raw();
        let other_uid = my_uid.wrapping_add(1);
        let listener = WakeListener::bind_for_uid(&wrong_uid_sock, other_uid)
            .await
            .unwrap();
        let (stop_tx, stop_rx) = oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            listener
                .serve_until(|_id| async { true }, async move {
                    let _ = stop_rx.await;
                })
                .await;
        });
        let reply = send_raw(
            &wrong_uid_sock,
            format!("{{\"message_id\":\"{SAMPLE_UUID}\"}}\n").as_bytes(),
        )
        .await;
        assert_eq!(reply, "err not your user\n");
        let _ = stop_tx.send(());
        task.await.unwrap();

        let sock = dir.path().join("malformed.sock");
        let listener2 = WakeListener::bind(&sock).await.unwrap();
        let (halt_sender, halt_receiver) = oneshot::channel::<()>();
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count_clone = Arc::clone(&count);
        let task2 = tokio::spawn(async move {
            listener2
                .serve_until(
                    move |_id| {
                        count_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        async { true }
                    },
                    async move {
                        let _ = halt_receiver.await;
                    },
                )
                .await;
        });

        assert_eq!(
            send_raw(&sock, b"{\"message_id\":\"not-uuid\"}\n").await,
            "err\n"
        );
        let oversized = format!(
            "{{\"message_id\":\"11111111-2222-4333-8444-555555555555\",\"x\":\"{}\"}}\n",
            "a".repeat(MAX_WAKE_LINE_BYTES + 64)
        );
        assert_eq!(send_raw(&sock, oversized.as_bytes()).await, "err\n");
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 0);

        let _ = halt_sender.send(());
        task2.await.unwrap();
    }
}
