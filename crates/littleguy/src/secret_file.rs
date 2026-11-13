//! Redacted secret-file loader (`specs/0006`, `REQ-LG-CONT-01`).
//!
//! Opens a regular `0600` file owned by the effective UID with `O_NOFOLLOW | O_NONBLOCK`,
//! caps reads at [`MAX_SECRET_FILE_BYTES`], validates graphic ASCII after trailing
//! newline trimming, and redacts secret bytes from `Debug` and error formatting.

use std::fmt;
use std::io::{self, Read as _};
use std::os::unix::fs::MetadataExt as _;
use std::path::Path;

use rustix::fs::{Mode, OFlags, open};
use rustix::io::Errno;
use thiserror::Error;

/// Maximum size in bytes of a secret file read (`REQ-LG-CONT-01`).
pub const MAX_SECRET_FILE_BYTES: usize = 4 * 1024;

/// Errors returned while loading a [`SecretFile`].
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SecretFileError {
    /// Metadata lookup or file read failed.
    #[error("failed to read the secret file: {0}")]
    Stat(String),
    /// The path is a symlink, directory, FIFO, socket, or other non-regular file.
    #[error("the secret file must be a regular file and not a symlink")]
    NotRegular,
    /// The file has permission bits other than `0600`.
    #[error("the secret file has mode {mode:04o}; expected 0600")]
    Permissions {
        /// Observed permission bits (`mode & 0o7777`).
        mode: u32,
    },
    /// The file is owned by another UID.
    #[error("the secret file is owned by uid {owner_uid}; expected {expected_uid}")]
    Owner {
        /// File owner UID.
        owner_uid: u32,
        /// Expected effective UID.
        expected_uid: u32,
    },
    /// The file exceeds [`MAX_SECRET_FILE_BYTES`].
    #[error("the secret file exceeds {max_bytes} bytes")]
    TooLarge {
        /// Maximum permitted size in bytes.
        max_bytes: usize,
    },
    /// The file contents are empty, non-UTF-8, or contain non-graphic ASCII bytes.
    #[error("the secret file does not hold a valid graphic-ASCII secret")]
    Invalid,
}

/// Redacted graphic-ASCII secret read from a `0600` regular file (`REQ-LG-CONT-01`).
#[derive(Clone, PartialEq, Eq)]
pub struct SecretFile(String);

impl fmt::Debug for SecretFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretFile([REDACTED])")
    }
}

impl SecretFile {
    /// Loads a secret from `path` for the process effective UID (`0600`, regular file).
    ///
    /// # Errors
    ///
    /// Returns [`SecretFileError`] when `path` fails `O_NOFOLLOW` open, file-type, mode,
    /// effective-UID, size (`<= 4 KiB`), or graphic-ASCII checks.
    pub fn load(path: &Path) -> Result<Self, SecretFileError> {
        Self::load_for_uid(path, rustix::process::geteuid().as_raw())
    }

    /// Loads a secret from `path`, checking ownership against `expected_uid`.
    ///
    /// # Errors
    ///
    /// Returns [`SecretFileError`] when `path` fails `O_NOFOLLOW` open, file-type, mode,
    /// owner, size, or graphic-ASCII checks.
    pub fn load_for_uid(path: &Path, expected_uid: u32) -> Result<Self, SecretFileError> {
        let stat_err = |err: io::Error| SecretFileError::Stat(err.kind().to_string());
        let fd = match open(
            path,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK,
            Mode::empty(),
        ) {
            Ok(fd) => fd,
            Err(Errno::LOOP | Errno::ISDIR) => return Err(SecretFileError::NotRegular),
            Err(err) => return Err(stat_err(io::Error::from(err))),
        };
        let file = std::fs::File::from(fd);
        let meta = file.metadata().map_err(stat_err)?;
        if !meta.file_type().is_file() {
            return Err(SecretFileError::NotRegular);
        }
        let mode = meta.mode() & 0o7777;
        if mode != 0o600 {
            return Err(SecretFileError::Permissions { mode });
        }
        if meta.uid() != expected_uid {
            return Err(SecretFileError::Owner {
                owner_uid: meta.uid(),
                expected_uid,
            });
        }
        let max_u64 = u64::try_from(MAX_SECRET_FILE_BYTES).unwrap_or(u64::MAX - 1);
        if meta.len() > max_u64 {
            return Err(SecretFileError::TooLarge {
                max_bytes: MAX_SECRET_FILE_BYTES,
            });
        }
        let mut buf = Vec::with_capacity(256);
        (&file)
            .take(max_u64.saturating_add(1))
            .read_to_end(&mut buf)
            .map_err(stat_err)?;
        if buf.len() > MAX_SECRET_FILE_BYTES {
            return Err(SecretFileError::TooLarge {
                max_bytes: MAX_SECRET_FILE_BYTES,
            });
        }
        let raw = std::str::from_utf8(&buf).map_err(|_| SecretFileError::Invalid)?;
        let trimmed = raw.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() || !trimmed.bytes().all(|b| b.is_ascii_graphic()) {
            return Err(SecretFileError::Invalid);
        }
        Ok(Self(trimmed.to_owned()))
    }

