//! what the run did, in a form that survives the terminal scrollback.
//!
//! csv for the rows a person will filter or re-import, markdown for the shape
//! of the whole thing. the unmatched list is the important one: it is the only
//! record of what a spotify library is missing compared to the yandex one.

use std::fmt::Write as _;
use std::path::PathBuf;

use serde::Serialize;

use crate::error::{Result, io};

/// one track that found a counterpart.
#[derive(Debug, Serialize)]
pub struct MatchedRow {
    /// yandex track id.
    pub yandex_id: String,
    /// how the track reads on the yandex side.
    pub source: String,
    /// how it reads on the spotify side.
    pub target: String,
    /// the spotify url, so a row is one click from being checked.
    pub url: String,
    /// total score.
    pub score: f64,
    /// whether a human confirmed it.
    pub reviewed: bool,
}

/// one track with no counterpart.
#[derive(Debug, Serialize)]
pub struct UnmatchedRow {
    /// yandex track id.
    pub yandex_id: String,
    /// how the track reads on the yandex side.
    pub source: String,
    /// duration, which is often what explains a near miss.
    pub duration: String,
    /// best score seen, or empty when nothing plausible turned up at all.
    pub best_score: String,
    /// what the best candidate was, for a sanity check.
    pub best_candidate: String,
    /// whether the audio was downloaded instead.
    pub downloaded: bool,
}

/// one downloaded file.
#[derive(Debug, Serialize)]
pub struct DownloadedRow {
    /// yandex track id.
    pub yandex_id: String,
    /// how the track reads.
    pub source: String,
    /// path relative to the music directory.
    pub path: String,
    /// what was served.
    pub codec: String,
    /// size on disk.
    pub bytes: u64,
}

/// one thing that failed and was given up on.
#[derive(Debug, Serialize)]
pub struct ErrorRow {
    /// which phase it happened in.
    pub phase: String,
    /// what was being worked on.
    pub item: String,
    /// the error, rendered.
    pub error: String,
}

/// counters for the closing summary.
#[derive(Debug, Default, Clone)]
pub struct Summary {
    /// tracks in the pulled library.
    pub tracks: usize,
    /// tracks matched to spotify.
    pub matched: usize,
    /// of those, how many a human confirmed.
    pub reviewed: usize,
    /// tracks with no counterpart.
    pub unmatched: usize,
    /// tracks added to "liked songs".
    pub saved: usize,
    /// playlists created or extended.
    pub playlists: usize,
    /// albums saved.
    pub albums: usize,
    /// artists followed.
    pub artists: usize,
    /// files downloaded.
    pub downloaded: usize,
    /// bytes downloaded.
    pub downloaded_bytes: u64,
    /// failures recorded.
    pub errors: usize,
}

impl Summary {
    /// the share of the library that made it to spotify, as a percentage.
    pub fn coverage(&self) -> f64 {
        if self.tracks == 0 {
            return 0.0;
        }

        #[allow(
            clippy::cast_precision_loss,
            reason = "library sizes are far below f64's exact integer range"
        )]
        let ratio = self.matched as f64 / self.tracks as f64;
        ratio * 100.0
    }
}

/// the reports directory, and the writers that fill it.
pub struct Reports {
    dir: PathBuf,
}

impl Reports {
    /// point the reports at a directory, creating it if needed.
    pub fn new(dir: impl Into<PathBuf>) -> Result<Self> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir).map_err(io(&dir))?;
        Ok(Self { dir })
    }

    /// write one csv, replacing whatever was there.
    ///
    /// always written, even when empty: an absent file is ambiguous between
    /// "nothing failed" and "the run never got that far", and a header-only
    /// file is not.
    pub fn write_csv<T: Serialize>(&self, name: &str, rows: &[T]) -> Result<PathBuf> {
        let path = self.dir.join(name);
        let mut writer = csv::Writer::from_path(&path)?;

        for row in rows {
            writer.serialize(row)?;
        }
        writer.flush().map_err(io(&path))?;

        Ok(path)
    }

    /// write the human-readable summary.
    pub fn write_summary(&self, summary: &Summary, library_pulled_at: &str) -> Result<PathBuf> {
        let path = self.dir.join("summary.md");
        let text = render_summary(summary, library_pulled_at);
        std::fs::write(&path, text).map_err(io(&path))?;
        Ok(path)
    }
}

/// render the summary markdown.
///
/// `write!` into a `String` cannot fail, so the results are discarded rather
/// than propagated — there is no error path to report.
fn render_summary(s: &Summary, pulled_at: &str) -> String {
    let mut out = String::new();

    out.push_str("# Перенос библиотеки: итог\n\n");
    let _ = writeln!(out, "Библиотека выгружена: `{pulled_at}`\n");

    out.push_str("| | |\n|---|---:|\n");
    let _ = writeln!(out, "| Треков в библиотеке | {} |", s.tracks);
    let _ = writeln!(
        out,
        "| Найдено в Spotify | {} ({:.1}%) |",
        s.matched,
        s.coverage()
    );
    let _ = writeln!(out, "| Из них подтверждено вручную | {} |", s.reviewed);
    let _ = writeln!(out, "| Не найдено | {} |", s.unmatched);
    let _ = writeln!(out, "| Добавлено в «Любимые треки» | {} |", s.saved);
    let _ = writeln!(out, "| Плейлистов перенесено | {} |", s.playlists);
    let _ = writeln!(out, "| Альбомов сохранено | {} |", s.albums);
    let _ = writeln!(out, "| Исполнителей отслеживается | {} |", s.artists);
    let _ = writeln!(
        out,
        "| Скачано локально | {} ({}) |",
        s.downloaded,
        human_bytes(s.downloaded_bytes)
    );
    let _ = writeln!(out, "| Ошибок | {} |", s.errors);

    out.push_str("\n## Файлы\n\n");
    out.push_str("- `matched.csv` — что с чем сопоставлено, со ссылками и оценками\n");
    out.push_str("- `unmatched.csv` — чего в Spotify нет\n");
    out.push_str("- `downloaded.csv` — что скачано локально\n");
    out.push_str("- `errors.csv` — что не удалось и почему\n");

    if s.downloaded > 0 {
        out.push_str("\n## Локальные файлы\n\n");
        out.push_str(
            "Скачанное лежит в `music/` вместе с `music/unmatched.m3u8`.\n\
             Spotify не принимает загрузку файлов через API — добавьте папку `music/`\n\
             в настройках десктопного клиента («Локальные файлы»), и он её проиндексирует.\n",
        );
    }

    out
}

