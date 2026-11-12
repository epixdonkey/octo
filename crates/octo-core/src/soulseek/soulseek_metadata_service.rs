//! The Soulseek metadata service's routing model: [`RoutingKind`] and [`SoulseekRouting`],
//! which `external-ids.json` carries (checked against the `external-ids.json` fixture).
//!
//! The service itself lives in `octo::services::soulseek::soulseek_metadata_service`.

use serde::ser::SerializeStruct;
use serde::{Deserialize, Serialize, Serializer};
use serde_repr::{Deserialize_repr, Serialize_repr};

use crate::soulseek::song_length::LengthSource;
use crate::soulseek::stream_facts::StreamFacts;

/// What a short external id stands for. Written as its number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize_repr, Deserialize_repr)]
#[repr(i32)]
pub enum RoutingKind {
    #[default]
    Song = 0,
    Album = 1,
    Artist = 2,
}

/// Where an outside song, album or artist came from, enough to find it again: what the short
/// external ids Octo hands to Subsonic clients stand for.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(remote = "Self", rename_all = "PascalCase", default)]
pub struct SoulseekRouting {
    pub kind: RoutingKind,
    pub you_tube_id: Option<String>,
    pub artist: Option<String>,
    pub title: Option<String>,
    pub album: Option<String>,
    pub duration: Option<i32>,

    /// Deezer album id, when an album search resolved one. Absent on album
    /// routings minted from a song row, which fall back to a name lookup.
    pub external_album_id: Option<String>,

    /// Deezer artist id behind an artist routing, once an artist search or an artist
    /// page settled on one. The id itself is minted from the name alone, and two artists can
    /// share a name, so this is what says which of them the page lists. Not part of the id.
    pub external_artist_id: Option<String>,

    /// Position within its album. Carried so a track downloaded as part of an
    /// album keeps its ordering: the download path rebuilds the song from its id alone,
    /// and without this every track lands untracked and sorts alphabetically.
    pub track: Option<i32>,

    /// Disc within a multi-disc release. Same reasoning as [`SoulseekRouting::track`].
    pub disc_number: Option<i32>,

    /// Track count of the album this came from. Without it the tagger fills the
    /// "x of y" denominator from a per-track Deezer search that can match a different
    /// release, producing nonsense like 5/10 on an 8-track album.
    pub total_tracks: Option<i32>,

    /// The track's ISRC, when the album listing that minted this routing named one.
    /// Carried for the same reason as [`SoulseekRouting::track`], and because it is the
    /// strongest evidence download verification can be given about which recording was asked
    /// for. Not part of the id, so routings minted before it existed keep their ids.
    pub isrc: Option<String>,

    /// The length shown for this song once a lookup found one, kept here so every
    /// later response carries it, across restarts too. Display only: see
    /// [`crate::soulseek::SongLength`] for why this is not [`SoulseekRouting::duration`].
    pub shown_duration: Option<i32>,

    /// Where [`SoulseekRouting::shown_duration`] came from, so a weaker source never
    /// replaces a stronger one.
    pub shown_duration_source: LengthSource,

    /// Whether the words are explicit (1), the clean edit (3) or neither (0), as the catalog
    /// said, so every later response for this id carries it. Display only, not part of the id
    /// (`Octo.Models.Domain.ExplicitStatus`).
    pub explicit_content: Option<i32>,

    /// "clean" for a clean edit, which gets an id of its own so it can be listed beside the
    /// explicit original. None for every other song or album, so every id minted before this
    /// existed (stars, playlists, pins) stays the same.
    pub version: Option<String>,

    /// Which discovery service suggested the row this routing names, as its own label
    /// ("YouTube Music (song)", "YouTube Music (video)", "YouTube") — the label the row's
    /// `octoSuggestedBy` and its genre badge read. Written when a YouTube-origin row is
    /// registered, so a playlist position added from the row can keep its source; display
    /// only, not part of the id (like [`SoulseekRouting::explicit_content`]), so ids minted
    /// before it existed keep their shape.
    pub suggested_by: Option<String>,

