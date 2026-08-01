//! the phases themselves, in the order a migration runs them.
//!
//! each one reads the journal before acting and writes it after, so any of them
//! can be run alone, rerun, or interrupted. that is the whole design: no phase
//! assumes the previous one finished, only that it recorded what it did.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use futures::StreamExt;

use crate::config::Paths;
use crate::download::{Downloader, Format, write_m3u};
use crate::error::{Error, Result};
use crate::matcher::score::{Candidate, Score};
use crate::matcher::{Matcher, SearchCache, Thresholds, Verdict};
use crate::model::{Library, SourceTrack};
use crate::report::{DownloadedRow, ErrorRow, MatchedRow, UnmatchedRow};
use crate::resilience::guarded;
use crate::spotify::Spotify;
use crate::state::{Journal, MatchRecord, UnmatchedRecord};
use crate::ui::{Decision, Ui, advance};

/// how long a phase may go without flushing the journal.
///
/// counted in time rather than in items, because the two are not related: a
/// match runs at seconds per track, so "every hundred items" was a quarter of
/// an hour of work at risk — and for a library smaller than a hundred tracks it
/// meant never flushing at all, so a ctrl-c threw the whole run away.
const FLUSH_AFTER: Duration = Duration::from_secs(10);

/// which tracks the download phase should take.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DownloadPolicy {
    /// only what spotify does not carry. the default.
    #[default]
    Unmatched,
    /// the whole library, matched or not.
    Everything,
    /// nothing, unless review explicitly asked for a track.
    Never,
}

/// rows gathered during matching, for the reports.
#[derive(Debug, Default)]
pub struct MatchOutcome {
    /// tracks that found a counterpart.
    pub matched: Vec<MatchedRow>,
    /// tracks that did not.
    pub unmatched: Vec<UnmatchedRow>,
    /// failures that were skipped.
    pub errors: Vec<ErrorRow>,
}

/// find every unmatched track's spotify counterpart.
pub async fn run_match(
    library: &Library,
    spotify: &Spotify,
    journal: &mut Journal,
    ui: &Ui,
    paths: &Paths,
    thresholds: Thresholds,
) -> Result<MatchOutcome> {
    let cache = SearchCache::load(&paths.search_cache);
    if cache.len() > 0 {
        ui.note(&format!("  кэш поиска: {} запросов", cache.len()));
    }

    let matcher = Matcher::new(spotify, cache, thresholds);

    let pending: Vec<&SourceTrack> = library
        .tracks
        .iter()
        .filter(|t| !journal.state.is_decided(&t.id))
        .collect();

    let mut outcome = MatchOutcome::default();

    if pending.is_empty() {
        ui.note("  все треки уже сопоставлены");
        return Ok(outcome);
    }

    let bar = ui.bar(pending.len(), "поиск в spotify");
    // ambiguities are collected rather than resolved inline: stopping a
    // thousand-track scan for a prompt every twenty tracks turns a background
    // job into an afternoon.
    let mut ambiguous: Vec<(&SourceTrack, Vec<(Candidate, Score)>)> = Vec::new();

    let mut last_flush = Instant::now();

    let result = async {
        for track in &pending {
            bar.set_message(truncate(&track.display(), 40));

            let Some(verdict) = guarded(ui, "поиск в spotify", || matcher.find(track)).await?
            else {
                outcome.errors.push(ErrorRow {
                    phase: "match".into(),
                    item: track.display(),
                    error: "поиск не удался".into(),
                });
                bar.inc(1);
                continue;
            };

            match verdict {
                Verdict::Auto(candidate, score) => {
                    let row = matched_row(track, &candidate, score, false);
                    journal
                        .state
                        .matched
                        .insert(track.id.clone(), record(&row, &candidate.id));
                    outcome.matched.push(row);
                }
                Verdict::Ambiguous(candidates) => ambiguous.push((track, candidates)),
                Verdict::NotFound => {
                    journal
                        .state
                        .unmatched
                        .insert(track.id.clone(), UnmatchedRecord::default());
                    outcome.unmatched.push(unmatched_row(track, None));
                }
            }

            bar.inc(1);
            if last_flush.elapsed() >= FLUSH_AFTER {
                journal.save()?;
                matcher.save_cache(&paths.search_cache)?;
                last_flush = Instant::now();
            }

            // asked to stop: leave through the abort path, which flushes.
            if crate::interrupt::requested() {
                return Err(Error::Aborted);
            }
        }
        Ok::<_, Error>(())
    }
    .await;

    bar.finish_and_clear();

    // the journal and the cache are flushed even on the way out of an abort —
    // that is what makes "прервать и сохранить прогресс" true.
    journal.save()?;
    matcher.into_cache().save(&paths.search_cache)?;
    result?;

    if !ambiguous.is_empty() {
        ui.note(&format!(
            "\n  спорных совпадений: {} — разбираем\n",
            ambiguous.len()
        ));
        review(&ambiguous, journal, ui, &mut outcome)?;
        journal.save()?;
    }

    Ok(outcome)
}