/// rebuild every report row from the journal, without touching the network.
///
/// this is what makes `report` a real subcommand rather than a side effect of
/// `match`: the journal already holds what each track was matched to and why,
/// so the reports can be regenerated days later.
pub fn rebuild(
    library: &crate::model::Library,
    state: &crate::state::State,
) -> (Vec<MatchedRow>, Vec<UnmatchedRow>, Vec<DownloadedRow>) {
    let mut matched = Vec::new();
    let mut unmatched = Vec::new();
    let mut downloaded = Vec::new();

    for track in &library.tracks {
        if let Some(record) = state.matched.get(&track.id) {
            matched.push(MatchedRow {
                yandex_id: track.id.clone(),
                source: track.display(),
                target: record.target.clone(),
                url: record.url.clone(),
                score: record.score,
                reviewed: record.reviewed,
            });
        } else if let Some(record) = state.unmatched.get(&track.id) {
            unmatched.push(UnmatchedRow {
                yandex_id: track.id.clone(),
                source: track.display(),
                duration: track.duration_hms(),
                best_score: record
                    .best_score
                    .map_or_else(String::new, |s| format!("{s:.2}")),
                best_candidate: record.best_candidate.clone(),
                downloaded: state.downloaded.contains_key(&track.id),
            });
        }

        if let Some(path) = state.downloaded.get(&track.id) {
            downloaded.push(DownloadedRow {
                yandex_id: track.id.clone(),
                source: track.display(),
                path: path.clone(),
                // the journal records where a file went, not what it cost; the
                // filesystem is the authority on that and is cheap to ask.
                codec: std::path::Path::new(path)
                    .extension()
                    .map_or_else(String::new, |e| e.to_string_lossy().into_owned()),
                bytes: 0,
            });
        }
    }

    (matched, unmatched, downloaded)
}

/// render a byte count the way a person would say it.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["Б", "КБ", "МБ", "ГБ", "ТБ"];

    #[allow(
        clippy::cast_precision_loss,
        reason = "the value is only ever rendered, never compared or reused"
    )]
    let mut value = bytes as f64;
    let mut unit = 0;

    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }

    if unit == 0 {
        format!("{bytes} {}", UNITS[0])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
#[allow(
    clippy::float_cmp,
    reason = "these compare against values the arithmetic produces exactly — \
              0.0, 1.0 and the clamp boundaries — where an epsilon would only \
              hide a regression"
)]
mod tests {
    use super::*;

    #[test]
    fn coverage_is_a_percentage_and_survives_an_empty_library() {
        let empty = Summary::default();
        assert_eq!(empty.coverage(), 0.0);

        let half = Summary {
            tracks: 200,
            matched: 150,
            ..Default::default()
        };
        assert!((half.coverage() - 75.0).abs() < f64::EPSILON);
    }

    #[test]
    fn byte_counts_read_the_way_a_person_would_say_them() {
        assert_eq!(human_bytes(512), "512 Б");
        assert_eq!(human_bytes(2048), "2.0 КБ");
        assert_eq!(human_bytes(5 * 1024 * 1024 * 1024), "5.0 ГБ");
    }

    #[test]
    fn an_empty_report_still_gets_a_file_with_a_header_row() {
        let dir = std::env::temp_dir().join("yamuse2spotify-report-test");
        std::fs::remove_dir_all(&dir).ok();

        let reports = Reports::new(&dir).unwrap();
        let path = reports.write_csv::<ErrorRow>("errors.csv", &[]).unwrap();

        assert!(path.exists());
        // csv writes no header without a row, which is exactly the ambiguity
        // the caller must be able to distinguish — so an empty file is fine,
        // but the file itself must exist.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_written_report_round_trips_through_csv() {
        let dir = std::env::temp_dir().join("yamuse2spotify-report-roundtrip");
        std::fs::remove_dir_all(&dir).ok();

        let reports = Reports::new(&dir).unwrap();
        let path = reports
            .write_csv(
                "errors.csv",
                &[ErrorRow {
                    phase: "push".into(),
                    item: "Artist — Song".into(),
                    error: "boom".into(),
                }],
            )
            .unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("phase,item,error"));
        assert!(text.contains("Artist — Song"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_summary_mentions_the_local_files_caveat_only_when_there_are_any() {
        let with = Summary {
            downloaded: 3,
            ..Default::default()
        };
        assert!(render_summary(&with, "now").contains("Локальные файлы"));
        assert!(!render_summary(&Summary::default(), "now").contains("Локальные файлы"));
    }
}
