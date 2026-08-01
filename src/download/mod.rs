//! pulling audio off yandex for the tracks spotify does not carry.
//!
//! this exists because the web api has no way to put a file into a spotify
//! library — "local files" are a desktop-client feature and are indexed from a
//! folder, never uploaded. so the tracks with no counterpart are written to
//! disk in a layout that client will read, tagged well enough to be usable, and
//! listed in an m3u8 next to them.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use lofty::config::WriteOptions;
use lofty::file::TaggedFileExt;
use lofty::picture::{MimeType, Picture, PictureType};
use lofty::probe::Probe;
use lofty::tag::{Accessor, ItemKey, Tag, TagExt};
use yamuse::Client;
use yamuse::download::{Downloader as Stream, Strategy};
use yamuse::models::track::DownloadInfo;

use crate::error::{Error, Result, io};
use crate::model::SourceTrack;

/// quality yandex is asked for on the modern endpoint.
const LOSSLESS_QUALITY: &str = "lossless";
/// cover size to embed. large enough for a desktop client, small enough that
/// a few thousand of them do not dominate the download.
const COVER_SIZE: &str = "600x600";

/// which formats to accept, in order of preference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Format {
    /// try flac, fall back to the best mp3. the default.
    #[default]
    FlacThenMp3,
    /// flac only — a track without lossless access is reported, not downgraded.
    Flac,
    /// mp3 only, skipping the signed lossless endpoint entirely.
    Mp3,
}

/// one finished download.
#[derive(Debug, Clone)]
pub struct Downloaded {
    /// path relative to the music directory, as recorded in the journal.
    pub relative: String,
    /// size on disk.
    pub bytes: u64,
    /// what was actually served.
    pub codec: &'static str,
}

/// downloads tracks into a tagged, browsable directory tree.
pub struct Downloader<'a> {
    client: &'a Client,
    http: reqwest::Client,
    root: PathBuf,
    format: Format,
}

impl<'a> Downloader<'a> {
    /// build a downloader writing under `root`.
    pub fn new(client: &'a Client, root: impl Into<PathBuf>, format: Format) -> Self {
        Self {
            client,
            http: reqwest::Client::new(),
            root: root.into(),
            format,
        }
    }

    /// fetch one track, tag it, and return where it landed.
    ///
    /// a file already in place with a plausible size is left alone, which is
    /// what makes rerunning an interrupted download cheap.
    pub async fn fetch(&self, track: &SourceTrack) -> Result<Downloaded> {
        let dir = self.root.join(relative_dir(track));
        std::fs::create_dir_all(&dir).map_err(io(&dir))?;

        let stem = file_stem(track);

        for codec in ["flac", "mp3"] {
            let path = dir.join(format!("{stem}.{codec}"));
            if let Some(bytes) = already_complete(&path) {
                return Ok(Downloaded {
                    relative: relative_to(&self.root, &path),
                    bytes,
                    codec: if codec == "flac" { "flac" } else { "mp3" },
                });
            }
        }

        let (codec, bytes, path) = self.download(track, &dir, &stem).await?;

        // tagging failing must not throw away a file that took a minute to
        // fetch: the audio is correct either way and the tags can be redone.
        if let Err(e) = self.tag(&path, track).await {
            tracing::warn!(track = %track.display(), %e, "could not write tags");
        }

        Ok(Downloaded {
            relative: relative_to(&self.root, &path),
            bytes,
            codec,
        })
    }

    /// fetch the audio itself, trying the formats the policy allows.
    async fn download(
        &self,
        track: &SourceTrack,
        dir: &Path,
        stem: &str,
    ) -> Result<(&'static str, u64, PathBuf)> {
        let mut lossless_error = None;

        if self.format != Format::Mp3 {
            match self.download_lossless(track, dir, stem).await {
                Ok(result) => return Ok(result),
                Err(e) if self.format == Format::Flac => return Err(e),
                Err(e) => {
                    tracing::info!(track = %track.display(), %e, "no lossless stream, trying mp3");
                    lossless_error = Some(e);
                }
            }
        }

        self.download_mp3(track, dir, stem)
            .await
            .map_err(|mp3_err| {
                // the lossless failure is the more informative one — "no plus
                // subscription" explains both — so it is what the report gets.
                lossless_error.unwrap_or(mp3_err)
            })
    }

