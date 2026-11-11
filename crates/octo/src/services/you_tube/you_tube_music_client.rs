//! YouTube Music search and radio, through the yt-dlp shim's `/ytm` endpoints.

use octo_core::common::song_identity::{SongMatchOptions, SongRef};
use octo_core::common::{SongIdentity, dotnet};
use octo_core::json::element::{ElementResult, enumerate_array, get_int32, try_get_property};
use octo_core::models::domain::Song;
use octo_core::settings::SettingsStore;
use octo_core::soulseek::{RoutingKind, SongLength, SoulseekRouting};
use serde_json::Value;
use tracing::debug;

use super::YouTubeResolver;
use super::you_tube_resolver::Failure;

/// One row of YouTube Music, as the shim reads it through ytmusicapi.
///
/// `video_type`: MUSIC_VIDEO_TYPE_ATV (a release), OMV (official video), UGC (an upload) or
/// PODCAST_EPISODE; None when YouTube Music did not say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct YtmRow {
    pub video_id: String,
    pub title: String,
    pub artists: Vec<String>,
    pub album: Option<String>,
    pub duration_seconds: Option<i32>,
    pub video_type: Option<String>,
}

pub struct YouTubeMusicClient {
    /// `yt-dlp-shim-search`, the shim's 60-second client.
    http: reqwest::Client,
    shim_url: Option<String>,
}

impl YouTubeMusicClient {
    /// Reads `YouTube:ShimUrl` once, like YouTubeResolver: the shim's address changes only with a
    /// restart.
    pub fn new(settings: &SettingsStore) -> Self {
        Self::with_shim_url(settings.raw("YouTube:ShimUrl").as_deref())
    }

    pub fn with_shim_url(shim_url: Option<&str>) -> Self {
        YouTubeMusicClient {
            http: reqwest::Client::builder()
                .timeout(YouTubeResolver::SEARCH_TIMEOUT)
                .no_gzip()
                .no_deflate()
                .build()
                .expect("a plain HTTP client builds"),
            shim_url: shim_url.map(|url| url.trim_end_matches('/').to_string()),
        }
    }

    pub fn configured(&self) -> bool {
        self.shim_url.as_deref().is_some_and(|url| !url.is_empty())
    }

    /// `filter`: "songs" for releases, "videos" for uploads. The rows; `Ok(None)` when the shim
    /// could not answer, so a caller never keeps that as "nothing found". `Err` when the shim is
    /// unreachable, or for an answer that is not its JSON.
    pub async fn search(&self, query: &str, filter: &str) -> anyhow::Result<Option<Vec<YtmRow>>> {
        Ok(match self.search_limited(query, filter, 10).await? {
            YtmAnswer::Rows(rows) => Some(rows),
            YtmAnswer::Busy | YtmAnswer::Failed => None,
        })
    }

    /// [`Self::search`] for at most `limit` rows, telling a shim that said it is busy (HTTP 503
    /// with `{"error":"busy"}`) from any other failure to answer.
    pub async fn search_limited(&self, query: &str, filter: &str, limit: i32) -> anyhow::Result<YtmAnswer> {
        self.get_answer(&format!(
            "/ytm/search?q={}&filter={filter}&limit={limit}",
            dotnet::escape_data_string(query)
        ))
        .await
        .map(|answer| match answer {
            Answer::Rows(_, rows) => YtmAnswer::Rows(rows),
            Answer::Busy => YtmAnswer::Busy,
            Answer::Failed => YtmAnswer::Failed,
        })
    }

