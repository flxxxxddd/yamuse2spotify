//! the terminal: progress bars, and the two questions worth interrupting for.
//!
//! interface strings are russian, everything else in this crate is english.
//! the split is deliberate — the person reading the prompts and the person
//! reading the code are not necessarily the same one.

use std::collections::HashMap;
use std::fmt;
use std::sync::Mutex;

use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use inquire::{Select, Text};

use crate::error::{Error, Result};
use crate::matcher::score::{Candidate, Score};
use crate::model::SourceTrack;

/// what to do with a match that scored in the review band.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// take the candidate at this index.
    Accept(usize),
    /// leave the track unmatched and move on.
    Skip,
    /// leave it unmatched and queue it for download instead.
    Download,
    /// use this spotify track id, pasted by hand.
    Manual(String),
}

/// what to do with a request that failed every automatic retry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recovery {
    /// try the same thing again.
    Retry,
    /// give up on this item and carry on with the rest.
    Skip,
    /// stop the run; state is flushed first, so a rerun resumes.
    Abort,
}

/// how ambiguous matches are handled when nobody is at the keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Ambiguous {
    /// ask, one prompt per track.
    #[default]
    Ask,
    /// leave every ambiguous track unmatched.
    Skip,
    /// take the highest-scoring candidate without asking.
    AcceptBest,
}

/// key under which a blanket "skip every remaining ambiguity" is remembered.
///
/// deliberately not a valid operation label, so it cannot collide with one.
const AMBIGUITY_KEY: &str = "\0ambiguous";

/// the terminal, and whatever the user has already said to apply to everything.
pub struct Ui {
    multi: MultiProgress,
    interactive: bool,
    ambiguous: Ambiguous,
    /// remembered "…and all the rest" answers, keyed by the failing operation.
    remembered: Mutex<HashMap<String, Recovery>>,
}

impl Ui {
    /// build the terminal front end.
    pub fn new(interactive: bool, ambiguous: Ambiguous) -> Self {
        Self {
            multi: MultiProgress::new(),
            interactive,
            ambiguous,
            remembered: Mutex::new(HashMap::new()),
        }
    }

    /// the progress area itself, for code that has to print around the bars but
    /// cannot borrow the whole [`Ui`] — the spotify client's wait notice.
    pub fn progress_area(&self) -> MultiProgress {
        self.multi.clone()
    }

    /// a counted progress bar.
    ///
    /// takes a `usize` so callers never have to cast a `.len()` at the call
    /// site — the conversion is lossless on every platform this builds for.
    pub fn bar(&self, len: usize, message: &str) -> ProgressBar {
        let len = u64::try_from(len).unwrap_or(u64::MAX);
        let style = ProgressStyle::with_template(
            "{spinner:.cyan} {msg:<22} [{bar:28.cyan/blue}] {pos}/{len} {wide_msg}",
        )
        .expect("the template is a constant and is known to parse")
        .progress_chars("━━╸");

        let bar = self.multi.add(ProgressBar::new(len));
        bar.set_style(style);
        bar.set_message(message.to_owned());
        bar
    }

    /// an uncounted spinner, for work whose length is not known up front.
    pub fn spinner(&self, message: &str) -> ProgressBar {
        let style = ProgressStyle::with_template("{spinner:.cyan} {msg}")
            .expect("the template is a constant and is known to parse");

        let bar = self.multi.add(ProgressBar::new_spinner());
        bar.set_style(style);
        bar.set_message(message.to_owned());
        bar.enable_steady_tick(std::time::Duration::from_millis(120));
        bar
    }

    /// print a line without a progress bar overwriting it.
    pub fn note(&self, message: &str) {
        // `suspend` clears the bars, runs the closure, then redraws. printing
        // straight to stdout instead leaves the bars smeared across the scroll.
        self.multi.suspend(|| println!("{message}"));
    }

