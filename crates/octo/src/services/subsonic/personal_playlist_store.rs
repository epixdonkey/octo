//! `personal-playlists.json`: the owner's mixed personal playlists. A playlist keeps its real
//! Navidrome id and a list of positions, each holding BOTH sources of one song at once: the
//! library's id (`CanonicalId`) and the outside one (the registry's short id, a full `Song`
//! snapshot and the routing that minted the id, kept byte-for-byte as the position's `seed`).
//!
//! Adding a position never downloads anything (the heart's own paths do that); the seed is what
//! makes the position survive a restart or an LRU eviction, because
//! [`crate::services::soulseek::external_id_registry::ExternalIdRegistry::lookup`] only takes a
//! recovered routing back when it hashes to the very id it is asked for, and only the routing as
//! it was minted does (see [`seed_matching`]).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use octo_core::models::domain::Song;
use octo_core::soulseek::{RoutingKind, SoulseekRouting};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::services::family::store_file;
use crate::services::soulseek::external_id_registry::ExternalIdRegistry;

/// The version word a clean edit's routing carries (see `make_short_id`).
const CLEAN_WORD: &str = "clean";

/// One position of a personal playlist: one song, both of its sources.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct PersonalPlaylistEntry {
    /// Uniquely names this position, so the same song twice keeps two rows and drag-and-drop
    /// moves one of them, not both.
    #[serde(deserialize_with = "crate::services::state_file::null_as_default")]
    pub occurrence_id: String,
    /// The library's own song id, once the file is there. Never removes the outside half.
    pub library_id: Option<String>,
    /// The registry's short id (22 base62 characters), when the song came from outside.
    pub outside_id: Option<String>,
    /// The pinned video, kept beside the snapshot because `Song` has no field of its own.
    pub you_tube_id: Option<String>,
    /// The whole song as it was found: artist, title, album, duration, ISRC, MusicBrainz id.
    pub song_snapshot: Option<Song>,
    /// The routing that minted `outside_id`, byte-for-byte, so the registry's `lookup` guard
    /// takes the entry back after a restart or an eviction (D6). `None` when no candidate
    /// hashed to the id: the position still plays, but cannot be recovered.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seed: Option<SoulseekRouting>,
}

/// One personal playlist, on its real Navidrome id.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct PersonalPlaylist {
    /// Navidrome's own playlist id, unmodified: prefixing it would break clients (Fakt A1).
    #[serde(deserialize_with = "crate::services::state_file::null_as_default")]
    pub id: String,
    #[serde(deserialize_with = "crate::services::state_file::null_as_default")]
    pub owner: String,
    /// The base name, without the folder prefix.
    #[serde(deserialize_with = "crate::services::state_file::null_as_default")]
    pub name: String,
    /// The base comment, without the `#folder:` tag.
    #[serde(deserialize_with = "crate::services::state_file::null_as_default")]
    pub comment: String,
    pub public: bool,
    /// The folder the playlist is filed in, synchronized into the display name's prefix and the
    /// comment's `#folder:` tag (D9).
    pub folder: Option<String>,
    #[serde(deserialize_with = "crate::services::state_file::null_as_default")]
    pub entries: Vec<PersonalPlaylistEntry>,
    #[serde(with = "octo_core::json::datetime::utc")]
    pub changed: DateTime<Utc>,
}

impl PersonalPlaylist {
    /// The `#folder:Ścieżka` tag of the comment, when a folder is set.
    fn folder_tag(folder: &str) -> String {
        format!("#folder:{folder}")
    }

    /// The display name: `Folder / Podfolder / Nazwa` (D9's second carrier).
    pub fn display_name(&self) -> String {
        match self.display_folder() {
            Some(folder) => format!("{folder} / {}", self.name),
            None => self.name.clone(),
        }
    }

    /// The display comment, with the `#folder:` tag first (D9's third carrier). The tag keeps
    /// the folder as it is stored (`#folder:Techno/Peak Time`), while the name's prefix spells
    /// it out with ` / `.
    pub fn display_comment(&self) -> String {
        let Some(folder) = self.folder.as_deref().map(str::trim).filter(|f| !f.is_empty()) else {
            return self.comment.clone();
        };
        let tag = Self::folder_tag(folder);
        if self.comment.contains(&tag) {
            self.comment.clone()
        } else if self.comment.is_empty() {
            tag
        } else {
            format!("{} {tag}", self.comment)
        }
    }