    /// [`Self::search_limited`] as background work: the shim runs it on its own one-slot gate,
    /// never fills its cache, and answers busy while the slot is taken. The rows, or why the
    /// shim could not answer.
    pub async fn search_limited_background(
        &self,
        query: &str,
        filter: &str,
        limit: i32,
    ) -> Result<Vec<YtmRow>, Failure> {
        let Some(shim) = self.shim_url.as_deref().filter(|url| !url.is_empty()) else {
            return Err(Failure::Other);
        };
        let url = format!(
            "{shim}/ytm/search?q={}&filter={filter}&limit={limit}&bg=1",
            dotnet::escape_data_string(query)
        );
        let response = self.http.get(url).send().await.map_err(|e| {
            if e.is_connect() {
                Failure::Unreachable
            } else {
                Failure::Other
            }
        })?;
        let status = response.status();
        let body = response.text().await.map_err(|_| Failure::Other)?;
        let root: Option<Value> = serde_json::from_str(&body).ok();
        if status.as_u16() == 503
            && root.as_ref().and_then(|r| r.get("error")).and_then(Value::as_str) == Some("busy")
        {
            return Err(Failure::Busy);
        }
        if !status.is_success() {
            debug!("YouTube Music background search answered {}", status.as_u16());
            return Err(Failure::Other);
        }
        rows(&root.ok_or(Failure::Other)?).map_err(|_| Failure::Other)
    }

    pub async fn radio(&self, video_id: &str, limit: i32) -> anyhow::Result<Vec<YtmRow>> {
        Ok(self
            .get_rows(&format!(
                "/ytm/radio?videoId={}&limit={limit}",
                dotnet::escape_data_string(video_id)
            ))
            .await?
            .map(|(_, rows)| rows)
            .unwrap_or_default())
    }

    /// The radio YouTube Music offers for an artist, found by name, and the name it found.
    pub async fn artist_radio(
        &self,
        artist: &str,
        limit: i32,
    ) -> anyhow::Result<(Option<String>, Vec<YtmRow>)> {
        let answer = self
            .get_rows(&format!(
                "/ytm/artist-radio?q={}&limit={limit}",
                dotnet::escape_data_string(artist)
            ))
            .await?;
        Ok(match answer {
            Some((root, rows)) => {
                let found = match &root {
                    Value::Object(map) => map.get("artist").and_then(Value::as_str).map(str::to_string),
                    _ => None,
                };
                (found, rows)
            }
            None => (None, Vec::new()),
        })
    }

    async fn get_rows(&self, path_and_query: &str) -> anyhow::Result<Option<(Value, Vec<YtmRow>)>> {
        Ok(match self.get_answer(path_and_query).await? {
            Answer::Rows(root, rows) => Some((root, rows)),
            Answer::Busy | Answer::Failed => None,
        })
    }

    async fn get_answer(&self, path_and_query: &str) -> anyhow::Result<Answer> {
        let Some(shim) = self.shim_url.as_deref().filter(|url| !url.is_empty()) else {
            return Ok(Answer::Failed);
        };
        let response = self.http.get(format!("{shim}{path_and_query}")).send().await?;
        let status = response.status();
        if !status.is_success() {
            debug!(
                "YouTube Music {} answered {}",
                path_and_query.split('?').next().unwrap_or(""),
                status.as_u16()
            );
            let busy = status.as_u16() == 503
                && response
                    .text()
                    .await
                    .ok()
                    .and_then(|body| serde_json::from_str::<Value>(&body).ok())
                    .is_some_and(|root| root.get("error").and_then(Value::as_str) == Some("busy"));
            return Ok(if busy { Answer::Busy } else { Answer::Failed });
        }
        let root: Value = serde_json::from_str(&response.text().await?)?;
        let rows = rows(&root)?;
        Ok(Answer::Rows(root, rows))
    }
}

/// What the shim's YouTube Music search came to.
#[derive(Debug, Clone, PartialEq)]
pub enum YtmAnswer {
    /// It answered; the rows may be none.
    Rows(Vec<YtmRow>),
    /// It said it is busy (HTTP 503 with `{"error":"busy"}`).
    Busy,
    /// It could not answer: no address, or a non-2xx.
    Failed,
}

enum Answer {
    Rows(Value, Vec<YtmRow>),
    Busy,
    Failed,
}