    /// ask what to do about a track whose best candidate is not convincing.
    pub fn resolve(
        &self,
        track: &SourceTrack,
        candidates: &[(Candidate, Score)],
    ) -> Result<Decision> {
        if !self.interactive || self.ambiguous != Ambiguous::Ask {
            return Ok(match self.ambiguous {
                Ambiguous::AcceptBest if !candidates.is_empty() => Decision::Accept(0),
                _ => Decision::Skip,
            });
        }

        // "пропустить все оставшиеся" from an earlier prompt answers this one.
        if self.recall(AMBIGUITY_KEY).is_some() {
            return Ok(Decision::Skip);
        }

        let mut options: Vec<Choice> = candidates
            .iter()
            .enumerate()
            .map(|(i, (c, s))| Choice::Candidate {
                index: i,
                label: format!(
                    "{}  [{}]  {:.0}%  (назв. {:.2} / исп. {:.2} / длит. {:.2})",
                    c.display(),
                    c.duration_hms(),
                    s.total * 100.0,
                    s.title,
                    s.artist,
                    s.duration
                ),
            })
            .collect();

        options.push(Choice::Skip);
        options.push(Choice::Download);
        options.push(Choice::Manual);
        options.push(Choice::SkipAll);

        let prompt = format!("{}  [{}]", track.display(), track.duration_hms());

        let chosen = self.multi.suspend(|| {
            Select::new(&prompt, options)
                .with_page_size(12)
                .with_help_message("↑↓ выбрать · enter подтвердить")
                .prompt()
        })?;

        match chosen {
            Choice::Candidate { index, .. } => Ok(Decision::Accept(index)),
            Choice::Skip => Ok(Decision::Skip),
            Choice::Download => Ok(Decision::Download),
            Choice::SkipAll => {
                // stored under a key no operation label can collide with, so it
                // silences later ambiguity prompts and nothing else.
                self.remember(AMBIGUITY_KEY, Recovery::Skip);
                Ok(Decision::Skip)
            }
            Choice::Manual => {
                let raw = self
                    .multi
                    .suspend(|| Text::new("ссылка или uri трека в spotify:").prompt())?;
                if let Some(id) = parse_spotify_track_id(&raw) {
                    Ok(Decision::Manual(id))
                } else {
                    self.note("  не похоже на ссылку на трек — пропускаю");
                    Ok(Decision::Skip)
                }
            }
        }
    }

    /// ask what to do about an operation that failed every automatic retry.
    pub fn recover(&self, what: &str, error: &Error) -> Result<Recovery> {
        if let Some(remembered) = self.recall(what) {
            return Ok(remembered);
        }

        if !self.interactive {
            // unattended runs must not stall on a prompt nobody will answer.
            // skipping keeps the rest of the library moving and the failure is
            // in the error report either way.
            tracing::error!(what, %error, "skipping after repeated failures");
            return Ok(Recovery::Skip);
        }

        let prompt = format!("не удалось: {what} — {error}");
        let chosen = self.multi.suspend(|| {
            Select::new(
                &prompt,
                vec![
                    Fix::Retry,
                    Fix::Skip,
                    Fix::SkipAll,
                    Fix::RetryAll,
                    Fix::Abort,
                ],
            )
            .prompt()
        })?;

        Ok(match chosen {
            Fix::Retry => Recovery::Retry,
            Fix::Skip => Recovery::Skip,
            Fix::SkipAll => {
                self.remember(what, Recovery::Skip);
                Recovery::Skip
            }
            Fix::RetryAll => {
                self.remember(what, Recovery::Retry);
                Recovery::Retry
            }
            Fix::Abort => Recovery::Abort,
        })
    }

    fn remember(&self, what: &str, recovery: Recovery) {
        if let Ok(mut map) = self.remembered.lock() {
            map.insert(what.to_owned(), recovery);
        }
    }

    fn recall(&self, what: &str) -> Option<Recovery> {
        self.remembered.lock().ok()?.get(what).copied()
    }
}

/// one row in the ambiguity prompt.
enum Choice {
    Candidate { index: usize, label: String },
    Skip,
    Download,
    Manual,
    SkipAll,
}