    /// The folder as it is shown in the name's prefix: the kept segments, joined with ` / `.
    fn display_folder(&self) -> Option<String> {
        let folder = self.folder.as_deref()?.trim();
        if folder.is_empty() {
            return None;
        }
        let joined = folder
            .split('/')
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(" / ");
        (!joined.is_empty()).then_some(joined)
    }

    /// Sets the folder on all three carriers at once: the field, the display name's prefix and
    /// the comment's tag. The base name keeps its own words; `adopt` is the reader that takes a
    /// foldered name apart again.
    pub fn set_folder(&mut self, folder: Option<&str>) {
        let cleaned = folder
            .map(str::trim)
            .filter(|folder| !folder.is_empty())
            .map(str::to_string);
        self.folder = cleaned;
        self.changed = Utc::now();
    }

    /// The positions that still name a song outside the library, by their outside id: what an
    /// explicit "download the missing ones" takes (R-5 operation 4).
    pub fn missing_tracks_for_explicit_download(&self) -> Vec<String> {
        self.entries
            .iter()
            .filter(|entry| entry.library_id.is_none())
            .filter_map(|entry| entry.outside_id.clone())
            .collect()
    }

    /// Each position's playback source at once (`PREFER_LOCAL`, R-4): the library's file when it
    /// is still one of `available_local_ids`, else the outside stream the position kept. A
    /// position with neither is left out.
    pub fn resolve_playback_sources(&self, available_local_ids: &HashSet<String>) -> Vec<ServeSource> {
        self.entries
            .iter()
            .filter_map(|entry| {
                if let Some(library) = entry
                    .library_id
                    .as_deref()
                    .filter(|id| available_local_ids.contains(*id))
                {
                    return Some(ServeSource::Library {
                        id: library.to_string(),
                        outside: entry.outside_id.clone(),
                    });
                }
                entry.outside_id.clone().map(|outside_id| ServeSource::Outside {
                    outside_id,
                    you_tube_id: entry.you_tube_id.clone(),
                })
            })
            .collect()
    }
}

/// How one position is served: the library's file when the person still has it, else the
/// outside stream. A position keeps both sources, so a file that comes back (a download lands,
/// a deleted file returns) swaps the source without the position disappearing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServeSource {
    /// The library's song, by its Navidrome id, with the outside id as the fallback.
    Library { id: String, outside: Option<String> },
    /// The outside version, by the registry's short id and the video it pinned.
    Outside {
        outside_id: String,
        you_tube_id: Option<String>,
    },
}