    /// The exact byte count of the pinned video's audio stream, once an answer named it. It
    /// belongs to [`SoulseekRouting::you_tube_id`]: pinning another video clears it. Not part of
    /// the id.
    pub stream_size: Option<u64>,

    /// Whether [`SoulseekRouting::stream_size`] came from bytes that were played. Written only
    /// when true, so a routing is written exactly as before until a play confirms its size.
    pub stream_size_confirmed: bool,

    /// The download replaces a file already in the library, whose take (an instrumental, a
    /// karaoke track, a cover) is the person's own: such a take named in the title is then
    /// fetched even while Skip covers, karaoke and instrumentals is on. Written only when true,
    /// and not part of the id.
    pub replaces_library_file: bool,

    /// The pinned video's audio format id (140 is AAC in mp4, 251 Opus in webm).
    pub stream_itag: Option<String>,

    /// The pinned video's audio container, as yt-dlp names it ("m4a", "webm").
    pub stream_ext: Option<String>,

    /// The pinned video's average audio bit rate in kbps.
    pub stream_bit_rate: Option<i32>,

    /// ReplayGain track gain in dB of the pinned video's audio, once a play measured it.
    pub track_gain: Option<f64>,

    /// ReplayGain track peak (linear) of the pinned video's audio, measured with the gain.
    pub track_peak: Option<f64>,

    /// Videos found to be another song than this one, newest last, so neither a play nor a
    /// download takes them again. At most [`Self::MAX_REFUSED_VIDEOS`].
    pub refused_videos: Vec<String>,

    /// Which ranking picked `you_tube_id`: [`Self::VIDEO_PICK`] for the current one,
    /// [`Self::VIDEO_NAMED`] for a video the song was named by, 0 for one picked before
    /// rankings were versioned.
    pub video_pick: u8,

    /// A video the current ranking prefers to `you_tube_id`, applied when a play of the song
    /// next starts at its first byte. Not persisted.
    #[serde(skip)]
    pub pending_video: Option<String>,
}

/// The gain and peak are plain measured numbers, never NaN, so equality is total.
impl Eq for SoulseekRouting {}

impl SoulseekRouting {
    /// How many refused videos a song remembers; the oldest goes first.
    pub const MAX_REFUSED_VIDEOS: usize = 5;

    /// The version of the ranking that picks a song's video today.
    pub const VIDEO_PICK: u8 = 3;

    /// The mark of a video the song was named by, which no ranking picked.
    pub const VIDEO_NAMED: u8 = u8::MAX;

    pub fn has_you_tube(&self) -> bool {
        self.you_tube_id.as_deref().is_some_and(|id| !id.is_empty())
    }

    /// What is known of the pinned video's stream.
    pub fn stream_facts(&self) -> StreamFacts {
        StreamFacts {
            size: self.stream_size,
            size_confirmed: self.stream_size_confirmed,
            itag: self.stream_itag.clone(),
            ext: self.stream_ext.clone(),
            bit_rate: self.stream_bit_rate,
            track_gain: self.track_gain,
            track_peak: self.track_peak,
        }
    }

    /// Takes what `facts` knows of the pinned video's stream, keeping what it does not.
    pub fn merge_stream_facts(&mut self, facts: &StreamFacts) {
        let mut known = self.stream_facts();
        known.merge(facts);
        self.stream_size = known.size;
        self.stream_size_confirmed = known.size_confirmed;
        self.stream_itag = known.itag;
        self.stream_ext = known.ext;
        self.stream_bit_rate = known.bit_rate;
        self.track_gain = known.track_gain;
        self.track_peak = known.track_peak;
    }

    /// Pins `video` for playback. A video other than the pinned one forgets the old one's
    /// stream facts, which describe a different stream. A refused video is never pinned. True
    /// when the pin changed.
    pub fn pin_video(&mut self, video: &str) -> bool {
        if self.you_tube_id.as_deref() == Some(video) || self.refuses_video(video) {
            return false;
        }
        self.you_tube_id = Some(video.to_string());
        self.video_pick = 0;
        self.stream_size = None;
        self.stream_size_confirmed = false;
        self.stream_itag = None;
        self.stream_ext = None;
        self.stream_bit_rate = None;
        self.track_gain = None;
        self.track_peak = None;
        true
    }

