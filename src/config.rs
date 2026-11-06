use anyhow::{Context, Result, bail};
use reqwest::Url;
use serde::Deserialize;
use std::{fs, path::Path, path::PathBuf};

pub struct Config {
    pub url: Url,
    pub token: String,
    pub download_dir: PathBuf,
    pub timeout_secs: u64,
    pub max_attachment_bytes: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    #[serde(default = "default_url")]
    url: String,
    token: String,
    #[serde(default = "default_download_dir")]
    download_dir: PathBuf,
    #[serde(default = "default_timeout")]
    timeout_secs: u64,
    #[serde(default = "default_max_attachment")]
    max_attachment_bytes: u64,
}

fn default_url() -> String {
    "https://youtrack.com".into()
}

fn default_download_dir() -> PathBuf {
    "./downloads".into()
}

fn default_timeout() -> u64 {
    60
}

fn default_max_attachment() -> u64 {
    100 * 1024 * 1024
}

impl Config {
    pub fn resolve_path(explicit: Option<&Path>) -> Result<PathBuf> {
        if let Some(path) = explicit {
            return Ok(path.to_owned());
        }
        let cwd = std::env::current_dir().context("Не удалось определить текущий каталог")?;
        resolve_default_path(&cwd, std::env::home_dir().as_deref())
    }

    pub fn load(path: &Path) -> Result<Self> {
        let path = fs::canonicalize(path)
            .with_context(|| format!("Не удалось найти конфигурацию: {}", path.display()))?;
        let text = fs::read_to_string(&path)
            .with_context(|| format!("Не удалось прочитать конфигурацию: {}", path.display()))?;
        // Ошибки TOML содержат фрагменты исходного файла, в том числе токен.
        let raw: FileConfig = toml::from_str(&text).map_err(|_| {
            anyhow::anyhow!(
                "Некорректная конфигурация TOML: проверьте синтаксис, названия и типы полей; token обязателен"
            )
        })?;
        let mut url =
            Url::parse(&raw.url).map_err(|_| anyhow::anyhow!("Некорректный url в конфигурации"))?;
        if !matches!(url.scheme(), "https" | "http")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            bail!("url должен быть базовым HTTP(S)-адресом без логина, пароля, query и fragment");
        }
        if !url.path().ends_with('/') {
            url.set_path(&format!("{}/", url.path()));
        }
        if raw.token.trim().is_empty() || raw.token == "perm:YOUR_TOKEN" {
            bail!("В конфигурации необходимо указать действительный token");
        }
        if raw.timeout_secs == 0 || raw.timeout_secs > 3600 {
            bail!("timeout_secs должен быть от 1 до 3600");
        }
        if raw.max_attachment_bytes == 0 {
            bail!("max_attachment_bytes должен быть больше нуля");
        }
        if raw.download_dir.as_os_str().is_empty() {
            bail!("download_dir не должен быть пустым");
        }
        let download_dir = if raw.download_dir.is_absolute() {
            raw.download_dir
        } else {
            path.parent()
                .context("У файла конфигурации отсутствует родительский каталог")?
                .join(raw.download_dir)
        };
        Ok(Self {
            url,
            token: raw.token,
            download_dir,
            timeout_secs: raw.timeout_secs,
            max_attachment_bytes: raw.max_attachment_bytes,
        })
    }
}

fn resolve_default_path(cwd: &Path, home: Option<&Path>) -> Result<PathBuf> {
    let local = cwd.join("yt-mcp.toml");
    if config_entry_exists(&local)? {
        return Ok(local);
    }
    let home = home.context(
        "yt-mcp.toml отсутствует в текущем каталоге; домашний каталог не определён. Укажите --config",
    )?;
    let global = home.join(".config/yt-mcp.toml");
    if config_entry_exists(&global)? {
        return Ok(global);
    }
    bail!(
        "Конфигурация не найдена: {} или {}. Укажите --config",
        local.display(),
        global.display()
    )
}

