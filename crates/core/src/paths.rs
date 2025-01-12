use serde::de::DeserializeOwned;
use serde::Serialize;
use std::fs::OpenOptions;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use dirs::{config_dir, data_local_dir, home_dir};

/// Resolve `~` and environment-relative paths the way the CLIs do.
pub fn expand_home(raw: &str) -> PathBuf {
    let raw = raw.trim();
    if let Some(rest) = raw.strip_prefix("~/") {
        return home().join(rest);
    }
    if raw == "~" {
        return home();
    }
    PathBuf::from(raw)
}

pub fn home() -> PathBuf {
    home_dir().unwrap_or_else(|| PathBuf::from("."))
}

pub fn app_data() -> PathBuf {
    data_local_dir().unwrap_or_else(home).join("MultiMeters")
}

pub fn app_config_dir() -> PathBuf {
    config_dir()
        .unwrap_or_else(|| home().join(".config"))
        .join("multimeters")
}

pub fn settings_path() -> PathBuf {
    app_data().join("settings.json")
}

pub fn cache_path() -> PathBuf {
    app_data().join("cache.json")
}

pub fn log_path() -> PathBuf {
    app_data().join("logs").join("multimeters.log")
}

pub fn proxy_config_path() -> PathBuf {
    home().join(".multimeters").join("config.json")
}

/// Cursor's VS Code-style state DB.
pub fn cursor_state_db() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        dirs::data_dir()
            .or_else(data_local_dir)
            .unwrap_or_else(home)
            .join("Cursor")
            .join("User")
            .join("globalStorage")
            .join("state.vscdb")
    }
    #[cfg(not(target_os = "windows"))]
    {
        home().join("Library/Application Support/Cursor/User/globalStorage/state.vscdb")
    }
}

pub fn cursor_state_db_alternate() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        data_local_dir().map(|p| {
            p.join("Cursor")
                .join("User")
                .join("globalStorage")
                .join("state.vscdb")
        })
    }
    #[cfg(not(target_os = "windows"))]
    {
        None
    }
}

pub fn claude_home() -> PathBuf {
    std::env::var("CLAUDE_CONFIG_DIR")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(|s| expand_home(&s))
        .unwrap_or_else(|| home().join(".claude"))
}

pub fn grok_home() -> PathBuf {
    std::env::var("GROK_HOME")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(|s| expand_home(&s))
        .unwrap_or_else(|| home().join(".grok"))
}

pub fn devin_credentials() -> PathBuf {
    home().join(".local/share/devin/credentials.toml")
}

pub fn devin_state_db() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        dirs::data_dir()
            .unwrap_or_else(home)
            .join("Devin/User/globalStorage/state.vscdb")
    }
    #[cfg(not(target_os = "windows"))]
    {
        home().join("Library/Application Support/Devin/User/globalStorage/state.vscdb")
    }
}

pub fn pi_sessions() -> PathBuf {
    std::env::var("PI_CODING_AGENT_SESSION_DIR")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(|s| expand_home(&s))
        .or_else(|| {
            std::env::var("PI_CODING_AGENT_DIR")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .map(|s| expand_home(&s).join("sessions"))
        })
        .unwrap_or_else(|| home().join(".pi/agent/sessions"))
}

pub fn first_existing(paths: &[PathBuf]) -> Option<PathBuf> {
    paths.iter().find(|p| p.exists()).cloned()
}