    /// Forgets `video` as the song's video, what was known of its stream, and a length learned
    /// from it. False when the song plays another video, or none.
    pub fn unpin_video(&mut self, video: &str) -> bool {
        if self.you_tube_id.as_deref() != Some(video) {
            return false;
        }
        // A length learned from this video goes with it; the song's own length (catalog or
        // file) takes its place.
        if self.shown_duration_source == LengthSource::Video {
            if self.duration.is_some() && self.duration == self.shown_duration {
                self.duration = None;
            }
            self.shown_duration = None;
            self.shown_duration_source = LengthSource::None;
        } else if self.shown_duration_source >= LengthSource::LastFm
            && let Some(known) = self.shown_duration.filter(|&s| s > 0)
        {
            // The catalog's or the file's length stands for the song again.
            self.duration = Some(known);
        }
        self.you_tube_id = None;
        self.video_pick = 0;
        self.stream_size = None;
        self.stream_size_confirmed = false;
        self.stream_itag = None;
        self.stream_ext = None;
        self.stream_bit_rate = None;
        self.track_gain = None;
        self.track_peak = None;
        true
    }

    /// Unpins `video` and remembers it as another song than this one, so it is never pinned
    /// again. True when anything changed.
    pub fn refuse_video(&mut self, video: &str) -> bool {
        if video.is_empty() {
            return false;
        }
        let unpinned = self.unpin_video(video);
        if self.refuses_video(video) {
            return unpinned;
        }
        self.refused_videos.push(video.to_string());
        if self.refused_videos.len() > Self::MAX_REFUSED_VIDEOS {
            self.refused_videos.remove(0);
        }
        true
    }

    /// Whether `video` was found to be another song than this one.
    pub fn refuses_video(&self, video: &str) -> bool {
        self.refused_videos.iter().any(|v| v == video)
    }

    /// Takes `others` into the refused videos, oldest first, keeping each once and at most
    /// [`Self::MAX_REFUSED_VIDEOS`].
    pub fn keep_refused_videos(&mut self, others: &[String]) {
        let mut all: Vec<String> = others.to_vec();
        for video in std::mem::take(&mut self.refused_videos) {
            if !all.contains(&video) {
                all.push(video);
            }
        }
        let excess = all.len().saturating_sub(Self::MAX_REFUSED_VIDEOS);
        self.refused_videos = all.split_off(excess);
    }

    pub fn has_artist_title(&self) -> bool {
        self.artist.as_deref().is_some_and(|a| !a.is_empty())
            && self.title.as_deref().is_some_and(|t| !t.is_empty())
    }
}

/// A length read from a downloaded file is written as Deezer's number with `ShownFromFile` beside
/// it, because a reader that knows only the first four sources refuses the whole file over a
/// number it does not know. Both that form and the plain number are read.
const SHOWN_FROM_FILE: &str = "ShownFromFile";

impl<'de> Deserialize<'de> for SoulseekRouting {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let value = serde_json::Value::deserialize(deserializer)?;
        let from_file = value.get(SHOWN_FROM_FILE).and_then(serde_json::Value::as_bool) == Some(true);
        let mut routing = SoulseekRouting::deserialize(value).map_err(D::Error::custom)?;
        if from_file && routing.shown_duration_source == LengthSource::Deezer {
            routing.shown_duration_source = LengthSource::File;
        }
        Ok(routing)
    }
}