/// walk the ambiguous matches, asking about each one.
fn review(
    ambiguous: &[(&SourceTrack, Vec<(Candidate, Score)>)],
    journal: &mut Journal,
    ui: &Ui,
    outcome: &mut MatchOutcome,
) -> Result<()> {
    for (track, candidates) in ambiguous {
        let decision = ui.resolve(track, candidates)?;

        if matches!(decision, Decision::Download) {
            journal.state.download_forced.insert(track.id.clone());
        }

        match decision {
            Decision::Accept(index) => {
                let (candidate, score) = &candidates[index];
                let row = matched_row(track, candidate, *score, true);
                journal
                    .state
                    .matched
                    .insert(track.id.clone(), record(&row, &candidate.id));
                outcome.matched.push(row);
            }
            Decision::Manual(spotify_id) => {
                let row = MatchedRow {
                    yandex_id: track.id.clone(),
                    source: track.display(),
                    target: "(указано вручную)".into(),
                    url: format!("https://open.spotify.com/track/{spotify_id}"),
                    score: 1.0,
                    reviewed: true,
                };
                journal
                    .state
                    .matched
                    .insert(track.id.clone(), record(&row, &spotify_id));
                outcome.matched.push(row);
            }
            Decision::Skip | Decision::Download => {
                let best = candidates.first();
                journal.state.unmatched.insert(
                    track.id.clone(),
                    UnmatchedRecord {
                        best_score: best.map(|(_, s)| s.total),
                        best_candidate: best.map_or_else(String::new, |(c, _)| c.display()),
                    },
                );
                outcome.unmatched.push(unmatched_row(track, best));
            }
        }
    }

    Ok(())
}

/// what the push phase managed to do.
#[derive(Debug, Default)]
pub struct PushOutcome {
    /// tracks added to "liked songs".
    pub saved: usize,
    /// playlists created or extended.
    pub playlists: usize,
    /// albums saved.
    pub albums: usize,
    /// artists followed.
    pub artists: usize,
    /// failures that were skipped.
    pub errors: Vec<ErrorRow>,
}

/// push everything matched into the spotify account.
pub async fn run_push(
    library: &Library,
    spotify: &Spotify,
    journal: &mut Journal,
    ui: &Ui,
) -> Result<PushOutcome> {
    let mut out = PushOutcome::default();

    out.saved = push_liked_tracks(library, spotify, journal, ui, &mut out.errors).await?;
    out.playlists = push_playlists(library, spotify, journal, ui, &mut out.errors).await?;
    out.albums = push_albums(library, spotify, journal, ui, &mut out.errors).await?;
    out.artists = push_artists(library, spotify, journal, ui, &mut out.errors).await?;

    Ok(out)
}

