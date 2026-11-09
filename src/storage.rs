//! XDG locations, atomic versioned JSON files with flock, write budget and value cache.
//! Mirrors hyprdeck-razer's dpi_presets/cache patterns. See docs/DESIGN.md §3 rule 4, §6.
//!
//! OWNER: foundation. Shared interface.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

const APP: &str = "hyprdeck-corsair";
/// Largest JSON file we read.
pub const MAX_JSON: u64 = 1024 * 1024;

/// `$VAR` when it is an absolute path, else `$HOME/<fallback>`; then `/hyprdeck-corsair`.
fn xdg_dir(
    var: Option<OsString>,
    home: Option<OsString>,
    fallback: &str,
    what: &str,
) -> Result<PathBuf> {
    let base = var
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            home.map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .map(|home| home.join(fallback))
        })
        .with_context(|| format!("HOME / {what} unavailable"))?;
    Ok(base.join(APP))
}

/// `$XDG_CONFIG_HOME/hyprdeck-corsair` (fallback ~/.config/hyprdeck-corsair).
pub fn config_dir() -> Result<PathBuf> {
    xdg_dir(
        std::env::var_os("XDG_CONFIG_HOME"),
        std::env::var_os("HOME"),
        ".config",
        "XDG_CONFIG_HOME",
    )
}
/// `$XDG_STATE_HOME/hyprdeck-corsair` (fallback ~/.local/state/hyprdeck-corsair).
pub fn state_dir() -> Result<PathBuf> {
    xdg_dir(
        std::env::var_os("XDG_STATE_HOME"),
        std::env::var_os("HOME"),
        ".local/state",
        "XDG_STATE_HOME",
    )
}
/// `$XDG_RUNTIME_DIR/hyprdeck-corsair` (created 0700).
pub fn runtime_dir() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        // SAFETY: getuid has no preconditions.
        .unwrap_or_else(|| PathBuf::from(format!("/run/user/{}", unsafe { libc::getuid() })));
    let dir = base.join(APP);
    ensure_private_dir(&dir)?;
    Ok(dir)
}

/// Create `dir` with mode 0700 if missing; refuse symlinks and directories owned by others.
fn ensure_private_dir(dir: &Path) -> Result<()> {
    match DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => return Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error).with_context(|| format!("Creating {}", dir.display())),
    }
    let metadata =
        fs::symlink_metadata(dir).with_context(|| format!("Inspecting {}", dir.display()))?;
    // SAFETY: getuid has no preconditions.
    if !metadata.is_dir() || metadata.uid() != unsafe { libc::getuid() } {
        bail!(
            "{} must be a directory owned by the current user",
            dir.display()
        );
    }
    if metadata.mode() & 0o077 != 0 {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Read a JSON file; Ok(None) if missing. Rejects non-regular files and files > 1 MiB.
pub fn read_json<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("Reading {}", path.display())),
    };
    if !metadata.file_type().is_file() || metadata.len() > MAX_JSON {
        bail!("{} must be a regular file of at most 1 MiB", path.display());
    }
    let mut bytes = Vec::new();
    File::open(path)?
        .take(MAX_JSON + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_JSON {
        bail!("{} must be at most 1 MiB", path.display());
    }
    let value = serde_json::from_slice(&bytes).with_context(|| {
        format!(
            "Malformed {}; existing file was not overwritten",
            path.display()
        )
    })?;
    Ok(Some(value))
}

/// Serialize the exact on-disk form under the reader's limit, including the trailing newline.
/// The writer rejects an oversized chunk before extending its buffer.
pub(crate) fn bounded_json<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    struct Bounded(Vec<u8>);
    impl Write for Bounded {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > MAX_JSON as usize - self.0.len() {
                return Err(std::io::Error::other(
                    "JSON exceeds the aggregate 1 MiB limit",
                ));
            }
            self.0.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut out = Bounded(Vec::new());
    serde_json::to_writer_pretty(&mut out, value)
        .context("Configuration exceeds the JSON limit")?;
    out.write_all(b"\n")?;
    Ok(out.0)
}

