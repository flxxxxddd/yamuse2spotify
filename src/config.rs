//! persisted credentials and the layout of a run's working directory.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result, io};

/// spotify's loopback exception: since 2025 a redirect uri must be https,
/// *except* on the loopback interface — and `127.0.0.1` qualifies while
/// `localhost` explicitly does not.
pub const DEFAULT_REDIRECT_URI: &str = "http://127.0.0.1:8888/callback";

/// what survives between runs: the two credentials and where to send the
/// browser back to.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// yandex music oauth token, obtained through the device flow.
    pub yandex_token: Option<String>,
    /// spotify application client id. pkce needs no secret.
    pub spotify_client_id: Option<String>,
    /// redirect uri registered on the spotify application.
    pub redirect_uri: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            yandex_token: None,
            spotify_client_id: None,
            redirect_uri: DEFAULT_REDIRECT_URI.to_owned(),
        }
    }
}

/// a [`Config`] together with the file it came from.
pub struct Store {
    /// the loaded configuration.
    pub config: Config,
    path: PathBuf,
}

impl Store {
    /// load the configuration, or start from defaults when there is no file yet.
    pub fn load(path: PathBuf) -> Result<Self> {
        let config = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config::default(),
            Err(e) => return Err(io(&path)(e)),
        };

        Ok(Self { config, path })
    }

    /// write the configuration back, creating the directory if needed.
    ///
    /// the file holds two long-lived credentials, so it is written `0600` on
    /// unix rather than inheriting the umask.
    pub fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(io(parent))?;
        }

        let text = serde_json::to_string_pretty(&self.config)?;
        std::fs::write(&self.path, text).map_err(io(&self.path))?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600))
                .map_err(io(&self.path))?;
        }

        Ok(())
    }

    /// where the configuration lives.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// the default configuration path, `~/.config/yamuse2spotify/config.json`.
pub fn default_config_path() -> Result<PathBuf> {
    let base = dirs::config_dir()
        .ok_or_else(|| Error::Config("no configuration directory on this platform".into()))?;
    Ok(base.join("yamuse2spotify").join("config.json"))
}

/// every path a run reads or writes, derived from one output directory.
///
/// keeping them in one struct means a second account can be migrated into a
/// second directory with no shared state at all — `--out` is the only knob.
#[derive(Debug, Clone)]
pub struct Paths {
    /// the run's root directory.
    pub root: PathBuf,
    /// the pulled yandex library.
    pub library: PathBuf,
    /// cached spotify search responses, keyed by query.
    pub search_cache: PathBuf,
    /// resume state: what has already been matched, pushed and downloaded.
    pub state: PathBuf,
    /// spotify's own token cache, written by rspotify.
    pub spotify_token: PathBuf,
    /// where downloaded audio lands.
    pub music: PathBuf,
    /// where csv and markdown reports land.
    pub reports: PathBuf,
    /// the run log, so tracing never fights the progress bars for the terminal.
    pub log: PathBuf,
}

impl Paths {
    /// derive every path from the run's output directory.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Self {
            library: root.join("library.json"),
            search_cache: root.join("search-cache.json"),
            state: root.join("state.json"),
            spotify_token: root.join("spotify-token.json"),
            music: root.join("music"),
            reports: root.join("reports"),
            log: root.join("run.log"),
            root,
        }
    }

    /// create the directories a run writes into.
    pub fn prepare(&self) -> Result<()> {
        for dir in [&self.root, &self.music, &self.reports] {
            std::fs::create_dir_all(dir).map_err(io(dir))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_redirect_uri_uses_the_loopback_address_spotify_still_allows() {
        // `localhost` is rejected by spotify's 2025 redirect rules while the
        // literal loopback address is not. this must not drift back.
        assert!(DEFAULT_REDIRECT_URI.starts_with("http://127.0.0.1"));
        assert!(!DEFAULT_REDIRECT_URI.contains("localhost"));
    }

    #[test]
    fn a_missing_config_file_loads_as_defaults_rather_than_failing() {
        let dir = std::env::temp_dir().join("yamuse2spotify-config-test");
        let store = Store::load(dir.join("does-not-exist.json")).unwrap();
        assert!(store.config.yandex_token.is_none());
        assert_eq!(store.config.redirect_uri, DEFAULT_REDIRECT_URI);
    }

    #[test]
    fn every_run_path_sits_under_the_one_output_directory() {
        let paths = Paths::new("/tmp/run");
        for p in [
            &paths.library,
            &paths.search_cache,
            &paths.state,
            &paths.music,
            &paths.reports,
        ] {
            assert!(p.starts_with("/tmp/run"), "{p:?}");
        }
    }
}