    /// the modern signed endpoint: an aes-ctr encrypted flac stream.
    async fn download_lossless(
        &self,
        track: &SourceTrack,
        dir: &Path,
        stem: &str,
    ) -> Result<(&'static str, u64, PathBuf)> {
        let info = self
            .client
            .track_lossless_info(&track.id, LOSSLESS_QUALITY)
            .await?;

        if !info.is_lossless() {
            return Err(Error::Config(format!(
                "yandex served {} rather than flac",
                info.codec.as_deref().unwrap_or("nothing")
            )));
        }

        let links = info.links();
        if links.is_empty() {
            return Err(Error::Config(
                "the lossless response carried no urls".into(),
            ));
        }

        let path = dir.join(format!("{stem}.flac"));
        let bytes = Stream::new(self.client.transport(), links)
            .maybe_encraw_key(info.key.clone())
            // racing the mirrors matters here: one slow cdn host otherwise sets
            // the pace for the whole queue.
            .strategy(Strategy::Race)
            .to_file(&path)
            .await?;

        Ok(("flac", bytes, path))
    }

    /// the legacy endpoint: pick the best mp3 variant and resolve its link.
    async fn download_mp3(
        &self,
        track: &SourceTrack,
        dir: &Path,
        stem: &str,
    ) -> Result<(&'static str, u64, PathBuf)> {
        let variants = self.client.track_download_info(&track.id).await?;

        let best = best_mp3(&variants)
            .ok_or_else(|| Error::Config("yandex offers no mp3 variant for this track".into()))?;

        // the resolved link is good for about a minute, so it is fetched here
        // rather than anywhere earlier in the pipeline.
        let link = best.direct_link(self.client.transport()).await?;

        let path = dir.join(format!("{stem}.mp3"));
        let bytes = Stream::new(self.client.transport(), [link])
            .to_file(&path)
            .await?;

        Ok(("mp3", bytes, path))
    }

    /// write metadata and cover art into the finished file.
    async fn tag(&self, path: &Path, track: &SourceTrack) -> Result<()> {
        let cover = self.cover(track).await;

        let path = path.to_path_buf();
        let track = track.clone();

        // lofty is synchronous and rewrites the file in place, which on a flac
        // means moving megabytes. off the runtime thread it goes.
        tokio::task::spawn_blocking(move || write_tags(&path, &track, cover))
            .await
            .map_err(|e| Error::Config(format!("tagging task failed: {e}")))?
    }

    /// fetch the cover art, or `None` if there is none or it will not load.
    async fn cover(&self, track: &SourceTrack) -> Option<Vec<u8>> {
        let uri = track.cover_uri.as_deref()?;
        let url = yamuse::models::normalize_image_url(uri, COVER_SIZE);

        let response = self.http.get(&url).send().await.ok()?;
        if !response.status().is_success() {
            return None;
        }

        response.bytes().await.ok().map(|b| b.to_vec())
    }
}

/// write tags and cover art. synchronous, and meant for `spawn_blocking`.
fn write_tags(path: &Path, track: &SourceTrack, cover: Option<Vec<u8>>) -> Result<()> {
    let mut file = Probe::open(path)?.read()?;

    let kind = file.primary_tag_type();
    if file.primary_tag().is_none() {
        file.insert_tag(Tag::new(kind));
    }
    let Some(tag) = file.primary_tag_mut() else {
        return Ok(());
    };

    tag.set_title(track.full_title());
    tag.set_artist(track.artists.join(", "));
    if let Some(album) = &track.album {
        tag.set_album(album.clone());
    }
    if let Some(year) = track.year.filter(|y| *y > 0) {
        // written as a bare year rather than through `set_date`: yandex only
        // ever knows the year, and a fabricated january 1st would be a lie the
        // tag cannot walk back.
        tag.insert_text(ItemKey::Year, year.to_string());
    }
    if let Some(number) = track.track_number {
        if let Ok(number) = u32::try_from(number) {
            tag.set_track(number);
        }
    }
    if let Some(disc) = track.disc_number {
        if let Ok(disc) = u32::try_from(disc) {
            tag.set_disk(disc);
        }
    }
    if let Some(genre) = &track.genre {
        tag.set_genre(genre.clone());
    }
    if let Some(album_artist) = &track.album_artist {
        tag.insert_text(ItemKey::AlbumArtist, album_artist.clone());
    }

    if let Some(bytes) = cover {
        let mime = sniff_mime(&bytes);
        let mut picture = Picture::unchecked(bytes).pic_type(PictureType::CoverFront);
        if let Some(mime) = mime {
            picture = picture.mime_type(mime);
        }
        tag.push_picture(picture.build());
    }

    tag.save_to_path(path, WriteOptions::default())?;
    Ok(())
}