fn sibling(path: &Path, suffix: &str) -> Result<PathBuf> {
    let name = path
        .file_name()
        .context("JSON path has no file name")?
        .to_string_lossy();
    Ok(path.with_file_name(format!("{name}{suffix}")))
}

/// Exclusive flock on `<path>.lock`, read-modify-write via temp file + fsync + rename + dir fsync.
/// `update` receives the current value (or None) and returns the new value.
pub fn update_json<T: Serialize + DeserializeOwned>(
    path: &Path,
    update: impl FnOnce(Option<T>) -> Result<T>,
) -> Result<T> {
    let parent = path.parent().context("JSON path has no parent directory")?;
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(parent)
        .with_context(|| format!("Creating {}", parent.display()))?;
    // Requests are separate processes and may overlap: hold the lock across read/modify/rename.
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(sibling(path, ".lock")?)
        .with_context(|| format!("Opening lock for {}", path.display()))?;
    // A slow concurrent save cannot hold a request thread past its work deadline.
    let deadline = crate::bragi::bounded_deadline(std::time::Duration::from_secs(10));
    loop {
        crate::bragi::check_deadline(deadline)?;
        // SAFETY: plain nonblocking flock on a live fd; released when `lock` is dropped.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            break;
        }
        let error = std::io::Error::last_os_error();
        if !matches!(
            error.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
        ) {
            return Err(error).with_context(|| format!("Locking {}", path.display()));
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let current = read_json(path)?;
    crate::bragi::check_deadline(deadline)?;
    let value = update(current)?;
    crate::bragi::check_deadline(deadline)?;
    let bytes = bounded_json(&value)?;
    // A successful save must be readable by this exact storage API.
    let _: T = serde_json::from_slice(&bytes).context("Prospective JSON is not re-readable")?;
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let temporary = sibling(path, &format!(".{}-{nanos}.tmp", std::process::id()))?;
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC)
            .open(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        crate::bragi::check_deadline(deadline)?;
        fs::rename(&temporary, path)
            .with_context(|| format!("Replacing {} atomically", path.display()))?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result?;
    drop(lock);
    Ok(value)
}

const STORE_VERSION: u32 = 1;

#[derive(Debug, Default, Serialize, Deserialize)]
struct BudgetFile {
    version: u32,
    #[serde(default)]
    devices: BTreeMap<String, BudgetEntry>,
}

#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize)]
struct BudgetEntry {
    /// Days since the UNIX epoch (UTC).
    day: u64,
    count: u32,
}

fn utc_day() -> u64 {
    unix_now() / 86_400
}

fn check_version(version: u32, path: &Path) -> Result<()> {
    if version != STORE_VERSION {
        bail!(
            "Unsupported version {version} in {}; existing file was not overwritten",
            path.display()
        );
    }
    Ok(())
}

/// Persistent-write budget (500/UTC day/device). `record` errors when the budget is exhausted.
pub struct WriteBudget;

impl WriteBudget {
    pub const DAILY_LIMIT: u32 = 500;
    /// Count one persistent write for `device_id`; Err (user-facing text) if over the limit.
    pub fn record(device_id: &str) -> Result<u32> {
        Self::record_at(&state_dir()?.join("writes.json"), device_id, utc_day())
    }
    /// Writes recorded today for `device_id`.
    pub fn used_today(device_id: &str) -> u32 {
        state_dir()
            .map(|dir| Self::used_at(&dir.join("writes.json"), device_id, utc_day()))
            .unwrap_or(0)
    }
    fn record_at(path: &Path, device_id: &str, day: u64) -> Result<u32> {
        let file = update_json(path, |current: Option<BudgetFile>| {
            let mut file = current.unwrap_or(BudgetFile {
                version: STORE_VERSION,
                ..BudgetFile::default()
            });
            check_version(file.version, path)?;
            file.devices.retain(|_, entry| entry.day == day);
            let entry = file
                .devices
                .entry(device_id.to_string())
                .or_insert(BudgetEntry { day, count: 0 });
            if entry.count >= Self::DAILY_LIMIT {
                bail!(
                    "Daily limit of {} keyboard writes reached (protects the keyboard's flash memory) — \
                     try again after midnight UTC",
                    Self::DAILY_LIMIT
                );
            }
            entry.count += 1;
            Ok(file)
        })?;
        Ok(file.devices.get(device_id).map_or(0, |entry| entry.count))
    }
    fn used_at(path: &Path, device_id: &str, day: u64) -> u32 {
        read_json::<BudgetFile>(path)
            .ok()
            .flatten()
            .and_then(|file| file.devices.get(device_id).copied())
            .filter(|entry| entry.day == day)
            .map_or(0, |entry| entry.count)
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct CacheFile {
    version: u32,
    #[serde(default)]
    devices: BTreeMap<String, BTreeMap<String, serde_json::Value>>,
}

/// Small per-device value cache in the state dir (cache.json).
/// Keys used by the plugin: "sleep_minutes" (u32), "effect_raw" (hex string), "effect_read_at" (unix secs),
/// "polling_hz" (u32). Values are serde_json::Value.
pub struct ValueCache;

impl ValueCache {
    pub fn get(device_id: &str, key: &str) -> Option<serde_json::Value> {
        Self::get_at(&state_dir().ok()?.join("cache.json"), device_id, key)
    }
    pub fn set(device_id: &str, key: &str, value: serde_json::Value) -> Result<()> {
        Self::set_at(&state_dir()?.join("cache.json"), device_id, key, value)
    }
    fn get_at(path: &Path, device_id: &str, key: &str) -> Option<serde_json::Value> {
        let file = read_json::<CacheFile>(path).ok()??;
        if file.version != STORE_VERSION {
            return None;
        }
        file.devices.get(device_id)?.get(key).cloned()
    }
    fn set_at(path: &Path, device_id: &str, key: &str, value: serde_json::Value) -> Result<()> {
        update_json(path, |current: Option<CacheFile>| {
            let mut file = current.unwrap_or(CacheFile {
                version: STORE_VERSION,
                ..CacheFile::default()
            });
            check_version(file.version, path)?;
            file.devices
                .entry(device_id.to_string())
                .or_default()
                .insert(key.to_string(), value);
            Ok(file)
        })?;
        Ok(())
    }
}

/// Seconds since the UNIX epoch.
pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::tests::TempDir;
    use serde_json::json;

    #[test]
    fn xdg_directories_prefer_absolute_variables() {
        let dir = xdg_dir(
            Some("/x/cfg".into()),
            Some("/home/u".into()),
            ".config",
            "X",
        )
        .unwrap();
        assert_eq!(dir, PathBuf::from("/x/cfg/hyprdeck-corsair"));
        let dir = xdg_dir(
            Some("relative".into()),
            Some("/home/u".into()),
            ".config",
            "X",
        )
        .unwrap();
        assert_eq!(dir, PathBuf::from("/home/u/.config/hyprdeck-corsair"));
        let dir = xdg_dir(None, Some("/home/u".into()), ".local/state", "X").unwrap();
        assert_eq!(dir, PathBuf::from("/home/u/.local/state/hyprdeck-corsair"));
        assert!(xdg_dir(None, None, ".config", "X").is_err());
    }

    #[test]
    fn json_round_trip_is_atomic_and_rejects_bad_files() {
        let tmp = TempDir::new("storage-json");
        let path = tmp.0.join("nested/data.json");
        assert_eq!(read_json::<serde_json::Value>(&path).unwrap(), None);
        let value = update_json(&path, |current: Option<serde_json::Value>| {
            assert!(current.is_none());
            Ok(json!({"a": 1}))
        })
        .unwrap();
        assert_eq!(value, json!({"a": 1}));
        update_json(&path, |current: Option<serde_json::Value>| {
            let mut current = current.unwrap();
            current["b"] = json!(2);
            Ok(current)
        })
        .unwrap();
        assert_eq!(
            read_json::<serde_json::Value>(&path).unwrap(),
            Some(json!({"a": 1, "b": 2}))
        );
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        // No temporary files left behind.
        let names: Vec<String> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        assert!(
            names.iter().all(|name| !name.ends_with(".tmp")),
            "{names:?}"
        );
        // Update errors leave the file untouched.
        assert!(
            update_json(
                &path,
                |_: Option<serde_json::Value>| -> Result<serde_json::Value> { bail!("no") }
            )
            .is_err()
        );
        assert_eq!(
            read_json::<serde_json::Value>(&path).unwrap(),
            Some(json!({"a": 1, "b": 2}))
        );
        // Malformed files are never overwritten.
        fs::write(&path, b"{oops").unwrap();
        let error = update_json(&path, |_: Option<serde_json::Value>| Ok(json!({}))).unwrap_err();
        assert!(error.to_string().contains("not overwritten"), "{error}");
        assert_eq!(fs::read(&path).unwrap(), b"{oops");
        // Non-regular and oversized files are rejected.
        assert!(read_json::<serde_json::Value>(&tmp.0).is_err());
        let big = tmp.0.join("big.json");
        fs::write(&big, vec![b' '; (MAX_JSON + 1) as usize]).unwrap();
        assert!(read_json::<serde_json::Value>(&big).is_err());
        let link = tmp.0.join("link.json");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(read_json::<serde_json::Value>(&link).is_err());
    }

    #[test]
    fn write_budget_counts_per_device_and_day() {
        let tmp = TempDir::new("storage-budget");
        let path = tmp.0.join("writes.json");
        assert_eq!(WriteBudget::used_at(&path, "k65plus-a", 100), 0);
        assert_eq!(WriteBudget::record_at(&path, "k65plus-a", 100).unwrap(), 1);
        assert_eq!(WriteBudget::record_at(&path, "k65plus-a", 100).unwrap(), 2);
        assert_eq!(WriteBudget::record_at(&path, "k100-b", 100).unwrap(), 1);
        assert_eq!(WriteBudget::used_at(&path, "k65plus-a", 100), 2);
        assert_eq!(WriteBudget::used_at(&path, "k65plus-a", 101), 0);
        assert_eq!(
            WriteBudget::record_at(&path, "k65plus-a", 101).unwrap(),
            1,
            "new UTC day resets"
        );
        // Exhaust the budget.
        update_json(&path, |_: Option<BudgetFile>| {
            let mut file = BudgetFile {
                version: 1,
                ..BudgetFile::default()
            };
            file.devices.insert(
                "k65plus-a".into(),
                BudgetEntry {
                    day: 101,
                    count: WriteBudget::DAILY_LIMIT,
                },
            );
            Ok(file)
        })
        .unwrap();
        let error = WriteBudget::record_at(&path, "k65plus-a", 101).unwrap_err();
        assert!(error.to_string().contains("Daily limit of 500"), "{error}");
        assert_eq!(WriteBudget::used_at(&path, "k65plus-a", 101), 500);
        assert_eq!(WriteBudget::record_at(&path, "k100-b", 101).unwrap(), 1);
    }

    #[test]
    fn value_cache_is_per_device() {
        let tmp = TempDir::new("storage-cache");
        let path = tmp.0.join("cache.json");
        assert_eq!(ValueCache::get_at(&path, "dev", "sleep_minutes"), None);
        ValueCache::set_at(&path, "dev", "sleep_minutes", json!(10)).unwrap();
        ValueCache::set_at(&path, "dev", "effect_raw", json!("7ea00203")).unwrap();
        ValueCache::set_at(&path, "other", "sleep_minutes", json!(5)).unwrap();
        assert_eq!(
            ValueCache::get_at(&path, "dev", "sleep_minutes"),
            Some(json!(10))
        );
        assert_eq!(
            ValueCache::get_at(&path, "dev", "effect_raw"),
            Some(json!("7ea00203"))
        );
        assert_eq!(
            ValueCache::get_at(&path, "other", "sleep_minutes"),
            Some(json!(5))
        );
        assert_eq!(ValueCache::get_at(&path, "dev", "missing"), None);
        fs::write(&path, b"garbage").unwrap();
        assert_eq!(ValueCache::get_at(&path, "dev", "sleep_minutes"), None);
        assert!(ValueCache::set_at(&path, "dev", "x", json!(1)).is_err());
    }

    #[test]
    fn private_dirs_are_created_0700() {
        let tmp = TempDir::new("storage-runtime");
        let dir = tmp.0.join("rt");
        ensure_private_dir(&dir).unwrap();
        assert_eq!(fs::metadata(&dir).unwrap().mode() & 0o777, 0o700);
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        ensure_private_dir(&dir).unwrap();
        assert_eq!(fs::metadata(&dir).unwrap().mode() & 0o777, 0o700);
        let file = tmp.0.join("file");
        fs::write(&file, b"").unwrap();
        assert!(ensure_private_dir(&file).is_err());
    }
}

#[cfg(test)]
mod safety_regressions {
    use super::*;
    use crate::devices::tests::TempDir;
    use serde_json::json;

    #[test]
    fn oversized_prospective_json_preserves_editable_previous_file() {
        let dir = TempDir::new("bounded-json");
        let path = dir.0.join("live.json");
        update_json(&path, |_: Option<serde_json::Value>| {
            Ok(json!({"ok": true}))
        })
        .unwrap();
        let before = fs::read(&path).unwrap();
        let too_large = json!({"text": "x".repeat(MAX_JSON as usize)});
        assert!(update_json(&path, |_: Option<serde_json::Value>| Ok(too_large)).is_err());
        assert_eq!(fs::read(&path).unwrap(), before);
        update_json(&path, |_: Option<serde_json::Value>| Ok(json!({}))).unwrap();
        assert_eq!(
            read_json::<serde_json::Value>(&path).unwrap(),
            Some(json!({}))
        );
        assert!(
            !fs::read_dir(&dir.0).unwrap().any(|e| e
                .unwrap()
                .path()
                .extension()
                .is_some_and(|e| e == "tmp"))
        );
    }

    #[test]
    fn json_limit_includes_escaping_pretty_layout_and_newline() {
        let dir = TempDir::new("escaped-json");
        let path = dir.0.join("data.json");
        let escaped = "\t".repeat(MAX_JSON as usize / 2);
        assert!(update_json(&path, |_: Option<String>| Ok(escaped)).is_err());
        assert!(!path.exists());
        let at_limit = "x".repeat(MAX_JSON as usize - 3); // quotes + newline
        update_json(&path, |_: Option<String>| Ok(at_limit.clone())).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().len(), MAX_JSON);
        assert_eq!(read_json::<String>(&path).unwrap(), Some(at_limit));
    }
}

#[cfg(test)]
mod bounded_size_regressions {
    use super::*;
    use crate::devices::tests::TempDir;
    use serde_json::json;

    #[test]
    fn prospective_size_rejection_preserves_editable_file() {
        let tmp = TempDir::new("bounded-save");
        let path = tmp.0.join("data.json");
        update_json(&path, |_: Option<serde_json::Value>| Ok(json!({"old": 1}))).unwrap();
        let before = fs::read(&path).unwrap();
        // Escaping expands a small input far past the reader limit.
        let result = update_json(&path, |_: Option<serde_json::Value>| {
            Ok(json!({"text": "\n".repeat(MAX_JSON as usize)}))
        });
        assert!(result.is_err(), "oversized prospective JSON was accepted");
        assert_eq!(fs::read(&path).unwrap(), before);
        update_json(&path, |_: Option<serde_json::Value>| Ok(json!({}))).unwrap();
        assert_eq!(
            read_json::<serde_json::Value>(&path).unwrap(),
            Some(json!({}))
        );
        assert!(fs::read_dir(&tmp.0).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp")
        }));
    }
}