/// The `tracks` of an answer, each with a video id and a title.
pub fn rows(root: &Value) -> ElementResult<Vec<YtmRow>> {
    let Some(tracks @ Value::Array(_)) = try_get_property(root, "tracks")? else {
        return Ok(Vec::new());
    };
    let mut rows = Vec::new();
    for row in enumerate_array(tracks)? {
        let (Some(id), Some(title)) = (text(row, "videoId")?, text(row, "title")?) else {
            continue;
        };
        let artists = match try_get_property(row, "artists")? {
            Some(Value::Array(list)) => list
                .iter()
                .filter_map(Value::as_str)
                .filter(|name| !name.is_empty())
                .map(str::to_string)
                .collect(),
            _ => Vec::new(),
        };
        let seconds = match try_get_property(row, "durationSeconds")? {
            Some(value @ Value::Number(_)) => Some(get_int32(value)?),
            _ => None,
        };
        rows.push(YtmRow {
            video_id: id,
            title,
            artists,
            album: text(row, "album")?,
            duration_seconds: seconds,
            video_type: text(row, "videoType")?,
        });
    }
    Ok(rows)
}

/// A `YtmRow` (from either filter, `songs` or `videos`) as the search pipeline takes it: the
/// song the row becomes, and the routing minted with the row's video and its length from the
/// start — the video and the length are part of the registry id, so the row is seed-stable and
/// nothing later replaces them (D6/D13). A catalog row is the contrast: it mints with no video
/// and resolves one lazily. The song's `id` is empty here; the registry fills it when it
/// registers the routing. The first artist is the main one and the rest follow it, as
/// [`pick_song`] reads the row's artists.
pub fn song_and_routing(row: &YtmRow) -> (Song, SoulseekRouting) {
    let artist = row.artists.join(", ");
    let routing = SoulseekRouting {
        kind: RoutingKind::Song,
        you_tube_id: Some(row.video_id.clone()),
        artist: Some(artist.clone()),
        title: Some(row.title.clone()),
        album: row.album.clone(),
        duration: row.duration_seconds,
        ..Default::default()
    };
    (
        Song {
            artist,
            title: row.title.clone(),
            album: row.album.clone().unwrap_or_default(),
            duration: row.duration_seconds,
            is_local: false,
            ..Default::default()
        },
        routing,
    )
}

/// `video_type` of a release's own audio track.
pub const RELEASE: &str = "MUSIC_VIDEO_TYPE_ATV";
/// `video_type` of a podcast episode, which is never a song.
pub const PODCAST_EPISODE: &str = "MUSIC_VIDEO_TYPE_PODCAST_EPISODE";
/// How far a row's length may sit from the known length and still be the same recording.
pub const SAME_LENGTH_SECONDS: i32 = 5;

/// The row of a songs search that is the song `artist - title`: the same title and artist
/// ([`SongIdentity::same`]), a length that could be one song, within [`SAME_LENGTH_SECONDS`] of
/// `length` when that is known, and no podcast episode. Of several, a release's own audio track
/// comes first, then the closest length, then YouTube Music's order.
pub fn pick_song<'a>(
    artist: &str,
    title: &str,
    length: Option<i32>,
    rows: &'a [YtmRow],
) -> Option<&'a YtmRow> {
    let options = SongMatchOptions {
        length_tolerance_seconds: Some(SAME_LENGTH_SECONDS),
        ..Default::default()
    };
    let length = length.filter(|&seconds| seconds > 0);
    let wanted = SongRef::new(title, artist).with_seconds(length.map(f64::from));
    rows.iter()
        .enumerate()
        .filter(|(_, row)| row.video_type.as_deref() != Some(PODCAST_EPISODE))
        .filter(|(_, row)| SongLength::sane_video_length(row.duration_seconds).is_some())
        .filter(|(_, row)| {
            let found = SongRef::new(row.title.clone(), row.artists.join(", "))
                .with_seconds(row.duration_seconds.map(f64::from));
            SongIdentity::same(&wanted, &found, Some(&options)).is_same()
        })
        .min_by_key(|(index, row)| {
            let not_release = row.video_type.as_deref() != Some(RELEASE);
            let apart = match (length, row.duration_seconds) {
                (Some(want), Some(got)) => (want - got).abs(),
                _ => 0,
            };
            (not_release, apart, *index)
        })
        .map(|(_, row)| row)
}