/// By hand, because the computed `HasYouTube` and `HasArtistTitle` are written too (and
/// ignored again on read). The stream facts are written only when known, so a routing without
/// them is written exactly as before.
impl Serialize for SoulseekRouting {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let stream_fields = [
            self.stream_size.is_some(),
            self.stream_size_confirmed,
            self.replaces_library_file,
            self.stream_itag.is_some(),
            self.stream_ext.is_some(),
            self.stream_bit_rate.is_some(),
            self.track_gain.is_some(),
            self.track_peak.is_some(),
            !self.refused_videos.is_empty(),
            self.video_pick > 0,
        ]
        .into_iter()
        .filter(|known| *known)
        .count();
        let from_file = self.shown_duration_source == LengthSource::File;
        let mut s = serializer.serialize_struct(
            "SoulseekRouting",
            18 + stream_fields + usize::from(from_file) + usize::from(self.suggested_by.is_some()),
        )?;
        s.serialize_field("Kind", &self.kind)?;
        s.serialize_field("YouTubeId", &self.you_tube_id)?;
        s.serialize_field("Artist", &self.artist)?;
        s.serialize_field("Title", &self.title)?;
        s.serialize_field("Album", &self.album)?;
        s.serialize_field("Duration", &self.duration)?;
        s.serialize_field("ExternalAlbumId", &self.external_album_id)?;
        s.serialize_field("ExternalArtistId", &self.external_artist_id)?;
        s.serialize_field("Track", &self.track)?;
        s.serialize_field("DiscNumber", &self.disc_number)?;
        s.serialize_field("TotalTracks", &self.total_tracks)?;
        s.serialize_field("Isrc", &self.isrc)?;
        s.serialize_field("ShownDuration", &self.shown_duration)?;
        if from_file {
            s.serialize_field("ShownDurationSource", &LengthSource::Deezer)?;
        } else {
            s.serialize_field("ShownDurationSource", &self.shown_duration_source)?;
        }
        s.serialize_field("ExplicitContent", &self.explicit_content)?;
        s.serialize_field("Version", &self.version)?;
        if let Some(by) = &self.suggested_by {
            s.serialize_field("SuggestedBy", by)?;
        }
        if let Some(size) = &self.stream_size {
            s.serialize_field("StreamSize", size)?;
        }
        if self.stream_size_confirmed {
            s.serialize_field("StreamSizeConfirmed", &true)?;
        }
        if self.replaces_library_file {
            s.serialize_field("ReplacesLibraryFile", &true)?;
        }
        if let Some(itag) = &self.stream_itag {
            s.serialize_field("StreamItag", itag)?;
        }
        if let Some(ext) = &self.stream_ext {
            s.serialize_field("StreamExt", ext)?;
        }
        if let Some(bit_rate) = &self.stream_bit_rate {
            s.serialize_field("StreamBitRate", bit_rate)?;
        }
        if let Some(gain) = &self.track_gain {
            s.serialize_field("TrackGain", gain)?;
        }
        if let Some(peak) = &self.track_peak {
            s.serialize_field("TrackPeak", peak)?;
        }
        if !self.refused_videos.is_empty() {
            s.serialize_field("RefusedVideos", &self.refused_videos)?;
        }
        if self.video_pick > 0 {
            s.serialize_field("VideoPick", &self.video_pick)?;
        }
        if from_file {
            s.serialize_field(SHOWN_FROM_FILE, &true)?;
        }
        s.serialize_field("HasYouTube", &self.has_you_tube())?;
        s.serialize_field("HasArtistTitle", &self.has_artist_title())?;
        s.end()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unpinning_the_songs_video_forgets_it_and_its_stream() {
        let mut routing = SoulseekRouting {
            you_tube_id: Some("wrong".into()),
            stream_size: Some(2_366_424),
            track_gain: Some(-11.0),
            ..Default::default()
        };

        assert!(!routing.unpin_video("other"));
        assert_eq!(routing.you_tube_id.as_deref(), Some("wrong"));

        assert!(routing.unpin_video("wrong"));
        assert_eq!(routing.you_tube_id, None);
        assert_eq!((routing.stream_size, routing.track_gain), (None, None));
    }

    #[test]
    fn unpinning_forgets_a_length_learned_from_the_video() {
        let mut routing = SoulseekRouting {
            you_tube_id: Some("v".into()),
            shown_duration: Some(153),
            shown_duration_source: LengthSource::Video,
            duration: Some(153),
            ..Default::default()
        };
        assert!(routing.unpin_video("v"));
        assert_eq!(routing.shown_duration, None);
        assert_eq!(routing.shown_duration_source, LengthSource::None);
        assert_eq!(routing.duration, None);

        let mut catalog = SoulseekRouting {
            you_tube_id: Some("v".into()),
            shown_duration: Some(179),
            shown_duration_source: LengthSource::Deezer,
            duration: Some(179),
            ..Default::default()
        };
        assert!(catalog.unpin_video("v"));
        assert_eq!(catalog.shown_duration, Some(179));
        assert_eq!(catalog.shown_duration_source, LengthSource::Deezer);
        assert_eq!(catalog.duration, Some(179));
    }

