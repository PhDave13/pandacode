//! The relay token and the device's X25519 private key, in a 0600 JSON file in the app data
//! dir. Never the keychain: macOS pins each item to the exact build that made it, and without
//! an Apple Team ID every update asked for the login password on every read.
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

const FILE: &str = "connect-secrets.json";

/// Serializes read-modify-write: sign-in, key creation and token refresh can overlap.
static WRITE: Mutex<()> = Mutex::new(());

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Secrets {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// The base64 X25519 private key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

/// What is stored. Missing means none; unreadable or corrupt also means none, logged.
pub(crate) fn read(dir: &Path) -> Secrets {
    let path = dir.join(FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Secrets::default(),
        Err(error) => {
            eprintln!("Could not read {}: {error}", path.display());
            return Secrets::default();
        }
    };
    serde_json::from_str(&text).unwrap_or_else(|error| {
        eprintln!("Ignoring corrupt {}: {error}", path.display());
        Secrets::default()
    })
}

/// Replaces the stored secrets with `change(current)`; an empty result deletes the file.
pub(crate) fn update(dir: &Path, change: impl FnOnce(Secrets) -> Secrets) -> crate::Result<()> {
    let _guard = WRITE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let next = change(read(dir));
    let path = dir.join(FILE);
    if next == Secrets::default() {
        return match std::fs::remove_file(&path) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(format!("Could not remove PandaCode Connect secrets: {error}")),
            _ => Ok(()),
        };
    }
    let text = serde_json::to_vec(&next).map_err(|e| e.to_string())?;
    write_atomic(dir, &path, &text).map_err(|e| format!("Could not save PandaCode Connect secrets: {e}"))
}

pub(crate) fn set_token(dir: &Path, token: &str) -> crate::Result<()> {
    update(dir, |s| Secrets { token: Some(token.to_owned()), ..s })
}

/// Sign-out: drops the token and keeps the device key.
pub(crate) fn remove_token(dir: &Path) -> crate::Result<()> {
    update(dir, |s| Secrets { token: None, ..s })
}

pub(crate) fn set_key(dir: &Path, key: &str) -> crate::Result<()> {
    update(dir, |s| Secrets { key: Some(key.to_owned()), ..s })
}

fn write_atomic(dir: &Path, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let temp = dir.join(format!("{FILE}.{}.tmp", uuid::Uuid::new_v4()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let written = options.open(&temp).and_then(|mut file| {
        file.write_all(bytes)?;
        file.sync_all()
    });
    let renamed = written.and_then(|_| std::fs::rename(&temp, path));
    if renamed.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    renamed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("pandacode-connect-secrets-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn round_trip_and_sign_out_keeps_the_key() {
        let dir = temp_dir();
        set_key(&dir, "k").unwrap();
        set_token(&dir, "t").unwrap();
        assert_eq!(read(&dir), Secrets { token: Some("t".into()), key: Some("k".into()) });
        remove_token(&dir).unwrap();
        assert_eq!(read(&dir), Secrets { token: None, key: Some("k".into()) });
        // Removing the last field removes the file.
        update(&dir, |_| Secrets::default()).unwrap();
        assert!(!dir.join(FILE).exists());
        remove_token(&dir).unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn missing_or_corrupt_means_none() {
        let dir = temp_dir();
        assert_eq!(read(&dir), Secrets::default());
        std::fs::write(dir.join(FILE), "{not json").unwrap();
        assert_eq!(read(&dir), Secrets::default());
        // A corrupt file is replaced on the next write.
        set_token(&dir, "t").unwrap();
        assert_eq!(read(&dir).token.as_deref(), Some("t"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn writes_are_atomic_and_private() {
        let dir = temp_dir();
        set_token(&dir, "t").unwrap();
        set_key(&dir, "k").unwrap();
        let names: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|entry| entry.unwrap().file_name()).collect();
        assert_eq!(names, vec![std::ffi::OsString::from(FILE)]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(dir.join(FILE)).unwrap().permissions().mode() & 0o777, 0o600);
        }
        let _ = std::fs::remove_dir_all(dir);
    }
}