/// the highest-bitrate mp3 among the offered variants.
///
/// previews are excluded: they are 30 seconds long and would otherwise win
/// whenever yandex lists one at a higher bitrate than the full track.
pub fn best_mp3(variants: &[DownloadInfo]) -> Option<&DownloadInfo> {
    variants
        .iter()
        .filter(|v| v.codec.as_deref() == Some("mp3"))
        .filter(|v| !v.preview.unwrap_or(false))
        .max_by_key(|v| v.bitrate_in_kbps.unwrap_or(0))
}

/// guess an image mime type from its first bytes.
fn sniff_mime(bytes: &[u8]) -> Option<MimeType> {
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some(MimeType::Jpeg)
    } else if bytes.starts_with(b"\x89PNG") {
        Some(MimeType::Png)
    } else {
        None
    }
}

/// whether a file is already there and big enough to be real audio.
///
/// the size check catches the one failure mode that matters — an interrupted
/// write leaving a few kilobytes behind, which would otherwise be treated as a
/// finished download forever.
fn already_complete(path: &Path) -> Option<u64> {
    const MIN_PLAUSIBLE_BYTES: u64 = 64 * 1024;

    let size = std::fs::metadata(path).ok()?.len();
    (size >= MIN_PLAUSIBLE_BYTES).then_some(size)
}

/// `artist/album` for a track, sanitised for the filesystem.
pub fn relative_dir(track: &SourceTrack) -> PathBuf {
    let artist = track
        .album_artist
        .clone()
        .or_else(|| track.artists.first().cloned())
        .unwrap_or_else(|| "Unknown Artist".into());

    let album = track
        .album
        .clone()
        .unwrap_or_else(|| "Unknown Album".into());

    PathBuf::from(sanitize(&artist)).join(sanitize(&album))
}

/// `NN - Title` for a track, sanitised for the filesystem.
pub fn file_stem(track: &SourceTrack) -> String {
    let title = sanitize(&track.full_title());
    let title = if title.is_empty() {
        format!("track {}", track.id)
    } else {
        title
    };

    match track.track_number {
        Some(n) if n > 0 => format!("{n:02} - {title}"),
        _ => title,
    }
}

/// make a string safe as a single path component.
///
/// the reserved set is the union of what apfs, ntfs and ext4 object to, so the
/// same tree copies between them without renaming. length is capped well under
/// the 255-byte component limit, since a cyrillic title costs two bytes a
/// character and the extension still has to fit.
fn sanitize(s: &str) -> String {
    const MAX_CHARS: usize = 80;

    let cleaned: String = s
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => ' ',
            c => c,
        })
        .collect();

    let cleaned = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let truncated: String = cleaned.chars().take(MAX_CHARS).collect();

    // a component ending in a dot or space is legal on unix and rejected on
    // windows, and trailing dots make files invisible to some tools.
    truncated.trim_matches(['.', ' ']).to_string()
}