    /// Borrows the loaded secret string.
    #[must_use]
    pub fn expose_secret(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    const RAW_SECRET: &str = "sk-live-secret-9988776655";

    fn write_0600(dir: &Path, name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        path
    }

    #[test]
    fn happy_path_loads_trims_newline_and_redacts_debug() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_0600(dir.path(), "key", format!("{RAW_SECRET}\r\n").as_bytes());
        let loaded = SecretFile::load(&path).unwrap();
        assert_eq!(loaded.expose_secret(), RAW_SECRET);
        let dbg = format!("{loaded:?}");
        assert_eq!(dbg, "SecretFile([REDACTED])");
        assert!(!dbg.contains(RAW_SECRET));
    }

    #[test]
    fn mode_other_than_0600_or_wrong_owner_refuses_and_redacts() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_0600(dir.path(), "key", RAW_SECRET.as_bytes());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let err = SecretFile::load(&path).unwrap_err();
        assert_eq!(err, SecretFileError::Permissions { mode: 0o644 });
        assert!(!err.to_string().contains(RAW_SECRET));

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let uid = rustix::process::geteuid().as_raw();
        let other_uid = uid.wrapping_add(1);
        let err = SecretFile::load_for_uid(&path, other_uid).unwrap_err();
        assert_eq!(
            err,
            SecretFileError::Owner {
                owner_uid: uid,
                expected_uid: other_uid,
            }
        );
        assert!(!err.to_string().contains(RAW_SECRET));
    }

    #[test]
    fn symlink_directory_fifo_missing_and_oversize_paths_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let target = write_0600(dir.path(), "real", RAW_SECRET.as_bytes());
        let link = dir.path().join("symlink");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert_eq!(SecretFile::load(&link), Err(SecretFileError::NotRegular));
        let subdir = dir.path().join("secret.dir");
        std::fs::create_dir(&subdir).unwrap();
        std::fs::set_permissions(&subdir, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(SecretFile::load(&subdir), Err(SecretFileError::NotRegular));
        std::fs::set_permissions(&subdir, std::fs::Permissions::from_mode(0o700)).unwrap();

        let fifo = dir.path().join("pipe");
        rustix::fs::mknodat(
            rustix::fs::CWD,
            &fifo,
            rustix::fs::FileType::Fifo,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
            0,
        )
        .unwrap();
        assert_eq!(SecretFile::load(&fifo), Err(SecretFileError::NotRegular));

        let missing = SecretFile::load(&dir.path().join("missing")).unwrap_err();
        assert!(matches!(missing, SecretFileError::Stat(_)));

        let big = vec![b'k'; MAX_SECRET_FILE_BYTES + 1];
        let big_path = write_0600(dir.path(), "big", &big);
        assert_eq!(
            SecretFile::load(&big_path),
            Err(SecretFileError::TooLarge {
                max_bytes: MAX_SECRET_FILE_BYTES,
            })
        );
    }

    #[test]
    fn empty_whitespace_and_non_utf8_contents_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        for (name, bytes) in [
            ("empty", b"".as_slice()),
            ("newline", b"\r\n".as_slice()),
            ("space", b"sk bad key".as_slice()),
            ("nonutf8", &[0xff, 0xfe]),
        ] {
            let path = write_0600(dir.path(), name, bytes);
            assert_eq!(SecretFile::load(&path), Err(SecretFileError::Invalid));
        }
    }
}