/// add the matched likes to "liked songs".
async fn push_liked_tracks(
    library: &Library,
    spotify: &Spotify,
    journal: &mut Journal,
    ui: &Ui,
    errors: &mut Vec<ErrorRow>,
) -> Result<usize> {
    if library.liked_track_ids.is_empty() {
        return Ok(0);
    }

    // `liked_track_ids` is already oldest-liked-first: the pull sorts it by the
    // timestamp yandex attaches to each like. pushing in that order is what
    // makes spotify's own "date added" come out in the original chronology.
    let candidates = dedup(
        library
            .liked_track_ids
            .iter()
            .filter_map(|yandex_id| journal.state.spotify_id(yandex_id))
            .filter(|id| !journal.state.saved_tracks.contains(*id))
            .map(str::to_owned),
    );

    // the journal covers what this tool added; spotify is asked about the rest,
    // in batches, so the cost tracks the migration rather than the size of the
    // account's existing library.
    let spinner = ui.spinner("что уже есть в «любимых»…");
    let existing = guarded(
        ui,
        "проверка любимых треков spotify",
        || spotify.already_saved(&candidates),
    )
    .await?
    .unwrap_or_default();
    spinner.finish_and_clear();

    let to_add: Vec<String> = candidates
        .into_iter()
        .filter(|id| !existing.contains(id))
        .collect();

    if to_add.is_empty() {
        ui.note("  «любимые треки» уже актуальны");
        return Ok(0);
    }

    let bar = ui.bar(to_add.len(), "любимые треки");
    let mut added = 0;

    for chunk in to_add.chunks(50) {
        let done = guarded(ui, "добавление в «любимые»", || {
            spotify.save_tracks(chunk)
        })
        .await?;

        if done.is_some() {
            journal.state.saved_tracks.extend(chunk.iter().cloned());
            added += chunk.len();
            // saved per batch rather than per phase: a push writes to somebody's
            // library, and a batch that landed must never look unsent.
            journal.save()?;
        } else {
            errors.push(ErrorRow {
                phase: "push".into(),
                item: format!("{} треков в «любимые»", chunk.len()),
                error: "пропущено после ошибок".into(),
            });
        }

        advance(&bar, chunk.len());

        if crate::interrupt::requested() {
            break;
        }
    }

    bar.finish_and_clear();
    Ok(added)
}

/// recreate the yandex playlists in spotify.
async fn push_playlists(
    library: &Library,
    spotify: &Spotify,
    journal: &mut Journal,
    ui: &Ui,
    errors: &mut Vec<ErrorRow>,
) -> Result<usize> {
    if library.playlists.is_empty() {
        return Ok(0);
    }

    let spinner = ui.spinner("плейлисты spotify…");
    let existing = guarded(ui, "чтение плейлистов spotify", || {
        spotify.own_playlists()
    })
    .await?
    .unwrap_or_default();
    spinner.finish_and_clear();

    let bar = ui.bar(library.playlists.len(), "плейлисты");
    let mut touched = 0;

    for playlist in &library.playlists {
        bar.set_message(truncate(&playlist.title, 40));

        let wanted: Vec<String> = playlist
            .track_ids
            .iter()
            .filter_map(|yandex_id| journal.state.spotify_id(yandex_id).map(str::to_owned))
            .collect();

        if wanted.is_empty() {
            bar.inc(1);
            continue;
        }

        let key = playlist.kind.to_string();

        // a spotify playlist already carrying this name is adopted rather than
        // duplicated: the common case is a rerun after the journal was deleted,
        // and creating a second "Мой плейлист" would be a mess to undo.
        let known = journal
            .state
            .playlists
            .get(&key)
            .or_else(|| existing.get(&playlist.title))
            .cloned();

        let spotify_id = if let Some(id) = known {
            id
        } else {
            let created = guarded(ui, "создание плейлиста", || {
                spotify.create_playlist(&playlist.title, playlist.description.as_deref())
            })
            .await?;

            let Some(id) = created else {
                errors.push(ErrorRow {
                    phase: "push".into(),
                    item: playlist.title.clone(),
                    error: "не удалось создать плейлист".into(),
                });
                bar.inc(1);
                continue;
            };
            id
        };

        journal.state.playlists.insert(key, spotify_id.clone());

        let present: HashSet<String> = guarded(
            ui,
            "чтение содержимого плейлиста",
            || spotify.playlist_track_ids(&spotify_id),
        )
        .await?
        .unwrap_or_default()
        .into_iter()
        .collect();

        let missing: Vec<String> = wanted
            .into_iter()
            .filter(|id| !present.contains(id))
            .collect();

        if !missing.is_empty() {
            let added = guarded(ui, "наполнение плейлиста", || {
                spotify.playlist_add(&spotify_id, &missing)
            })
            .await?;

            if added.is_none() {
                errors.push(ErrorRow {
                    phase: "push".into(),
                    item: playlist.title.clone(),
                    error: "не удалось добавить треки".into(),
                });
            }
        }

        touched += 1;
        journal.save()?;
        bar.inc(1);

        if crate::interrupt::requested() {
            break;
        }
    }

    bar.finish_and_clear();
    Ok(touched)
}

