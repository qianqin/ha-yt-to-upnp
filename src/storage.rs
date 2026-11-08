//! Where persistent files live and how they are written.
//!
//! `YTCAST_DATA_DIR` moves config and the nsig cache into one directory
//! (e.g. `/data` in a container). When it is unset, the historical per-user
//! locations are used.
//!
//! Path resolution takes the env value as a parameter so it can be tested
//! without mutating the process environment.

use std::io;
use std::path::{Path, PathBuf};

pub const DATA_DIR_ENV: &str = "YTCAST_DATA_DIR";

/// The `YTCAST_DATA_DIR` value, if set and non-empty.
pub fn data_dir_env() -> Option<PathBuf> {
    normalize_data_dir(std::env::var(DATA_DIR_ENV).ok())
}

/// An empty `YTCAST_DATA_DIR` means "unset".
pub fn normalize_data_dir(value: Option<String>) -> Option<PathBuf> {
    value.filter(|v| !v.is_empty()).map(PathBuf::from)
}

/// A relative data dir resolves against the working directory, which is
/// rarely what a service wants; it is still used, but worth a startup warning.
pub fn warn_if_relative_data_dir(dir: &Path) {
    if !dir.is_absolute() {
        tracing::warn!(
            "{}={} is not an absolute path; it resolves against the working directory",
            DATA_DIR_ENV,
            dir.display()
        );
    }
}

/// Directory for the config file.
pub fn config_dir(data_dir: Option<&Path>) -> Option<PathBuf> {
    match data_dir {
        Some(d) => Some(d.to_path_buf()),
        None => dirs::config_dir(),
    }
}

/// Directory for the preprocessed nsig player cache.
pub fn nsig_cache_dir(data_dir: Option<&Path>) -> Option<PathBuf> {
    match data_dir {
        Some(d) => Some(d.join("nsig-cache")),
        None => dirs::cache_dir().map(|d| d.join("ytcast")),
    }
}

/// Create `dir` and any missing parents; newly created directories are 0700.
/// Existing directories keep their permissions.
pub fn create_private_dir_all(dir: &Path) -> io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)
}

/// Atomically write `contents` to `path` as a 0600 file.
///
/// The temp file is created with 0600 before any byte is written, then renamed
/// over `path`, so the secret is never readable by others — also when `path`
/// already exists with looser permissions.
pub fn write_private_atomic(path: &Path, contents: &[u8]) -> io::Result<()> {
    use std::io::Write;

    if let Some(parent) = path.parent() {
        create_private_dir_all(parent)?;
    }

    let mut tmp_name = path.as_os_str().to_owned();
    tmp_name.push(".tmp");
    let tmp = PathBuf::from(tmp_name);

    // A leftover temp file would keep its old mode; mode() only applies on create.
    match std::fs::remove_file(&tmp) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }

    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(&tmp)?;
    file.write_all(contents)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&tmp, path)?;
    sync_parent_dir(path);
    Ok(())
}

/// Persist the rename itself. Best-effort: the file content is already
/// synced, so a failure here only weakens crash durability.
#[cfg(unix)]
fn sync_parent_dir(path: &Path) {
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    if let Err(e) = std::fs::File::open(parent).and_then(|d| d.sync_all()) {
        tracing::debug!("fsync of {} failed: {}", parent.display(), e);
    }
}

#[cfg(not(unix))]
fn sync_parent_dir(_path: &Path) {}

/// Warn (path only, never contents) if a secret file is readable by group or
/// others. Startup is not refused.
pub fn warn_if_loose_permissions(path: &Path) {
    if is_group_or_world_readable(path) {
        tracing::warn!(
            "{} is readable by group/others; restrict it with: chmod 600 {}",
            path.display(),
            path.display()
        );
    }
}

#[cfg(unix)]
fn is_group_or_world_readable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.permissions().mode() & 0o044 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_group_or_world_readable(_path: &Path) -> bool {
    false
}

#[cfg(test)]
#[allow(dead_code)] // shared by tests in both the lib and bin crates
pub(crate) mod test_util {
    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    /// A fresh, empty temp directory removed on drop.
    pub struct TempDir(pub PathBuf);