/// render `path` relative to `root`, for the journal and the m3u8.
fn relative_to(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// write an m3u8 listing every downloaded file, in the order given.
///
/// pointed at the music directory, a desktop spotify client indexes these files
/// as "local files"; the playlist is what makes them usable as a set rather
/// than a folder to browse.
pub fn write_m3u(path: &Path, entries: &[(String, &SourceTrack)]) -> Result<()> {
    let mut out = String::from("#EXTM3U\n");

    for (relative, track) in entries {
        let seconds = track.duration_ms.unwrap_or(0) / 1000;
        // writing into a `String` is infallible, so there is no error to report.
        let _ = writeln!(
            out,
            "#EXTINF:{seconds},{} - {}\n{relative}",
            track.artists.join(", "),
            track.full_title()
        );
    }

    std::fs::write(path, out).map_err(io(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track() -> SourceTrack {
        SourceTrack {
            id: "42".into(),
            title: "Song".into(),
            artists: vec!["Artist".into()],
            album: Some("Album".into()),
            track_number: Some(3),
            ..Default::default()
        }
    }

    #[test]
    fn a_path_component_loses_every_character_some_filesystem_rejects() {
        assert_eq!(sanitize("AC/DC: Back?"), "AC_DC_ Back_");
        assert_eq!(sanitize("  spaced   out  "), "spaced out");
        assert_eq!(sanitize("trailing."), "trailing");
    }

    #[test]
    fn a_component_is_capped_short_of_the_filesystem_limit() {
        let long = "я".repeat(300);
        let sanitized = sanitize(&long);
        assert_eq!(sanitized.chars().count(), 80);
        // cyrillic is two bytes a character, so the byte length is what matters.
        assert!(sanitized.len() < 255);
    }

    #[test]
    fn the_layout_is_artist_album_and_a_numbered_title() {
        assert_eq!(
            relative_dir(&track()),
            PathBuf::from("Artist").join("Album")
        );
        assert_eq!(file_stem(&track()), "03 - Song");
    }

    #[test]
    fn a_track_without_a_number_or_album_still_gets_a_usable_path() {
        let mut t = track();
        t.track_number = None;
        t.album = None;
        t.artists.clear();

        assert_eq!(
            relative_dir(&t),
            PathBuf::from("Unknown Artist").join("Unknown Album")
        );
        assert_eq!(file_stem(&t), "Song");
    }

    #[test]
    fn a_track_whose_title_sanitises_to_nothing_falls_back_to_its_id() {
        let mut t = track();
        t.title = "///".into();
        t.track_number = None;
        assert_eq!(file_stem(&t), "___");

        t.title = "...".into();
        assert_eq!(file_stem(&t), "track 42");
    }

    #[test]
    fn the_best_mp3_is_the_highest_bitrate_full_variant_not_a_preview() {
        let variants: Vec<DownloadInfo> = serde_json::from_str(
            r#"[
                {"codec":"mp3","bitrateInKbps":128},
                {"codec":"mp3","bitrateInKbps":320},
                {"codec":"mp3","bitrateInKbps":320,"preview":true},
                {"codec":"aac","bitrateInKbps":256}
            ]"#,
        )
        .unwrap();

        let best = best_mp3(&variants).unwrap();
        assert_eq!(best.bitrate_in_kbps, Some(320));
        assert_eq!(best.preview, None);
    }

    #[test]
    fn no_mp3_among_the_variants_is_reported_rather_than_guessed_at() {
        let variants: Vec<DownloadInfo> =
            serde_json::from_str(r#"[{"codec":"aac","bitrateInKbps":256}]"#).unwrap();
        assert!(best_mp3(&variants).is_none());
    }

    #[test]
    fn a_truncated_leftover_file_is_not_mistaken_for_a_finished_download() {
        let dir = std::env::temp_dir().join("yamuse2spotify-download-test");
        std::fs::create_dir_all(&dir).unwrap();

        let stub = dir.join("stub.flac");
        std::fs::write(&stub, b"not really audio").unwrap();
        assert!(already_complete(&stub).is_none());

        let full = dir.join("full.flac");
        std::fs::write(&full, vec![0u8; 128 * 1024]).unwrap();
        assert_eq!(already_complete(&full), Some(128 * 1024));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn image_bytes_are_identified_by_their_signature_not_by_the_url() {
        assert!(matches!(
            sniff_mime(&[0xFF, 0xD8, 0xFF, 0x00]),
            Some(MimeType::Jpeg)
        ));
        assert!(matches!(sniff_mime(b"\x89PNG\r\n"), Some(MimeType::Png)));
        assert!(sniff_mime(b"GIF89a").is_none());
    }

    #[test]
    fn the_m3u_carries_a_duration_and_a_relative_path_for_every_entry() {
        let dir = std::env::temp_dir().join("yamuse2spotify-m3u-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("list.m3u8");

        let mut t = track();
        t.duration_ms = Some(185_000);
        write_m3u(&path, &[("Artist/Album/03 - Song.flac".into(), &t)]).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("#EXTM3U"));
        assert!(text.contains("#EXTINF:185,Artist - Song"));
        assert!(text.contains("Artist/Album/03 - Song.flac"));

        std::fs::remove_dir_all(&dir).ok();
    }
}