/// save the liked albums, matching them by "artist — title".
async fn push_albums(
    library: &Library,
    spotify: &Spotify,
    journal: &mut Journal,
    ui: &Ui,
    errors: &mut Vec<ErrorRow>,
) -> Result<usize> {
    if library.albums.is_empty() {
        return Ok(0);
    }

    let bar = ui.bar(library.albums.len(), "альбомы");
    let mut found = Vec::new();

    for album in &library.albums {
        bar.set_message(truncate(&album.display(), 40));

        let query = format!("{} {}", album.artists.join(" "), album.title);
        let hit = guarded(ui, "поиск альбома", || {
            spotify.search_album(&query)
        })
        .await?;

        // the two `None`s mean different things and the report must say which:
        // the outer one is "the search kept failing", the inner one is "spotify
        // does not have this album".
        match hit {
            Some(Some((id, _, _))) if !journal.state.saved_albums.contains(&id) => found.push(id),
            Some(Some(_)) => {}
            Some(None) => errors.push(ErrorRow {
                phase: "push".into(),
                item: album.display(),
                error: "альбом не найден в spotify".into(),
            }),
            None => errors.push(ErrorRow {
                phase: "push".into(),
                item: album.display(),
                error: "поиск альбома не удался".into(),
            }),
        }

        bar.inc(1);
    }

    bar.finish_and_clear();

    if found.is_empty() {
        return Ok(0);
    }

    let saved = guarded(ui, "сохранение альбомов", || {
        spotify.save_albums(&found)
    })
    .await?;
    if saved.is_some() {
        journal.state.saved_albums.extend(found.iter().cloned());
        journal.save()?;
        return Ok(found.len());
    }

    Ok(0)
}

/// follow the liked artists.
async fn push_artists(
    library: &Library,
    spotify: &Spotify,
    journal: &mut Journal,
    ui: &Ui,
    errors: &mut Vec<ErrorRow>,
) -> Result<usize> {
    if library.artists.is_empty() {
        return Ok(0);
    }

    let bar = ui.bar(library.artists.len(), "исполнители");
    let mut found = Vec::new();

    for artist in &library.artists {
        bar.set_message(truncate(&artist.name, 40));

        let hit = guarded(ui, "поиск исполнителя", || {
            spotify.search_artist(&artist.name)
        })
        .await?;

        match hit {
            Some(Some((id, _))) if !journal.state.followed_artists.contains(&id) => found.push(id),
            Some(Some(_)) => {}
            Some(None) => errors.push(ErrorRow {
                phase: "push".into(),
                item: artist.name.clone(),
                error: "исполнитель не найден в spotify".into(),
            }),
            None => errors.push(ErrorRow {
                phase: "push".into(),
                item: artist.name.clone(),
                error: "поиск исполнителя не удался".into(),
            }),
        }

        bar.inc(1);
    }

    bar.finish_and_clear();

    if found.is_empty() {
        return Ok(0);
    }

    let followed = guarded(ui, "подписка на исполнителей", || {
        spotify.follow_artists(&found)
    })
    .await?;

    if followed.is_some() {
        journal.state.followed_artists.extend(found.iter().cloned());
        journal.save()?;
        return Ok(found.len());
    }

    Ok(0)
}