    impl TempDir {
        pub fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "ytcast-test-{}-{}-{}",
                std::process::id(),
                name,
                uuid::Uuid::new_v4()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }

        pub fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[cfg(unix)]
    pub fn mode(path: &std::path::Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[derive(Clone, Default)]
    pub struct LogBuf(Arc<Mutex<Vec<u8>>>);

    impl LogBuf {
        pub fn contents(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    impl Write for LogBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Run `f` with a thread-local subscriber and return everything it logged.
    pub fn capture_logs(f: impl FnOnce()) -> String {
        let buf = LogBuf::default();
        let writer = buf.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(subscriber, f);
        buf.contents()
    }
}

#[cfg(test)]
mod tests {
    use super::test_util::*;
    use super::*;

    #[test]
    fn data_dir_unset_keeps_historical_dirs() {
        assert_eq!(config_dir(None), dirs::config_dir());
        assert_eq!(nsig_cache_dir(None), dirs::cache_dir().map(|d| d.join("ytcast")));
    }

    #[test]
    fn data_dir_set_moves_everything_under_it() {
        let data = Some(Path::new("/data"));
        assert_eq!(config_dir(data), Some(PathBuf::from("/data")));
        assert_eq!(nsig_cache_dir(data), Some(Path::new("/data").join("nsig-cache")));
    }

    #[test]
    fn empty_data_dir_is_unset() {
        assert_eq!(normalize_data_dir(None), None);
        assert_eq!(normalize_data_dir(Some(String::new())), None);
        assert_eq!(normalize_data_dir(Some("/data".into())), Some(PathBuf::from("/data")));
        assert_eq!(
            normalize_data_dir(Some("relative/dir".into())),
            Some(PathBuf::from("relative/dir"))
        );
    }

    #[test]
    fn relative_data_dir_warns_absolute_does_not() {
        let logs = capture_logs(|| warn_if_relative_data_dir(Path::new("relative/dir")));
        assert!(logs.contains("WARN"), "{logs}");
        assert!(logs.contains("relative/dir"), "{logs}");
        assert!(logs.contains(DATA_DIR_ENV), "{logs}");

        assert_eq!(capture_logs(|| warn_if_relative_data_dir(Path::new("/data"))), "");
    }

    #[test]
    fn write_private_atomic_creates_file_and_dirs() {
        let tmp = TempDir::new("storage-write");
        let path = tmp.path().join("a/b/secret.json");
        write_private_atomic(&path, b"{}").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{}");
        #[cfg(unix)]
        {
            assert_eq!(mode(&path), 0o600);
            assert_eq!(mode(&tmp.path().join("a")), 0o700);
            assert_eq!(mode(&tmp.path().join("a/b")), 0o700);
        }
    }

    #[cfg(unix)]
    #[test]
    fn write_private_atomic_tightens_existing_loose_file_and_stale_tmp() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new("storage-overwrite");
        let path = tmp.path().join("secret.json");
        let stale_tmp = tmp.path().join("secret.json.tmp");
        for p in [&path, &stale_tmp] {
            std::fs::write(p, "old").unwrap();
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        write_private_atomic(&path, b"new").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
        assert_eq!(mode(&path), 0o600);
        assert!(!stale_tmp.exists());
    }

    #[cfg(unix)]
    #[test]
    fn existing_dirs_keep_their_mode() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new("storage-existing");
        std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        create_private_dir_all(tmp.path()).unwrap();
        assert_eq!(mode(tmp.path()), 0o755);
    }

    #[cfg(unix)]
    #[test]
    fn loose_permissions_warning_names_path_not_contents() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new("storage-warn");
        let path = tmp.path().join("secret.txt");
        std::fs::write(&path, "SUPER-SECRET-VALUE").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let logs = capture_logs(|| warn_if_loose_permissions(&path));
        assert!(logs.contains("WARN"), "{logs}");
        assert!(logs.contains(path.to_str().unwrap()), "{logs}");
        assert!(!logs.contains("SUPER-SECRET-VALUE"), "{logs}");

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(capture_logs(|| warn_if_loose_permissions(&path)), "");
    }
}