pub fn read_text(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

pub fn write_text(path: &Path, text: &str) -> std::io::Result<()> {
    write_bytes_atomic(path, text.as_bytes())
}

pub fn write_json_atomic<T: Serialize + ?Sized>(path: &Path, value: &T) -> anyhow::Result<()> {
    let bytes = serde_json::to_vec_pretty(value)?;
    write_bytes_atomic(path, &bytes)?;
    Ok(())
}

pub fn read_json_with_backup<T: DeserializeOwned>(path: &Path) -> anyhow::Result<Option<T>> {
    match std::fs::read(path) {
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(value) => Ok(Some(value)),
            Err(primary_error) => {
                let backup = backup_path(path);
                match std::fs::read(&backup) {
                    Ok(bytes) => serde_json::from_slice(&bytes)
                        .map(Some)
                        .map_err(|backup_error| {
                            anyhow::anyhow!(
                                "invalid primary JSON ({primary_error}) and backup JSON ({backup_error})"
                            )
                        }),
                    Err(backup_error) if backup_error.kind() == ErrorKind::NotFound => {
                        Err(primary_error.into())
                    }
                    Err(backup_error) => Err(anyhow::anyhow!(
                        "invalid primary JSON ({primary_error}) and backup could not be read ({backup_error})"
                    )),
                }
            }
        },
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn write_bytes_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("data");
    let temporary = path.with_file_name(format!(".{file_name}.{}.tmp", std::process::id()));
    let backup = backup_path(path);
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);

    if path.exists() {
        match std::fs::remove_file(&backup) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        std::fs::rename(path, &backup)?;
    }
    if let Err(error) = std::fs::rename(&temporary, path) {
        if backup.exists() && !path.exists() {
            if let Err(rollback) = std::fs::rename(&backup, path) {
                tracing::error!(%rollback, backup = %backup.display(), target = %path.display(), "atomic-write rollback failed");
            }
        }
        if let Err(cleanup) = std::fs::remove_file(&temporary) {
            if cleanup.kind() != ErrorKind::NotFound {
                tracing::warn!(%cleanup, temporary = %temporary.display(), "could not remove failed atomic-write temporary file");
            }
        }
        return Err(error);
    }
    Ok(())
}

fn backup_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("data");
    path.with_file_name(format!("{name}.bak"))
}

pub fn sqlite_value(path: &Path, sql: &str) -> Option<String> {
    let conn = rusqlite::Connection::open(path).ok()?;
    conn.query_row(sql, [], |row| row.get::<_, String>(0))
        .ok()
        .or_else(|| {
            conn.query_row(sql, [], |row| row.get::<_, Vec<u8>>(0))
                .ok()
                .and_then(|b| String::from_utf8(b).ok())
        })
}

pub fn cred_read(service: &str, user: Option<&str>) -> Option<String> {
    match cred_read_checked(service, user) {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%service, user = user.unwrap_or("default"), %error, "could not read OS credentials");
            None
        }
    }
}

pub fn cred_read_checked(service: &str, user: Option<&str>) -> anyhow::Result<Option<String>> {
    let user = user.unwrap_or("default");
    let entry = keyring::Entry::new(service, user)?;
    let raw = match entry.get_password() {
        Ok(raw) => raw,
        Err(keyring::Error::NoEntry) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if raw.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(unwrap_go_keyring(&raw)?))
}

fn unwrap_go_keyring(raw: &str) -> anyhow::Result<String> {
    const PREFIX: &str = "go-keyring-base64:";
    if let Some(rest) = raw.strip_prefix(PREFIX) {
        let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, rest)
            .map_err(|error| anyhow::anyhow!("invalid go-keyring base64 wrapper: {error}"))?;
        return String::from_utf8(bytes)
            .map_err(|error| anyhow::anyhow!("go-keyring value is not UTF-8: {error}"));
    }
    Ok(raw.to_string())
}

pub fn cred_write(service: &str, user: Option<&str>, value: &str) -> anyhow::Result<()> {
    let user = user.unwrap_or("default");
    keyring::Entry::new(service, user)?.set_password(value)?;
    Ok(())
}

pub fn cred_delete(service: &str, user: Option<&str>) -> anyhow::Result<()> {
    let user = user.unwrap_or("default");
    let entry = keyring::Entry::new(service, user)?;
    match entry.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

const API_KEY_SERVICE: &str = "MultiMeters API Keys";

pub fn app_api_key_checked(provider: &str) -> anyhow::Result<Option<String>> {
    cred_read_checked(API_KEY_SERVICE, Some(provider))
}

pub fn set_app_api_key(provider: &str, value: &str) -> anyhow::Result<()> {
    if value.trim().is_empty() {
        cred_delete(API_KEY_SERVICE, Some(provider))
    } else {
        cred_write(API_KEY_SERVICE, Some(provider), value.trim())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unwraps_go_keyring_values_and_rejects_broken_wrappers() {
        assert_eq!(
            unwrap_go_keyring("go-keyring-base64:aGVsbG8=").unwrap(),
            "hello"
        );
        assert!(unwrap_go_keyring("go-keyring-base64:not-base64").is_err());
        assert_eq!(unwrap_go_keyring("raw-token").unwrap(), "raw-token");
    }
}