    #[test]
    fn unpinning_puts_back_the_catalog_length() {
        let mut routing = SoulseekRouting {
            you_tube_id: Some("v".into()),
            duration: Some(171),
            shown_duration: Some(179),
            shown_duration_source: LengthSource::Deezer,
            ..Default::default()
        };
        assert!(routing.unpin_video("v"));
        assert_eq!(routing.duration, Some(179));
        assert_eq!(routing.shown_duration, Some(179));
        assert_eq!(routing.shown_duration_source, LengthSource::Deezer);
    }

    #[test]
    fn a_refused_video_is_never_pinned_again() {
        let mut routing = SoulseekRouting {
            you_tube_id: Some("v".into()),
            ..Default::default()
        };
        assert!(routing.refuse_video("v"));
        assert_eq!(routing.you_tube_id, None);
        assert!(routing.refuses_video("v"));
        assert!(!routing.pin_video("v"));
        assert_eq!(routing.you_tube_id, None);
        assert!(routing.pin_video("w"));
    }

    #[test]
    fn refused_videos_keep_each_once_and_the_newest_five() {
        let mut routing = SoulseekRouting::default();
        for video in ["a", "a", "b", "c", "d", "e", "f"] {
            routing.refuse_video(video);
        }
        assert_eq!(routing.refused_videos, ["b", "c", "d", "e", "f"]);
    }

    #[test]
    fn keeping_refused_videos_merges_oldest_first() {
        let mut routing = SoulseekRouting {
            refused_videos: vec!["c".into()],
            ..Default::default()
        };
        routing.keep_refused_videos(&["a".into(), "b".into(), "c".into()]);
        assert_eq!(routing.refused_videos, ["a", "b", "c"]);
    }

