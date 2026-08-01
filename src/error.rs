//! the one error type every phase returns.

use std::path::PathBuf;

/// anything that can go wrong during a migration.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// the yandex music client failed.
    #[error("yandex music: {0}")]
    Yandex(#[from] yamuse::Error),

    /// the spotify client failed without an http status to go by — a dropped
    /// connection, a timeout, or a session that would not open.
    #[error("spotify: {0}")]
    Spotify(String),

    /// spotify answered with an error status, and this is what it said.
    ///
    /// separate from [`Self::Spotify`] because a bare "status code 403" is
    /// indistinguishable between a missing scope, a malformed body and a
    /// parameter spotify no longer accepts.
    #[error("spotify {status}: {message}{}", rate_limit_hint(*status, *retry_after))]
    SpotifyStatus {
        /// the http status.
        status: u16,
        /// the `error.message` spotify returned, or the raw body.
        message: String,
        /// seconds spotify asked us to wait, when it said.
        retry_after: Option<u64>,
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
fn rate_limit_hint(status: u16, retry_after: Option<u64>) -> String {
    // an hour or more is not the rolling window spotify documents — it is the
    // app's quota being spent for the day. slowing down cannot shorten it, and
    // saying so matters: the obvious remedy is the wrong one.
    const LOCKOUT: u64 = 3_600;

    if status != 429 {
        return String::new();
    }

    let (wait, remedy) = match retry_after {
        Some(secs) if secs >= LOCKOUT => (
            format!("\n  spotify просит подождать {}.", humanise(secs)),
            "\n  столько — это не обычное окно, а исчерпанная суточная квота приложения. \
             сбавлять темп бесполезно, помогает только меньше запросов всего.",
        ),
        Some(secs) => (
            format!("\n  spotify просит подождать {}.", humanise(secs)),
            "\n  если повторится — уменьшите темп: --rps 1",
        ),
        // the header is only "normally" present, and a run has to be able to
        // resume without it — so say what to do rather than inventing a number.
        None => (
            "\n  spotify не сказал, сколько ждать — попробуйте через 15–30 минут.".to_owned(),
            "\n  если повторится — уменьшите темп: --rps 1",
        ),
    };

    format!(
        "{wait}\n  прогресс сохранён: та же команда продолжит с места остановки, \
         не потратив ни одного повторного запроса.{remedy}"
    )
}

/// render a number of seconds the way a person would say it.
pub fn humanise(seconds: u64) -> String {
    match seconds {
        0..=59 => format!("{seconds} с"),
        60..=3599 => {
            let minutes = seconds / 60;
            format!("{minutes} мин ({seconds} с)")
        }
        _ => {
            let hours = seconds / 3600;
            let minutes = (seconds % 3600) / 60;
            format!("{hours} ч {minutes} мин")
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rate_limit_says_how_long_and_a_plain_failure_says_nothing_extra() {
        let told = Error::SpotifyStatus {
            status: 429,
            message: "Too many requests".into(),
            retry_after: Some(120),
        };
        assert!(told.to_string().contains("2 мин"));
        assert!(told.to_string().contains("--rps 1"));

        // an hour or more is a spent quota, where slowing down does nothing —
        // offering it as the remedy would send the user down a dead end.
        let lockout = Error::SpotifyStatus {
            status: 429,
            message: "Too many requests".into(),
            retry_after: Some(84_000),
        };
        assert!(lockout.to_string().contains("суточная квота"));
        assert!(!lockout.to_string().contains("--rps 1"));

        // the header is only "normally" present; without it the message has to
        // stay useful rather than invent a number.
        let untold = Error::SpotifyStatus {
            status: 429,
            message: "Too many requests".into(),
            retry_after: None,
        };
        assert!(untold.to_string().contains("15–30 минут"));

        let other = Error::SpotifyStatus {
            status: 403,
            message: "Forbidden".into(),
            retry_after: None,
        };
        assert!(!other.to_string().contains("прогресс сохранён"));
    }

    #[test]
    fn a_wait_reads_as_a_person_would_say_it() {
        assert_eq!(humanise(45), "45 с");
        assert_eq!(humanise(600), "10 мин (600 с)");
        assert_eq!(humanise(7_800), "2 ч 10 мин");
    }
}