/// download whatever the policy says to, concurrently.
pub async fn run_download(
    library: &Library,
    yandex: &yamuse::Client,
    journal: &mut Journal,
    ui: &Ui,
    paths: &Paths,
    policy: DownloadPolicy,
    format: Format,
    concurrency: usize,
) -> Result<(Vec<DownloadedRow>, Vec<ErrorRow>)> {
    let targets: Vec<&SourceTrack> = library
        .tracks
        .iter()
        .filter(|t| {
            if journal.state.downloaded.contains_key(&t.id) {
                return false;
            }
            match policy {
                DownloadPolicy::Everything => true,
                DownloadPolicy::Unmatched => {
                    journal.state.unmatched.contains_key(&t.id)
                        || journal.state.download_forced.contains(&t.id)
                }
                DownloadPolicy::Never => journal.state.download_forced.contains(&t.id),
            }
        })
        .filter(|t| t.available)
        .collect();

    let mut rows = Vec::new();
    let mut errors = Vec::new();

    if targets.is_empty() {
        return Ok((rows, errors));
    }

    let downloader = Downloader::new(yandex, &paths.music, format);
    let bar = ui.bar(targets.len(), "скачивание");

    let mut stream = futures::stream::iter(targets.iter().map(|track| {
        let downloader = &downloader;
        async move { (*track, downloader.fetch(track).await) }
    }))
    .buffer_unordered(concurrency.max(1));

    let mut last_flush = Instant::now();

    while let Some((track, result)) = stream.next().await {
        match result {
            Ok(done) => {
                bar.set_message(truncate(&track.display(), 40));
                journal
                    .state
                    .downloaded
                    .insert(track.id.clone(), done.relative.clone());
                rows.push(DownloadedRow {
                    yandex_id: track.id.clone(),
                    source: track.display(),
                    path: done.relative,
                    codec: done.codec.to_string(),
                    bytes: done.bytes,
                });
            }
            Err(e) => {
                journal.state.download_failed.insert(track.id.clone());
                errors.push(ErrorRow {
                    phase: "download".into(),
                    item: track.display(),
                    error: e.to_string(),
                });
            }
        }

        bar.inc(1);
        if last_flush.elapsed() >= FLUSH_AFTER {
            journal.save()?;
            last_flush = Instant::now();
        }

        if crate::interrupt::requested() {
            break;
        }
    }

    bar.finish_and_clear();
    journal.save()?;

    if !rows.is_empty() {
        let entries: Vec<(String, &SourceTrack)> = rows
            .iter()
            .filter_map(|row| library.track(&row.yandex_id).map(|t| (row.path.clone(), t)))
            .collect();
        write_m3u(&paths.music.join("unmatched.m3u8"), &entries)?;
    }

    Ok((rows, errors))
}

/// the journal entry behind a report row.
fn record(row: &MatchedRow, spotify_id: &str) -> MatchRecord {
    MatchRecord {
        spotify_id: spotify_id.to_owned(),
        target: row.target.clone(),
        url: row.url.clone(),
        score: row.score,
        reviewed: row.reviewed,
    }
}

/// build the report row for a match.
fn matched_row(
    track: &SourceTrack,
    candidate: &Candidate,
    score: Score,
    reviewed: bool,
) -> MatchedRow {
    MatchedRow {
        yandex_id: track.id.clone(),
        source: track.display(),
        target: candidate.display(),
        url: candidate.url.clone(),
        score: (score.total * 1000.0).round() / 1000.0,
        reviewed,
    }
}

/// build the report row for a track with no counterpart.
fn unmatched_row(track: &SourceTrack, best: Option<&(Candidate, Score)>) -> UnmatchedRow {
    UnmatchedRow {
        yandex_id: track.id.clone(),
        source: track.display(),
        duration: track.duration_hms(),
        best_score: best.map_or_else(String::new, |(_, s)| format!("{:.2}", s.total)),
        best_candidate: best.map_or_else(String::new, |(c, _)| c.display()),
        downloaded: false,
    }
}

/// shorten a label to fit beside a progress bar.
fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_owned();
    }
    let head: String = s.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{head}…")
}

/// drop repeats while keeping first-seen order.
///
/// two yandex playlists can point at the same spotify track, and the likes list
/// repeats one whenever the catalogue substituted it. a duplicate is not an
/// error, but it wastes a slot in every batch of fifty.
fn dedup(ids: impl Iterator<Item = String>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();

    for id in ids {
        if seen.insert(id.clone()) {
            out.push(id);
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_label_longer_than_the_bar_is_cut_with_an_ellipsis() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("abcdefghij", 5), "abcd…");
        // cut by characters, not bytes: cyrillic must not be sliced mid-glyph.
        assert_eq!(truncate("абвгдеёжзи", 5), "абвг…");
    }

    #[test]
    fn deduplication_keeps_the_first_occurrence_and_its_position() {
        let deduped = dedup(["b", "a", "b", "c"].into_iter().map(String::from));
        assert_eq!(deduped, vec!["b", "a", "c"]);
    }

    #[test]
    fn the_default_download_policy_only_takes_what_spotify_lacks() {
        assert_eq!(DownloadPolicy::default(), DownloadPolicy::Unmatched);
    }
}