fn config_entry_exists(path: &Path) -> Result<bool> {
    // Нечитаемый файл, каталог или сломанная ссылка не должны переключать учётную запись.
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error)
            .with_context(|| format!("Не удалось проверить конфигурацию: {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_path_is_used_even_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.toml");
        assert_eq!(Config::resolve_path(Some(&missing)).unwrap(), missing);
        assert!(Config::load(&missing).is_err());
    }

    #[test]
    fn default_search_prefers_local_then_home_config() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("work");
        let home = dir.path().join("home");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(home.join(".config")).unwrap();
        let global = home.join(".config/yt-mcp.toml");
        fs::write(&global, "token = 'global-token'\n").unwrap();
        let resolved = resolve_default_path(&cwd, Some(&home)).unwrap();
        assert_eq!(resolved, global);
        assert_eq!(
            Config::load(&resolved).unwrap().download_dir,
            global.parent().unwrap().join("./downloads")
        );

        let local = cwd.join("yt-mcp.toml");
        fs::write(&local, "token = 'local-token'\n").unwrap();
        assert_eq!(resolve_default_path(&cwd, Some(&home)).unwrap(), local);
        assert_eq!(resolve_default_path(&cwd, None).unwrap(), local);

        fs::write(&local, "invalid TOML").unwrap();
        let resolved = resolve_default_path(&cwd, Some(&home)).unwrap();
        assert_eq!(resolved, local);
        assert!(Config::load(&resolved).is_err());
    }

    #[test]
    fn missing_default_configs_produce_clear_errors() {
        let dir = tempfile::tempdir().unwrap();
        let error = resolve_default_path(dir.path(), Some(dir.path())).unwrap_err();
        assert!(error.to_string().contains(".config"));
        assert!(error.to_string().contains("--config"));
        assert!(resolve_default_path(dir.path(), None).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn broken_local_symlink_does_not_trigger_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("yt-mcp.toml");
        std::os::unix::fs::symlink(dir.path().join("missing"), &path).unwrap();
        assert_eq!(
            resolve_default_path(dir.path(), Some(dir.path())).unwrap(),
            path
        );
        assert!(Config::load(&path).is_err());
    }

    #[test]
    fn defaults_and_paths_are_relative_to_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.toml");
        fs::write(&path, "token = 'test-token'\n").unwrap();
        let config = Config::load(&path).unwrap();
        assert_eq!(config.url.as_str(), "https://youtrack.com/");
        assert_eq!(config.timeout_secs, 60);
        assert_eq!(config.max_attachment_bytes, 104857600);
        assert_eq!(
            fs::canonicalize(dir.path()).unwrap().join("./downloads"),
            config.download_dir
        );
        assert!(!config.download_dir.exists());
    }

    #[test]
    fn invalid_config_never_exposes_token() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.toml");
        for contents in [
            "token = 'private-secret'\nunknown = true",
            "token = 'private-secret'\ntimeout_secs = 'wrong'",
            "token = 'private-secret",
            "url = 'https://private-secret@example.com'\ntoken = 'private-secret'",
            "token = ''",
            "token = 'perm:YOUR_TOKEN'",
            "url = 'ftp://example.com'\ntoken = 'private-secret'",
            "token = 'private-secret'\ntimeout_secs = 0",
            "token = 'private-secret'\nmax_attachment_bytes = 0",
            "token = 'private-secret'\ndownload_dir = ''",
            "url = 'https://example.com'",
        ] {
            fs::write(&path, contents).unwrap();
            let error = Config::load(&path).err().expect("Ожидалась ошибка");
            assert!(!format!("{error:#}").contains("private-secret"));
        }
        assert!(Config::load(&dir.path().join("missing.toml")).is_err());
    }

    #[test]
    fn explicit_settings_and_context_path() {
        let dir = tempfile::tempdir().unwrap();
        let downloads = dir.path().join("absolute-downloads");
        let path = dir.path().join("test.toml");
        fs::write(
            &path,
            format!(
                "token = 'test'\nurl = 'https://example.com/youtrack'\n\
                 download_dir = '{}'\ntimeout_secs = 5\nmax_attachment_bytes = 42",
                downloads.display()
            ),
        )
        .unwrap();
        let config = Config::load(&path).unwrap();
        assert_eq!(config.url.as_str(), "https://example.com/youtrack/");
        assert_eq!(config.download_dir, downloads);
        assert_eq!(config.timeout_secs, 5);
        assert_eq!(config.max_attachment_bytes, 42);
    }
}