/// A non-empty string property.
fn text(element: &Value, name: &str) -> ElementResult<Option<String>> {
    Ok(match try_get_property(element, name)? {
        Some(Value::String(text)) if !text.is_empty() => Some(text.clone()),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// Rows without an id or a title are left out, odd artists are skipped, and a fractional
    /// length fails the answer.
    #[test]
    fn rows_are_read_as_the_shim_writes_them() {
        let read = rows(&json!({"tracks": [
            {"videoId": "v1", "title": "One", "artists": ["A", "", 3, "B"], "durationSeconds": 200, "videoType": "MUSIC_VIDEO_TYPE_ATV", "album": "X"},
            {"videoId": "", "title": "No id"},
            {"videoId": "v3", "title": "Three", "durationSeconds": null}
        ]}))
        .expect("reads");
        assert_eq!(read.len(), 2);
        assert_eq!(read[0].artists, ["A", "B"]);
        assert_eq!(read[0].duration_seconds, Some(200));
        assert_eq!(read[0].album.as_deref(), Some("X"));
        assert_eq!(read[1].duration_seconds, None);
        assert!(rows(&json!({"tracks": [{"videoId": "v", "title": "T", "durationSeconds": 1.5}]})).is_err());
        assert!(rows(&json!({"tracks": "none"})).expect("reads").is_empty());
        assert!(rows(&json!([])).is_err());
        assert!(!YouTubeMusicClient::with_shim_url(None).configured());
        assert!(!YouTubeMusicClient::with_shim_url(Some("")).configured());
        assert!(YouTubeMusicClient::with_shim_url(Some("http://shim:8080/")).configured());
    }

    fn row(id: &str, title: &str, artist: &str, seconds: Option<i32>, kind: Option<&str>) -> YtmRow {
        YtmRow {
            video_id: id.into(),
            title: title.into(),
            artists: vec![artist.into()],
            album: None,
            duration_seconds: seconds,
            video_type: kind.map(str::to_string),
        }
    }

    /// The release's own audio track wins over an official video of the same song, even one
    /// listed first and closer in length.
    #[test]
    fn a_release_track_is_picked_before_a_music_video() {
        let rows = [
            row("omv", "Hello", "Adele", Some(366), Some("MUSIC_VIDEO_TYPE_OMV")),
            row("atv", "Hello", "Adele", Some(295), Some(RELEASE)),
        ];
        assert_eq!(
            pick_song("Adele", "Hello", Some(296), &rows).map(|r| r.video_id.as_str()),
            Some("atv")
        );
        assert_eq!(
            pick_song("Adele", "Hello", None, &rows).map(|r| r.video_id.as_str()),
            Some("atv")
        );
    }

    /// Of two releases of the song, the one closer to the known length.
    #[test]
    fn of_two_releases_the_closer_length_is_picked() {
        let rows = [
            row("far", "Teardrop", "Massive Attack", Some(334), Some(RELEASE)),
            row("near", "Teardrop", "Massive Attack", Some(331), Some(RELEASE)),
        ];
        assert_eq!(
            pick_song("Massive Attack", "Teardrop", Some(330), &rows).map(|r| r.video_id.as_str()),
            Some("near")
        );
    }

    /// A row more than five seconds off the known length, another song, another artist, a
    /// podcast or a length no song has is no pick, so the caller falls back.
    #[test]
    fn a_row_that_is_not_the_same_song_is_no_pick() {
        let off = [row("long", "Humble", "Kendrick Lamar", Some(190), Some(RELEASE))];
        assert!(pick_song("Kendrick Lamar", "Humble", Some(177), &off).is_none());
        assert!(pick_song("Kendrick Lamar", "Humble", Some(186), &off).is_some());
        let others = [
            row("a", "Humble Pie", "Kendrick Lamar", Some(177), Some(RELEASE)),
            row("b", "Humble", "Someone Else", Some(177), Some(RELEASE)),
            row("c", "Humble", "Kendrick Lamar", Some(177), Some(PODCAST_EPISODE)),
            row("d", "Humble", "Kendrick Lamar", Some(3), Some(RELEASE)),
            row("e", "Humble", "Kendrick Lamar", None, Some(RELEASE)),
            row("f", "Humble (Live)", "Kendrick Lamar", Some(177), Some(RELEASE)),
        ];
        assert!(pick_song("Kendrick Lamar", "Humble", Some(177), &others).is_none());
        assert!(pick_song("Kendrick Lamar", "Humble", None, &[]).is_none());
    }
}
