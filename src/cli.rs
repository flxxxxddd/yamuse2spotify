//! the command line.
//!
//! `run` does the whole migration; the individual phases exist because a
//! library big enough to need this tool is also big enough that you will want
//! to redo one step without redoing the others.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::download::Format;
use crate::matcher::{AUTO_THRESHOLD, REVIEW_THRESHOLD, Thresholds};
use crate::pipeline::DownloadPolicy;
use crate::source::Scope;
use crate::ui::Ambiguous;

/// migrate a yandex music library to spotify.
#[derive(Debug, Parser)]
#[command(name = "yamuse2spotify", version, about, long_about = None)]
pub struct Cli {
    /// where the library dump, caches, reports and downloads go.
    #[arg(long, short, global = true, default_value = "./out")]
    pub out: PathBuf,

    /// configuration file holding the two credentials.
    #[arg(long, global = true)]
    pub config: Option<PathBuf>,

    /// spotify application client id. stored in the config after the first run.
    #[arg(long, global = true, env = "SPOTIFY_CLIENT_ID")]
    pub spotify_client_id: Option<String>,

    /// yandex music oauth token, if you already have one.
    #[arg(long, global = true, env = "YM_TOKEN")]
    pub yandex_token: Option<String>,

    /// never prompt — take the defaults and report whatever needs a decision.
    #[arg(long, global = true)]
    pub non_interactive: bool,

    /// self-imposed ceiling on spotify requests per second.
    #[arg(long, global = true, default_value_t = 8.0)]
    pub rps: f64,

    /// what to do about matches that are plausible but not convincing.
    #[arg(long, global = true, value_enum, default_value_t = AmbiguousArg::Ask)]
    pub ambiguous: AmbiguousArg,

    /// the phase to run.
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// the phases, in the order `run` performs them.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// authorise both services and stop.
    Auth,

    /// download the yandex library into `library.json`.
    Pull {
        /// what to pull.
        #[command(flatten)]
        scope: ScopeArgs,
    },

    /// find the spotify counterpart of every track.
    Match {
        /// matching thresholds.
        #[command(flatten)]
        thresholds: ThresholdArgs,
    },

    /// push the matched library into spotify.
    Push,

    /// download what spotify does not carry.
    Download {
        /// download options.
        #[command(flatten)]
        options: DownloadArgs,
    },

    /// rewrite the reports from the current journal.
    Report,

    /// pull, match, push, download and report, in that order.
    Run {
        /// what to migrate.
        #[command(flatten)]
        scope: ScopeArgs,
        /// matching thresholds.
        #[command(flatten)]
        thresholds: ThresholdArgs,
        /// download options.
        #[command(flatten)]
        options: DownloadArgs,
    },
}

/// which parts of the library to touch.
#[derive(Debug, Clone, Default, Args)]
pub struct ScopeArgs {
    /// skip liked tracks.
    #[arg(long)]
    pub no_tracks: bool,
    /// skip playlists.
    #[arg(long)]
    pub no_playlists: bool,
    /// skip liked albums.
    #[arg(long)]
    pub no_albums: bool,
    /// skip liked artists.
    #[arg(long)]
    pub no_artists: bool,
}

impl From<&ScopeArgs> for Scope {
    fn from(args: &ScopeArgs) -> Self {
        Self {
            liked_tracks: !args.no_tracks,
            playlists: !args.no_playlists,
            albums: !args.no_albums,
            artists: !args.no_artists,
        }
    }
}

/// where the two confidence boundaries sit.
#[derive(Debug, Clone, Args)]
pub struct ThresholdArgs {
    /// accept a match at or above this score without asking.
    #[arg(long, default_value_t = AUTO_THRESHOLD)]
    pub auto_threshold: f64,

    /// below this, treat the track as having no counterpart at all.
    #[arg(long, default_value_t = REVIEW_THRESHOLD)]
    pub review_threshold: f64,
}

impl Default for ThresholdArgs {
    fn default() -> Self {
        Self {
            auto_threshold: AUTO_THRESHOLD,
            review_threshold: REVIEW_THRESHOLD,
        }
    }
}

impl ThresholdArgs {
    /// validate and convert. a review boundary above the auto one would leave
    /// no band to review, which is a configuration mistake worth catching.
    pub fn resolve(&self) -> crate::error::Result<Thresholds> {
        if !(0.0..=1.0).contains(&self.auto_threshold)
            || !(0.0..=1.0).contains(&self.review_threshold)
        {
            return Err(crate::error::Error::Config(
                "пороги задаются числом от 0.0 до 1.0".into(),
            ));
        }

        if self.review_threshold > self.auto_threshold {
            return Err(crate::error::Error::Config(
                "--review-threshold должен быть не больше --auto-threshold".into(),
            ));
        }

        Ok(Thresholds {
            auto: self.auto_threshold,
            review: self.review_threshold,
        })
    }
}

