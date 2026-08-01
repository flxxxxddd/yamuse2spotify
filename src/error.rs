//! the one error type every phase returns.

use std::path::PathBuf;

/// anything that can go wrong during a migration.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// the yandex music client failed.
    #[error("yandex music: {0}")]
    Yandex(#[from] yamuse::Error),

    /// the spotify client failed.
    #[error("spotify: {0}")]
    Spotify(#[from] rspotify::ClientError),

    /// spotify answered with an error status, and this is what it said.
    ///
    /// separate from [`Self::Spotify`] because rspotify's own rendering stops at
    /// "status code 403", and the reason is always in the body.
    #[error("spotify {status}: {message}{}", rate_limit_hint(*status))]
    SpotifyStatus {
        /// the http status.
        status: u16,
        /// the `error.message` spotify returned, or the raw body.
        message: String,
    },

    /// a file operation failed. carries the path, because "permission denied"
    /// on its own is useless when six paths are in play.
    #[error("{path}: {source}")]
    Io {
        /// the path being operated on.
        path: PathBuf,
        /// the underlying failure.
        source: std::io::Error,
    },

    /// a state or library file could not be parsed or written.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),

    /// a report could not be written.
    #[error("csv: {0}")]
    Csv(#[from] csv::Error),

    /// tagging a downloaded file failed.
    #[error("tags: {0}")]
    Tags(#[from] lofty::error::LoftyError),

    /// fetching cover art failed.
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),

    /// the terminal prompt failed, or the user pressed esc.
    #[error("prompt: {0}")]
    Prompt(#[from] inquire::InquireError),

    /// something is missing or contradictory in the configuration.
    #[error("{0}")]
    Config(String),

    /// a spotify id came back in a shape the model rejects.
    #[error("spotify id: {0}")]
    SpotifyId(#[from] rspotify::model::IdError),

    /// the user chose to stop. state is already flushed by the time this is
    /// returned, so the next run picks up where this one left off.
    #[error("aborted — rerun to resume where this left off")]
    Aborted,
}

/// the crate's result alias.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// the advice that turns a bare 429 into something actionable.
///
/// a rate limit is the one status where the right move is not obvious and not
/// in the message: nothing is broken, the run simply has to resume later, and
/// the journal already makes that free.
fn rate_limit_hint(status: u16) -> &'static str {
    if status == 429 {
        "\n  лимит запросов spotify. прогресс сохранён — подождите и запустите ту же команду снова, \
         или уменьшите темп: --rps 1"
    } else {
        ""
    }
}

/// build an [`Error::Io`] closure that remembers `path`.
///
/// meant for `.map_err(io(&path))?`, which is shorter than naming the struct
/// variant at all ~40 call sites.
pub fn io(path: impl Into<PathBuf>) -> impl FnOnce(std::io::Error) -> Error {
    let path = path.into();
    move |source| Error::Io { path, source }
}