    #[test]
    fn refused_videos_are_written_only_when_there_are_some() {
        let none = serde_json::to_value(SoulseekRouting::default()).unwrap();
        assert!(none.get("RefusedVideos").is_none());

        let routing = SoulseekRouting {
            refused_videos: vec!["v".into()],
            ..Default::default()
        };
        let json = crate::json::to_string(&routing);
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["RefusedVideos"], serde_json::json!(["v"]));
        let back: SoulseekRouting = serde_json::from_str(&json).expect("reads back");
        assert_eq!(back.refused_videos, ["v"]);
    }

    #[test]
    fn the_video_pick_is_written_only_when_set_and_another_video_resets_it() {
        let none = serde_json::to_value(SoulseekRouting::default()).unwrap();
        assert!(none.get("VideoPick").is_none());

        let mut routing = SoulseekRouting {
            you_tube_id: Some("v".into()),
            video_pick: SoulseekRouting::VIDEO_PICK,
            ..Default::default()
        };
        let json = crate::json::to_string(&routing);
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["VideoPick"], serde_json::json!(3));
        let back: SoulseekRouting = serde_json::from_str(&json).expect("reads back");
        assert_eq!(back.video_pick, SoulseekRouting::VIDEO_PICK);

        assert!(!routing.pin_video("v"));
        assert_eq!(routing.video_pick, SoulseekRouting::VIDEO_PICK);
        assert!(routing.pin_video("w"));
        assert_eq!(routing.video_pick, 0);

        routing.video_pick = SoulseekRouting::VIDEO_NAMED;
        assert!(routing.unpin_video("w"));
        assert_eq!(routing.video_pick, 0);
    }

    #[test]
    fn the_computed_flags_are_written_and_ignored_on_read() {
        let routing = SoulseekRouting {
            you_tube_id: Some("abc".into()),
            artist: Some("A".into()),
            title: Some(String::new()),
            ..Default::default()
        };
        let json = crate::json::to_string(&routing);
        assert!(
            json.ends_with(
                r#""ShownDurationSource":0,"ExplicitContent":null,"Version":null,"HasYouTube":true,"HasArtistTitle":false}"#
            ),
            "{json}"
        );
        let back: SoulseekRouting = serde_json::from_str(&json).expect("reads back");
        assert_eq!(back, routing);
    }

    #[test]
    fn a_length_from_a_file_is_written_in_numbers_an_older_reader_knows() {
        let routing = SoulseekRouting {
            title: Some("Angel".into()),
            shown_duration: Some(380),
            shown_duration_source: LengthSource::File,
            ..Default::default()
        };
        let json = crate::json::to_string(&routing);
        assert!(json.contains(r#""ShownDurationSource":3,"#), "{json}");
        assert!(json.contains(r#""ShownFromFile":true"#), "{json}");
        assert!(!json.contains(r#""ShownDurationSource":4"#), "{json}");

        // Read back, the file's length is still the file's.
        let back: SoulseekRouting = serde_json::from_str(&json).expect("reads back");
        assert_eq!(back, routing);
        // The number an earlier build wrote reads too.
        let older: SoulseekRouting =
            serde_json::from_str(r#"{"Kind":0,"ShownDuration":380,"ShownDurationSource":4}"#).expect("reads");
        assert_eq!(older.shown_duration_source, LengthSource::File);
        // A Deezer length stays Deezer's, with no marker written.
        let deezer = SoulseekRouting {
            shown_duration_source: LengthSource::Deezer,
            ..Default::default()
        };
        let json = crate::json::to_string(&deezer);
        assert!(!json.contains("ShownFromFile"), "{json}");
        assert_eq!(
            serde_json::from_str::<SoulseekRouting>(&json).expect("reads"),
            deezer
        );
    }

    #[test]
    fn stream_facts_round_trip_and_a_routing_written_before_them_still_reads() {
        let routing = SoulseekRouting {
            you_tube_id: Some("abc".into()),
            stream_size: Some(6_755_585),
            stream_itag: Some("140".into()),
            stream_ext: Some("m4a".into()),
            stream_bit_rate: Some(129),
            ..Default::default()
        };
        let json = crate::json::to_string(&routing);
        assert!(
            json.contains(r#""StreamSize":6755585,"StreamItag":"140","StreamExt":"m4a","StreamBitRate":129"#),
            "{json}"
        );
        assert_eq!(
            serde_json::from_str::<SoulseekRouting>(&json).expect("reads"),
            routing
        );
        let old: SoulseekRouting = serde_json::from_str(r#"{"Kind":0,"YouTubeId":"abc"}"#).expect("reads");
        assert_eq!(old.stream_facts(), StreamFacts::default());
    }

    #[test]
    fn pinning_another_video_forgets_the_old_streams_facts() {
        let mut routing = SoulseekRouting::default();
        assert!(routing.pin_video("a"));
        routing.merge_stream_facts(&StreamFacts {
            size: Some(10),
            itag: Some("140".into()),
            ..Default::default()
        });
        assert!(!routing.pin_video("a"));
        assert_eq!(routing.stream_size, Some(10));
        assert!(routing.pin_video("b"));
        assert_eq!(routing.you_tube_id.as_deref(), Some("b"));
        assert_eq!(routing.stream_facts(), StreamFacts::default());
    }

    #[test]
    fn a_confirmed_stream_size_is_saved_and_a_new_video_forgets_it() {
        let mut routing = SoulseekRouting {
            kind: RoutingKind::Song,
            you_tube_id: Some("vid-a".into()),
            ..Default::default()
        };
        let unconfirmed = serde_json::to_string(&routing).unwrap();
        assert!(!unconfirmed.contains("StreamSizeConfirmed"));
        routing.merge_stream_facts(&StreamFacts {
            size: Some(1000),
            size_confirmed: true,
            ..Default::default()
        });

        let json = serde_json::to_string(&routing).unwrap();
        assert!(
            json.contains(r#""StreamSize":1000,"StreamSizeConfirmed":true"#),
            "{json}"
        );
        let read: SoulseekRouting = serde_json::from_str(&json).unwrap();
        assert!(read.stream_facts().size_confirmed);

        routing.pin_video("vid-b");
        assert_eq!(routing.stream_facts(), StreamFacts::default());
    }
}