/// how the download phase behaves.
#[derive(Debug, Clone, Args)]
pub struct DownloadArgs {
    /// which tracks to download from yandex.
    #[arg(long, value_enum, default_value_t = DownloadArg::Unmatched)]
    pub download: DownloadArg,

    /// which audio formats to accept.
    #[arg(long, value_enum, default_value_t = FormatArg::FlacThenMp3)]
    pub format: FormatArg,

    /// how many downloads run at once.
    #[arg(long, default_value_t = 3)]
    pub jobs: usize,
}

impl Default for DownloadArgs {
    fn default() -> Self {
        Self {
            download: DownloadArg::Unmatched,
            format: FormatArg::FlacThenMp3,
            jobs: 3,
        }
    }
}

/// cli spelling of [`DownloadPolicy`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum DownloadArg {
    /// only what spotify does not carry.
    Unmatched,
    /// the whole library.
    All,
    /// nothing, unless asked for during review.
    None,
}

impl From<DownloadArg> for DownloadPolicy {
    fn from(arg: DownloadArg) -> Self {
        match arg {
            DownloadArg::Unmatched => Self::Unmatched,
            DownloadArg::All => Self::Everything,
            DownloadArg::None => Self::Never,
        }
    }
}

/// cli spelling of [`Format`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum FormatArg {
    /// flac, falling back to the best mp3.
    FlacThenMp3,
    /// flac only.
    Flac,
    /// mp3 only.
    Mp3,
}

impl From<FormatArg> for Format {
    fn from(arg: FormatArg) -> Self {
        match arg {
            FormatArg::FlacThenMp3 => Self::FlacThenMp3,
            FormatArg::Flac => Self::Flac,
            FormatArg::Mp3 => Self::Mp3,
        }
    }
}

/// cli spelling of [`Ambiguous`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum AmbiguousArg {
    /// ask about each one.
    Ask,
    /// leave them all unmatched.
    Skip,
    /// take the best candidate regardless.
    AcceptBest,
}

impl From<AmbiguousArg> for Ambiguous {
    fn from(arg: AmbiguousArg) -> Self {
        match arg {
            AmbiguousArg::Ask => Self::Ask,
            AmbiguousArg::Skip => Self::Skip,
            AmbiguousArg::AcceptBest => Self::AcceptBest,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn the_command_line_definition_is_internally_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn no_subcommand_is_allowed_so_the_bare_binary_can_do_something_useful() {
        let cli = Cli::try_parse_from(["yamuse2spotify"]).unwrap();
        assert!(cli.command.is_none());
    }

    #[test]
    fn skipping_a_part_of_the_library_turns_off_exactly_that_part() {
        let args = ScopeArgs {
            no_tracks: false,
            no_playlists: true,
            no_albums: false,
            no_artists: false,
        };
        let scope = Scope::from(&args);

        assert!(scope.liked_tracks);
        assert!(!scope.playlists);
        assert!(scope.albums);
        assert!(scope.artists);
    }

    #[test]
    fn a_review_band_above_the_auto_band_is_rejected_instead_of_silently_empty() {
        let bad = ThresholdArgs {
            auto_threshold: 0.5,
            review_threshold: 0.9,
        };
        assert!(bad.resolve().is_err());

        let out_of_range = ThresholdArgs {
            auto_threshold: 1.5,
            review_threshold: 0.5,
        };
        assert!(out_of_range.resolve().is_err());

        let good = ThresholdArgs {
            auto_threshold: 0.9,
            review_threshold: 0.5,
        };
        assert!(good.resolve().is_ok());
    }

    #[test]
    fn the_defaults_are_the_ones_the_matcher_ships_with() {
        let cli = Cli::try_parse_from(["yamuse2spotify", "match"]).unwrap();
        let Some(Command::Match { thresholds }) = cli.command else {
            panic!("expected the match subcommand");
        };

        let resolved = thresholds.resolve().unwrap();
        assert!((resolved.auto - AUTO_THRESHOLD).abs() < f64::EPSILON);
        assert!((resolved.review - REVIEW_THRESHOLD).abs() < f64::EPSILON);
    }
}