/// The routing that minted `outside_id`, found by playing the candidates back against the
/// registry's public, pure `make_short_id` (D6): the video the position knows (or none), the
/// snapshot's duration (or none) and the clean-edit mark, every combination at most eight times.
/// `None` when nothing hashes to the id: the position is kept without a seed, honestly logged.
pub fn seed_matching(
    outside_id: &str,
    you_tube_id: Option<&str>,
    artist: &str,
    title: &str,
    duration: Option<i32>,
) -> Option<SoulseekRouting> {
    for video in [you_tube_id, None] {
        for length in [duration, None] {
            for version in [None, Some(CLEAN_WORD)] {
                let candidate = SoulseekRouting {
                    kind: RoutingKind::Song,
                    you_tube_id: video.map(str::to_string),
                    artist: (!artist.is_empty()).then(|| artist.to_string()),
                    title: (!title.is_empty()).then(|| title.to_string()),
                    duration: length,
                    version: version.map(str::to_string),
                    ..Default::default()
                };
                if ExternalIdRegistry::make_short_id(&candidate) == outside_id {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

/// The store behind `personal-playlists.json`.
pub struct PersonalPlaylistStore {
    playlists: Mutex<Vec<PersonalPlaylist>>,
    path: Option<PathBuf>,
    save_lock: Mutex<()>,
    load_warning: Option<String>,
}

impl PersonalPlaylistStore {
    pub fn in_memory() -> Self {
        Self::new(None::<PathBuf>)
    }

    pub fn new(path: Option<impl AsRef<Path>>) -> Self {
        let path = store_file::clean_path(path);
        let (playlists, load_warning) = store_file::load::<Vec<PersonalPlaylist>>(path.as_deref());
        PersonalPlaylistStore {
            playlists: Mutex::new(playlists),
            path,
            save_lock: Mutex::new(()),
            load_warning,
        }
    }

    /// The warning a file that could not be read left, for the log at startup.
    pub fn load_warning(&self) -> Option<&str> {
        self.load_warning.as_deref()
    }

    fn save(&self) {
        let path = self.path.as_deref();
        store_file::save_with(path, &self.save_lock, || self.playlists.lock().clone());
    }

    /// Whether the playlist with Navidrome's own `id` is one of ours.
    pub fn knows(&self, id: &str) -> bool {
        !id.is_empty() && self.playlists.lock().iter().any(|list| list.id == id)
    }

    /// A copy of one of our playlists.
    pub fn find(&self, id: &str) -> Option<PersonalPlaylist> {
        self.playlists.lock().iter().find(|list| list.id == id).cloned()
    }

    /// A copy of every playlist we hold.
    pub fn snapshot(&self) -> Vec<PersonalPlaylist> {
        self.playlists.lock().clone()
    }

    /// Writes `playlist` whole, adding or replacing by its id. The overlay owns the whole
    /// document, so a mutation is one read, one change and this call.
    pub fn upsert(&self, playlist: PersonalPlaylist) {
        let mut playlists = self.playlists.lock();
        match playlists.iter_mut().find(|list| list.id == playlist.id) {
            Some(list) => *list = playlist,
            None => playlists.push(playlist),
        }
        drop(playlists);
        self.save();
    }

    /// Applies `edit` to the playlist with Navidrome's own `id` as ONE read-modify-write, under
    /// the store's own lock (O3): a caller-side `find` → change → `upsert` can interleave with a
    /// parallel request's and lose one of the changes, while this cannot. The edited playlist as
    /// the store now holds it, with `edit`'s own return value beside it; `None` (and no write)
    /// when the store does not hold `id`.
    pub fn edit<R>(
        &self,
        id: &str,
        edit: impl FnOnce(&mut PersonalPlaylist) -> R,
    ) -> Option<(PersonalPlaylist, R)> {
        let mut playlists = self.playlists.lock();
        let list = playlists.iter_mut().find(|list| list.id == id)?;
        let result = edit(list);
        let edited = list.clone();
        drop(playlists);
        self.save();
        Some((edited, result))
    }

    /// Adds `playlist` only while its id is not held yet, so two parallel adoptions of one plain
    /// playlist converge on whichever landed first instead of overwriting each other (O3). True
    /// when this call's playlist was taken; false when the id was already held.
    pub fn insert_if_absent(&self, playlist: PersonalPlaylist) -> bool {
        let mut playlists = self.playlists.lock();
        if playlists.iter().any(|list| list.id == playlist.id) {
            return false;
        }
        playlists.push(playlist);
        drop(playlists);
        self.save();
        true
    }

    /// Forgets the overlay of `id` (the playlist in Navidrome is `deletePlaylist`'s to take).
    pub fn remove(&self, id: &str) {
        let mut playlists = self.playlists.lock();
        playlists.retain(|list| list.id != id);
        drop(playlists);
        self.save();
    }

    /// The seed a position kept, verbatim: what `ExternalIdRegistry::lookup`'s guard takes back
    /// after a restart or an LRU eviction (D6). `None` for a position without one.
    pub fn recover_routing(&self, outside_id: &str) -> Option<SoulseekRouting> {
        self.playlists
            .lock()
            .iter()
            .find_map(|list| {
                list.entries
                    .iter()
                    .find(|entry| entry.outside_id.as_deref() == Some(outside_id))
            })
            .and_then(|entry| entry.seed.clone())
    }

    /// The full `Song` a position kept, for the overlay `get_song` puts on its thin answer.
    pub fn recover_song(&self, outside_id: &str) -> Option<Song> {
        self.playlists
            .lock()
            .iter()
            .find_map(|list| {
                list.entries
                    .iter()
                    .find(|entry| entry.outside_id.as_deref() == Some(outside_id))
            })
            .and_then(|entry| entry.song_snapshot.clone())
    }

    /// Binds `occurrence_id` of `playlist` to the library's song `library_id`, keeping the
    /// outside half and the snapshot: a file that goes away again falls back to the stream
    /// (R-4), and one call answers both a file landing and a file leaving (`library_id = None`).
    pub fn set_library_id(&self, playlist_id: &str, occurrence_id: &str, library_id: Option<String>) {
        let mut playlists = self.playlists.lock();
        let Some(list) = playlists.iter_mut().find(|list| list.id == playlist_id) else {
            return;
        };
        let Some(entry) = list
            .entries
            .iter_mut()
            .find(|entry| entry.occurrence_id == occurrence_id)
        else {
            return;
        };
        if entry.library_id == library_id {
            return;
        }
        entry.library_id = library_id;
        list.changed = Utc::now();
        drop(playlists);
        self.save();
    }
}

#[cfg(test)]
#[path = "personal_playlist_store_tests.rs"]
mod tests;