impl fmt::Display for Choice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Candidate { label, .. } => f.write_str(label),
            Self::Skip => f.write_str("— пропустить"),
            Self::Download => f.write_str("— скачать с яндекса"),
            Self::Manual => f.write_str("— вставить ссылку вручную"),
            Self::SkipAll => f.write_str("— пропустить все оставшиеся"),
        }
    }
}

/// one row in the failure prompt.
enum Fix {
    Retry,
    Skip,
    SkipAll,
    RetryAll,
    Abort,
}

impl fmt::Display for Fix {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Retry => f.write_str("повторить"),
            Self::Skip => f.write_str("пропустить"),
            Self::SkipAll => f.write_str("пропускать все такие"),
            Self::RetryAll => f.write_str("повторять все такие"),
            Self::Abort => f.write_str("прервать и сохранить прогресс"),
        }
    }
}

/// advance a progress bar by a `usize`, without a cast at the call site.
pub fn advance(bar: &ProgressBar, by: usize) {
    bar.inc(u64::try_from(by).unwrap_or(u64::MAX));
}

/// pull a track id out of whatever the user pasted.
///
/// accepts the share url, the `spotify:track:` uri and a bare id, because all
/// three are one copy away in the desktop client and picking the wrong one is
/// not a mistake worth making the user care about.
pub fn parse_spotify_track_id(raw: &str) -> Option<String> {
    let raw = raw.trim();

    let candidate = if let Some(rest) = raw.strip_prefix("spotify:track:") {
        rest
    } else if let Some(at) = raw.find("/track/") {
        &raw[at + "/track/".len()..]
    } else {
        raw
    };

    // strip a query string or trailing path segment left over from a share link.
    let id = candidate
        .split(['?', '#', '/'])
        .next()
        .unwrap_or(candidate)
        .trim();

    (id.len() == 22 && id.chars().all(|c| c.is_ascii_alphanumeric())).then(|| id.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_track_id_is_recovered_from_every_shape_the_client_copies() {
        let id = "4y4VO05kYgUTo2bzbox1an";
        assert_eq!(parse_spotify_track_id(id).as_deref(), Some(id));
        assert_eq!(
            parse_spotify_track_id(&format!("spotify:track:{id}")).as_deref(),
            Some(id)
        );
        assert_eq!(
            parse_spotify_track_id(&format!("https://open.spotify.com/track/{id}?si=abc"))
                .as_deref(),
            Some(id)
        );
        assert_eq!(
            parse_spotify_track_id(&format!("  https://open.spotify.com/intl-ru/track/{id}  "))
                .as_deref(),
            Some(id)
        );
    }

    #[test]
    fn an_album_link_is_rejected_rather_than_pushed_as_a_track() {
        assert!(
            parse_spotify_track_id("https://open.spotify.com/album/4y4VO05kYgUTo2bzbox1an")
                .is_none()
        );
        assert!(parse_spotify_track_id("").is_none());
        assert!(parse_spotify_track_id("not an id").is_none());
    }

    #[test]
    fn a_non_interactive_run_skips_instead_of_waiting_on_a_prompt() {
        let ui = Ui::new(false, Ambiguous::Ask);
        let decision = ui.resolve(&SourceTrack::default(), &[]).unwrap();
        assert_eq!(decision, Decision::Skip);
    }

    #[test]
    fn accept_best_takes_the_top_candidate_without_asking() {
        let ui = Ui::new(false, Ambiguous::AcceptBest);
        let candidates = vec![(
            Candidate {
                id: "a".into(),
                title: "t".into(),
                artists: vec![],
                album: String::new(),
                duration_ms: 0,
                explicit: false,
                url: String::new(),
            },
            Score::default(),
        )];
        assert_eq!(
            ui.resolve(&SourceTrack::default(), &candidates).unwrap(),
            Decision::Accept(0)
        );
    }

    #[test]
    fn a_remembered_answer_replaces_the_prompt_for_later_failures() {
        let ui = Ui::new(true, Ambiguous::Ask);
        ui.remember("save tracks", Recovery::Skip);
        let err = Error::Config("boom".into());
        assert_eq!(ui.recover("save tracks", &err).unwrap(), Recovery::Skip);
    }
}
