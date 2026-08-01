//! persisted credentials and the layout of a run's working directory.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result, io};

/// spotify's loopback exception: since 2025 a redirect uri must be https,
/// *except* on the loopback interface — and `127.0.0.1` qualifies while
/// `localhost` explicitly does not.
pub const DEFAULT_REDIRECT_URI: &str = "http://127.0.0.1:8888/callback";

/// the client id used when the configuration names no application of its own.
///
/// this is spotify's own desktop client id — the one librespot authorises with,
/// and the reason registering an application is optional here. a developer
/// application starts in development mode, where every account that is not the
/// app owner has to be added to a 25-slot allowlist by hand; a tool that asks a
/// stranger to migrate their library cannot also ask them to register an
/// application and wait for a quota extension.
///
/// the trade-off is not free, and the readme says so: it is spotify's client id
/// rather than ours, so this path is outside their developer terms. anyone who
/// would rather stay inside them passes `--spotify-client-id`.
pub const BUILTIN_CLIENT_ID: &str = "65b708073fc0480ea92a077233ca87bd";

/// where [`BUILTIN_CLIENT_ID`] sends the browser back to.
///
/// the path is `/login` rather than anything else because that is what the
/// client id accepts; the port is free, loopback uris being matched without it.
pub const BUILTIN_REDIRECT_URI: &str = "http://127.0.0.1:8898/login";

/// what survives between runs: the two credentials and where to send the
/// browser back to.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// yandex music oauth token, obtained through the device flow.
    pub yandex_token: Option<String>,
    /// spotify application client id, when one of your own is preferred over
    /// the built-in. pkce needs no secret either way.
    pub spotify_client_id: Option<String>,
    /// redirect uri, when it has to differ from the one the client id implies.
    pub redirect_uri: Option<String>,
}

impl Config {
    /// which spotify application to authorise against, and where it sends the
    /// browser back to.
    ///
    /// the two travel together because a redirect uri is a property of the
    /// application: one belonging to the other client id is not merely wrong,
    /// it is rejected before the user ever sees a consent screen.
    pub fn spotify_app(&self) -> (&str, &str) {
        if let Some(id) = self.spotify_client_id.as_deref() {
            return (
                id,
                self.redirect_uri.as_deref().unwrap_or(DEFAULT_REDIRECT_URI),
            );
        }

        // older versions wrote the field out on every save, so a stored value
        // equal to the old default is not a deliberate override — and it is one
        // the built-in client id would reject.
        let chosen = self
            .redirect_uri
            .as_deref()
            .filter(|uri| *uri != DEFAULT_REDIRECT_URI)
            .unwrap_or(BUILTIN_REDIRECT_URI);
        (BUILTIN_CLIENT_ID, chosen)
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
            music: root.join("music"),
            reports: root.join("reports"),
            log: root.join("run.log"),
            root,
        }
    }

    /// spotify's own token cache, written by rspotify.
    ///
    /// keyed by client id rather than fixed, because a token belongs to the
    /// application it was issued to and its refresh token more so. switching
    /// applications — which now happens simply by adding or dropping
    /// `--spotify-client-id` — has to look like a fresh authorisation instead
    /// of a 400 from the refresh endpoint that nothing explains.
    pub fn spotify_token(&self, client_id: &str) -> PathBuf {
        let tag: String = client_id
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .take(8)
            .collect();
        let tag = if tag.is_empty() { "default" } else { &tag };
        self.root.join(format!("spotify-token-{tag}.json"))
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
    fn every_redirect_uri_uses_the_loopback_address_spotify_still_allows() {
        // `localhost` is rejected by spotify's 2025 redirect rules while the
        // literal loopback address is not. this must not drift back.
        for uri in [DEFAULT_REDIRECT_URI, BUILTIN_REDIRECT_URI] {
            assert!(uri.starts_with("http://127.0.0.1"), "{uri}");
            assert!(!uri.contains("localhost"), "{uri}");
        }
    }

    #[test]
    fn a_missing_config_file_loads_as_defaults_rather_than_failing() {
        let dir = std::env::temp_dir().join("yamuse2spotify-config-test");
        let store = Store::load(dir.join("does-not-exist.json")).unwrap();
        assert!(store.config.yandex_token.is_none());
        assert_eq!(store.config.spotify_app().0, BUILTIN_CLIENT_ID);
    }

    #[test]
    fn no_client_id_of_ones_own_authorises_against_the_built_in_application() {
        // the whole point of the built-in id: a first run needs nothing from
        // developer.spotify.com.
        let config = Config::default();
        assert_eq!(
            config.spotify_app(),
            (BUILTIN_CLIENT_ID, BUILTIN_REDIRECT_URI)
        );
    }

    #[test]
    fn a_client_id_of_ones_own_keeps_the_redirect_uri_that_belongs_to_it() {
        let config = Config {
            spotify_client_id: Some("mine".into()),
            ..Config::default()
        };
        assert_eq!(config.spotify_app(), ("mine", DEFAULT_REDIRECT_URI));
    }

    #[test]
    fn a_redirect_uri_written_by_an_older_version_does_not_break_the_built_in_id() {
        // the field used to be written out on every save, so a config from
        // before this change carries the old default with no client id beside
        // it — and the built-in id would reject that uri outright.
        let config = Config {
            redirect_uri: Some(DEFAULT_REDIRECT_URI.into()),
            ..Config::default()
        };
        assert_eq!(
            config.spotify_app(),
            (BUILTIN_CLIENT_ID, BUILTIN_REDIRECT_URI)
        );
    }

    #[test]
    fn a_deliberate_redirect_uri_still_wins() {
        let config = Config {
            redirect_uri: Some("http://127.0.0.1:9000/login".into()),
            ..Config::default()
        };
        assert_eq!(config.spotify_app().1, "http://127.0.0.1:9000/login");
    }

    #[test]
    fn two_applications_do_not_share_one_token_cache() {
        // a refresh token issued to one client id is refused by the other, and
        // the failure is a bare 400 that says nothing about why.
        let paths = Paths::new("/tmp/run");
        assert_ne!(
            paths.spotify_token(BUILTIN_CLIENT_ID),
            paths.spotify_token("something-else")
        );
        assert!(paths.spotify_token("").starts_with("/tmp/run"));
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
            &paths.spotify_token(BUILTIN_CLIENT_ID),
        ] {
            assert!(p.starts_with("/tmp/run"), "{p:?}");
        }
    }
}
