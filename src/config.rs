use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

pub type Result<T> = std::result::Result<T, ConfigError>;

#[derive(Debug)]
pub enum ConfigError {
    Io(io::Error),
    Json(serde_json::Error),
    NoHomeDir,
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Io(e) => write!(f, "config io: {e}"),
            ConfigError::Json(e) => write!(f, "config json: {e}"),
            ConfigError::NoHomeDir => write!(f, "could not determine home directory"),
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ConfigError::Io(e) => Some(e),
            ConfigError::Json(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for ConfigError {
    fn from(e: io::Error) -> Self {
        ConfigError::Io(e)
    }
}

impl From<serde_json::Error> for ConfigError {
    fn from(e: serde_json::Error) -> Self {
        ConfigError::Json(e)
    }
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct Config {
    pub uuid: String,
    pub screen_id: String,
    /// Screen ID for the YouTube Music ("m" theme) Lounge session.
    #[serde(default)]
    pub screen_id_m: String,
}

impl Config {
    /// Load config from disk, or create a new one with defaults if the file
    /// does not exist.
    pub fn load() -> Result<Self> {
        Self::load_from(&Self::path()?)
    }

    fn load_from(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(&path) {
            Ok(data) => {
                crate::storage::warn_if_loose_permissions(path);
                let config: Config = serde_json::from_str(&data)?;
                tracing::info!("loaded config from {}", path.display());
                Ok(config)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let config = Config {
                    uuid: uuid::Uuid::new_v4().to_string(),
                    screen_id: String::new(),
                    screen_id_m: String::new(),
                };
                config.save_to(path)?;
                tracing::info!("created new config at {}", path.display());
                Ok(config)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Save config atomically (temp file + rename) as a private 0600 file.
    /// Prevents corruption on power loss (important for a Pi running 24/7).
    pub fn save(&self) -> Result<()> {
        self.save_to(&Self::path()?)
    }

    fn save_to(&self, path: &Path) -> Result<()> {
        let json = serde_json::to_string_pretty(self)?;
        crate::storage::write_private_atomic(path, json.as_bytes())?;
        Ok(())
    }

    /// Config file path: `$YTCAST_DATA_DIR/ytcast-lite.json`, else
    /// `~/.config/ytcast-lite.json`.
    fn path() -> Result<PathBuf> {
        Self::path_for(crate::storage::data_dir_env().as_deref())
    }

    fn path_for(data_dir: Option<&str>) -> Result<PathBuf> {
        let config_dir = crate::storage::config_dir(data_dir).ok_or(ConfigError::NoHomeDir)?;
        Ok(config_dir.join("ytcast-lite.json"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::test_util::*;

    #[test]
    fn path_unchanged_without_data_dir() {
        assert_eq!(
            Config::path_for(None).unwrap(),
            dirs::config_dir().unwrap().join("ytcast-lite.json")
        );
    }

    #[test]
    fn saved_under_data_dir_and_read_back() {
        let tmp = TempDir::new("config-roundtrip");
        let path = Config::path_for(Some(tmp.as_str())).unwrap();
        assert_eq!(path, tmp.path().join("ytcast-lite.json"));

        let config = Config {
            uuid: "u".into(),
            screen_id: "s".into(),
            screen_id_m: "m".into(),
        };
        config.save_to(&path).unwrap();
        let loaded = Config::load_from(&path).unwrap();
        assert_eq!(loaded.uuid, "u");
        assert_eq!(loaded.screen_id, "s");
        assert_eq!(loaded.screen_id_m, "m");
        #[cfg(unix)]
        assert_eq!(mode(&path), 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn loading_loose_config_warns_with_path() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new("config-loose");
        let path = tmp.path().join("ytcast-lite.json");
        std::fs::write(&path, r#"{"uuid":"u","screen_id":"SCREEN-ID-VALUE"}"#).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let mut loaded = None;
        let logs = capture_logs(|| loaded = Some(Config::load_from(&path).unwrap()));
        assert!(logs.contains("WARN"), "{logs}");
        assert!(logs.contains(path.to_str().unwrap()), "{logs}");
        assert!(!logs.contains("SCREEN-ID-VALUE"), "{logs}");
        assert_eq!(loaded.unwrap().screen_id, "SCREEN-ID-VALUE");
    }

    #[test]
    fn missing_config_is_created_privately() {
        let tmp = TempDir::new("config-create");
        let path = tmp.path().join("sub/ytcast-lite.json");
        let config = Config::load_from(&path).unwrap();
        assert!(!config.uuid.is_empty());
        assert!(path.exists());
        #[cfg(unix)]
        {
            assert_eq!(mode(&path), 0o600);
            assert_eq!(mode(&tmp.path().join("sub")), 0o700);
        }
    }
}
