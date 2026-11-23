//! Atomic TOML configuration loading, persistence, and async debounced saving.

use std::ffi::OsString;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tactus_protocol::{ConfigError, TactusConfig};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::{self, Instant};

use crate::log::{log_error, log_warn};

/// Default appliance configuration path (`SPEC.md §5`).
pub const DEFAULT_CONFIG_PATH: &str = "/var/lib/tactus/config.toml";

/// Environment variable overriding [`DEFAULT_CONFIG_PATH`] when `--config` is not supplied.
pub const CONFIG_ENV_VAR: &str = "TACTUS_CONFIG";

const SAVE_DEBOUNCE: Duration = Duration::from_millis(500);
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Error returned while loading, validating, or atomically writing `config.toml`.
#[derive(Debug)]
pub enum ConfigStoreError {
    Io {
        path: PathBuf,
        source: io::Error,
    },
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    Serialize(toml::ser::Error),
    Validation(ConfigError),
}

impl fmt::Display for ConfigStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(f, "I/O error on `{}`: {source}", path.display())
            }
            Self::Parse { path, source } => {
                write!(f, "failed to parse TOML `{}`: {source}", path.display())
            }
            Self::Serialize(err) => write!(f, "failed to serialize config TOML: {err}"),
            Self::Validation(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for ConfigStoreError {}

impl From<ConfigError> for ConfigStoreError {
    fn from(value: ConfigError) -> Self {
        Self::Validation(value)
    }
}

/// Resolve config path with precedence: `--config` > non-empty `TACTUS_CONFIG` > default path.
#[must_use]
pub fn resolve_config_path(
    cli_override: Option<PathBuf>,
    env_override: Option<OsString>,
) -> PathBuf {
    if let Some(path) = cli_override {
        return path;
    }
    if let Some(env_val) = env_override.filter(|v| !v.is_empty()) {
        return PathBuf::from(env_val);
    }
    PathBuf::from(DEFAULT_CONFIG_PATH)
}

/// Load and validate `config.toml`. Returns `(TactusConfig::default(), false)` if missing.
pub fn load_config(path: &Path) -> Result<(TactusConfig, bool), ConfigStoreError> {
    match fs::read_to_string(path) {
        Ok(raw) => {
            let config: TactusConfig =
                toml::from_str(&raw).map_err(|source| ConfigStoreError::Parse {
                    path: path.to_path_buf(),
                    source,
                })?;
            config.validate()?;
            Ok((config, true))
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok((TactusConfig::default(), false)),
        Err(source) => Err(ConfigStoreError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Atomically persist `config` to `path` via sibling temp file + `fsync` + `rename`.
pub fn save_config_atomic(path: &Path, config: &TactusConfig) -> Result<(), ConfigStoreError> {
    config.validate()?;
    let serialized = toml::to_string_pretty(config).map_err(ConfigStoreError::Serialize)?;

    let parent = match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    };
    fs::create_dir_all(parent).map_err(|source| ConfigStoreError::Io {
        path: parent.to_path_buf(),
        source,
    })?;

    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config.toml");

    let (tmp_path, mut tmp_file) = loop {
        let seq = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!(".{file_name}.tmp.{}.{seq}", std::process::id()));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => break (candidate, file),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(source) => {
                return Err(ConfigStoreError::Io {
                    path: candidate,
                    source,
                });
            }
        }
    };

    let write_res = (|| -> io::Result<()> {
        tmp_file.write_all(serialized.as_bytes())?;
        tmp_file.sync_all()?;
        Ok(())
    })();

    drop(tmp_file);

    if let Err(source) = write_res {
        let _ = fs::remove_file(&tmp_path);
        return Err(ConfigStoreError::Io {
            path: tmp_path,
            source,
        });
    }

    if let Err(source) = fs::rename(&tmp_path, path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(ConfigStoreError::Io {
            path: path.to_path_buf(),
            source,
        });
    }

    // Best-effort directory fsync so the renamed entry is durable across power loss.
    if let Ok(dir) = File::open(parent) {
        if let Err(err) = dir.sync_all() {
            log_warn!(
                "best-effort parent directory fsync failed on `{}`: {err}",
                parent.display()
            );
        }
    }

    Ok(())
}

enum PersistCommand {
    Schedule(Box<TactusConfig>),
    FlushAndStop(oneshot::Sender<()>),
}

/// Debounced background writer for `config.toml` running on the `tokio` runtime.
pub struct ConfigPersister {
    tx: mpsc::UnboundedSender<PersistCommand>,
    handle: Option<JoinHandle<()>>,
}

impl ConfigPersister {
    /// Spawn a debounced persistence worker targeting `path`.
    #[must_use]
    pub fn spawn(path: PathBuf) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let handle = tokio::spawn(run_persister(path, rx));
        Self {
            tx,
            handle: Some(handle),
        }
    }

    /// Queue an updated configuration for debounced atomic write.
    pub fn schedule_save(&self, config: TactusConfig) {
        let _ = self.tx.send(PersistCommand::Schedule(Box::new(config)));
    }

    /// Flush any pending configuration change immediately and join the writer task.
    pub async fn shutdown(&mut self) {
        let (ack_tx, ack_rx) = oneshot::channel();
        if self.tx.send(PersistCommand::FlushAndStop(ack_tx)).is_ok() {
            let _ = ack_rx.await;
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.await;
        }
    }
}

async fn run_persister(path: PathBuf, mut rx: mpsc::UnboundedReceiver<PersistCommand>) {
    let mut pending: Option<TactusConfig> = None;
    let mut deadline: Option<Instant> = None;

    loop {
        tokio::select! {
            cmd = rx.recv() => {
                match cmd {
                    Some(PersistCommand::Schedule(cfg)) => {
                        pending = Some(*cfg);
                        deadline = Some(Instant::now() + SAVE_DEBOUNCE);
                    }
                    Some(PersistCommand::FlushAndStop(ack)) => {
                        if let Some(cfg) = pending.take() {
                            persist_blocking(&path, cfg).await;
                        }
                        let _ = ack.send(());
                        break;
                    }
                    None => {
                        if let Some(cfg) = pending.take() {
                            persist_blocking(&path, cfg).await;
                        }
                        break;
                    }
                }
            }
            () = async {
                if let Some(until) = deadline {
                    time::sleep_until(until).await;
                } else {
                    std::future::pending::<()>().await;
                }
            } => {
                deadline = None;
                if let Some(cfg) = pending.take() {
                    persist_blocking(&path, cfg).await;
                }
            }
        }
    }
}

async fn persist_blocking(path: &Path, config: TactusConfig) {
    let path_owned = path.to_path_buf();
    match tokio::task::spawn_blocking(move || save_config_atomic(&path_owned, &config)).await {
        Ok(Ok(())) => {}
        Ok(Err(err)) => log_error!("failed to save config: {err}"),
        Err(err) => log_error!("config save task panicked: {err}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tactus_protocol::Direction;

    fn unique_temp_dir(label: &str) -> PathBuf {
        let seq = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "tactus-config-test-{label}-{}-{seq}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("create test temp dir");
        dir
    }

    #[test]
    fn config_path_precedence_prefers_cli_then_env_then_default() {
        assert_eq!(
            resolve_config_path(
                Some(PathBuf::from("/cli/config.toml")),
                Some(OsString::from("/env/config.toml"))
            ),
            PathBuf::from("/cli/config.toml")
        );
        assert_eq!(
            resolve_config_path(None, Some(OsString::from("/env/config.toml"))),
            PathBuf::from("/env/config.toml")
        );
        assert_eq!(
            resolve_config_path(None, Some(OsString::from(""))),
            PathBuf::from(DEFAULT_CONFIG_PATH)
        );
        assert_eq!(
            resolve_config_path(None, None),
            PathBuf::from(DEFAULT_CONFIG_PATH)
        );
    }

    #[test]
    fn missing_file_returns_default_without_creating_file() {
        let dir = unique_temp_dir("missing");
        let path = dir.join("config.toml");

        let (cfg, existed) = load_config(&path).expect("missing file should yield default");
        assert!(!existed);
        assert_eq!(cfg, TactusConfig::default());
        assert!(!path.exists());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_malformed_or_out_of_bounds_toml() {
        let dir = unique_temp_dir("invalid");
        let bad_syntax = dir.join("bad_syntax.toml");
        fs::write(&bad_syntax, "[sync\ndirection = ").expect("write bad toml");
        assert!(matches!(
            load_config(&bad_syntax),
            Err(ConfigStoreError::Parse { .. })
        ));

        let bad_bounds = dir.join("bad_bounds.toml");
        fs::write(&bad_bounds, "[sync]\nmidi_offset_ms = 99.0\n").expect("write oob toml");
        assert!(matches!(
            load_config(&bad_bounds),
            Err(ConfigStoreError::Validation(_))
        ));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn atomic_save_and_reload_leaves_no_temp_files() {
        let dir = unique_temp_dir("atomic");
        let path = dir.join("nested").join("config.toml");

        let mut cfg = TactusConfig::default();
        cfg.sync.direction = Direction::MidiToLink;
        cfg.sync.midi_offset_ms = -4.5;
        cfg.midi.input_port_match = "USB MIDI".to_string();

        save_config_atomic(&path, &cfg).expect("atomic save succeeds");
        let (reloaded, existed) = load_config(&path).expect("reload succeeds");
        assert!(existed);
        assert_eq!(reloaded, cfg);

        let entries: Vec<_> = fs::read_dir(path.parent().expect("parent"))
            .expect("read dir")
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, vec!["config.toml".to_string()]);

        let _ = fs::remove_dir_all(&dir);
    }
}
