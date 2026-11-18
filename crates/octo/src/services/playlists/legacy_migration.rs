//! One-time, idempotent move of the fork's old `personal-playlists.json` onto the held store's
//! `octo-playlists/` files, run at startup before any request is served, on the store the held
//! playlists are about to be built from (so the moved playlists are held from the first answer
//! on, and the revision every file carries comes from the store's own counter).
//!
//! What moves and what does not:
//!
//! - A playlist is moved only when at least one of its entries names a song outside the library
//!   (`outside_id`): those are the playlists the held mechanism exists for. A purely local
//!   playlist is an ordinary Navidrome playlist the held store would "forget" on its first
//!   operation anyway (`holds()` is false without a streamed entry), so it is skipped and left
//!   to Navidrome alone.
//! - A playlist whose id already lives in the store (a migration cut short by a crash, a
//!   partially run file) is never overwritten: the collision is logged as a warning and
//!   skipped, which is what makes a second run a no-op.
//! - An entry without a seed moves as an entry with its snapshot only (`seed: None`): it still
//!   serves and streams from the snapshot, but after the registry evicts its routing it cannot
//!   be recovered — the one regression this migration knowingly carries.
//! - Fields with no place in the held format are dropped where the song keeps them another way
//!   or nothing reads them anymore: `occurrence_id` (the entry's file position is its name),
//!   `you_tube_id` (kept inside the seed's routing), the `source` badge (on dev the row's badge
//!   is read from `seed.suggested_by`), and the `folder` field with the name prefix and the
//!   comment's `#folder:` tag (the dead station D9; the app reads folders from
//!   `octoPlaylistFolders`).
//!
//! The source file is never deleted. Once every playlist with an outside entry has been moved
//! or was a collision, it is renamed to `personal-playlists.json.migrated-<YYYY-MM-DD>`, which
//! is the whole rollback: rename it back and delete the generated `octo-playlists/<id>.json`
//! files.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use octo_core::models::domain::Song;
use octo_core::soulseek::SoulseekRouting;
use tracing::{info, warn};

use super::entries::snapshot_of_song;
use super::held_store::{HeldEntry, HeldPlaylist, HeldStore, PlaylistState};
use crate::services::common::acquisition_tracker::key_of;
use crate::services::state_file;

/// The name of the file the old store kept, beside `octo-playlists/`.
const OLD_FILE: &str = "personal-playlists.json";

/// What one migration run did, for the startup log and the tests.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MigrationCounts {
    /// Playlists written into the held store.
    pub moved: usize,
    /// Purely local playlists, skipped: ordinary Navidrome playlists, not held material.
    pub local: usize,
    /// Playlists whose id already lived in the store, skipped without being touched.
    pub collisions: usize,
    /// Playlists that could not be written (or have no id to write under).
    pub failed: usize,
    /// Entries of the moved playlists that keep a seed.
    pub with_seed: usize,
    /// Entries of the moved playlists that keep only a snapshot.
    pub without_seed: usize,
    /// Whether the source file was renamed to its `.migrated-<date>` name.
    pub renamed: bool,
}

/// One position of the old `personal-playlists.json`, read in its own shape only: nothing on
/// dev writes it anymore.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(rename_all = "PascalCase", default)]
struct LegacyEntry {
    #[serde(deserialize_with = "state_file::null_as_default")]
    occurrence_id: String,
    library_id: Option<String>,
    outside_id: Option<String>,
    you_tube_id: Option<String>,
    source: Option<String>,
    song_snapshot: Option<Song>,
    seed: Option<SoulseekRouting>,
}

/// One playlist of the old `personal-playlists.json`. The name, comment, public flag and folder
/// live in Navidrome under the playlist's real id already; the migration moves only what the
/// held format keeps.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(rename_all = "PascalCase", default)]
struct LegacyPlaylist {
    #[serde(deserialize_with = "state_file::null_as_default")]
    id: String,
    #[serde(deserialize_with = "state_file::null_as_default")]
    owner: String,
    #[serde(deserialize_with = "state_file::null_as_default")]
    entries: Vec<LegacyEntry>,
    #[serde(with = "octo_core::json::datetime::utc")]
    changed: DateTime<Utc>,
}

/// Moves the old `personal-playlists.json` beside `octo-playlists/` into the store, then
/// renames the source to its `.migrated-<date>` name once nothing that belongs in the store is
/// left in it. `config_dir` None (endpoint tests) or a missing file does nothing, quietly.
pub fn migrate_personal_playlists(store: &HeldStore, config_dir: Option<&Path>) -> MigrationCounts {
    let Some(dir) = config_dir else {
        return MigrationCounts::default();
    };
    let source: PathBuf = dir.join(OLD_FILE);
    let Some(text) = (match state_file::read_text(&source) {
        Ok(text) => text,
        Err(e) => {
            warn!("{OLD_FILE} could not be read, leaving it as it is: {e}");
            return MigrationCounts::default();
        }
    }) else {
        return MigrationCounts::default();
    };
    let playlists = match serde_json::from_str::<Vec<LegacyPlaylist>>(&text) {
        Ok(playlists) => playlists,
        Err(e) => {
            warn!("{OLD_FILE} could not be understood, leaving it as it is: {e}");
            return MigrationCounts::default();
        }
    };
    let mut counts = MigrationCounts::default();
    for playlist in &playlists {
        if playlist.entries.iter().all(|entry| entry.outside_id.is_none()) {
            counts.local += 1;
            continue;
        }
        if playlist.id.trim().is_empty() {
            warn!("a playlist of {OLD_FILE} has no id, so it cannot be moved; the file is kept");
            counts.failed += 1;
            continue;
        }
        if store.is_held(&playlist.id) {
            warn!(
                "playlist {} of {OLD_FILE} is already held, so the migration leaves it as it is",
                playlist.id
            );
            counts.collisions += 1;
            continue;
        }
        let next = PlaylistState {
            held: Some(held_of(playlist)),
            replaced: None,
        };
        let (before, stored) = store.swap(&playlist.id, next);
        if let Err(e) = store.save(&playlist.id, &before, &stored) {
            store.restore(&playlist.id, before);
            warn!(
                "playlist {} could not be moved into octo-playlists: {e}",
                playlist.id
            );
            counts.failed += 1;
        } else {
            let entries = &stored.held.expect("held, just swapped in").entries;
            counts.with_seed += entries
                .iter()
                .filter(|entry| entry.outside_id.is_some() && entry.seed.is_some())
                .count();
            counts.without_seed += entries
                .iter()
                .filter(|entry| entry.outside_id.is_some() && entry.seed.is_none())
                .count();
            counts.moved += 1;
        }
    }
    if counts.failed == 0 && (counts.moved + counts.collisions + counts.local) > 0 {
        let migrated = migrated_name(&source);
        match std::fs::rename(&source, &migrated) {
            Ok(()) => counts.renamed = true,
            Err(e) => warn!("{} could not be renamed away: {e}", migrated.display()),
        }
    }
    info!(
        "{}: {} playlists moved into octo-playlists ({} entries with a seed, {} without), {} \
         purely local and {} already held skipped, {} failed, source {}",
        OLD_FILE,
        counts.moved,
        counts.with_seed,
        counts.without_seed,
        counts.local,
        counts.collisions,
        counts.failed,
        if counts.renamed {
            format!("renamed to {}", migrated_name(&source).display())
        } else {
            "kept in place".to_string()
        }
    );
    counts
}

/// The old playlist as a held one: same id (its Navidrome identity), same owner, the entries
/// moved one by one, and the mirror seeded with the library ids the entries name — the list the
/// old store kept Navidrome in step with, which the first held operation re-checks anyway.
fn held_of(playlist: &LegacyPlaylist) -> HeldPlaylist {
    let entries: Vec<HeldEntry> = playlist
        .entries
        .iter()
        .map(|entry| entry_of(entry, playlist.changed))
        .collect();
    let mirror = entries
        .iter()
        .filter_map(|entry| entry.library_id.clone())
        .collect();
    HeldPlaylist {
        id: playlist.id.clone(),
        owner: playlist.owner.clone(),
        entries,
        mirror,
        pending: None,
        changed: playlist.changed,
        gone_seen: false,
        revision: 0,
    }
}

/// One old position as a held entry. A registry short id is routed exactly as the entry builder
/// routes one (`soulseek`, the id itself as `external_id`, and `seed_id` beside the seed, the
/// pair the caps count and the seed index read); an `ext-<provider>-<...>` id, should a file
/// carry one, keeps its provider and the id the prefix names. `added` is the playlist's own
/// `changed`: the old format kept no per-entry time.
fn entry_of(entry: &LegacyEntry, added: DateTime<Utc>) -> HeldEntry {
    let outside = entry.outside_id.clone().filter(|id| !id.trim().is_empty());
    let seed = entry.seed.clone();
    let (provider, external_id, key) = match &outside {
        Some(outside) if outside.starts_with("ext-") => match parse_ext_id(outside) {
            Some((provider, external_id)) => {
                let key = key_of(&provider, &external_id);
                (Some(provider), Some(external_id), Some(key))
            }
            None => (None, None, None),
        },
        Some(outside) => {
            let key = key_of("soulseek", outside);
            (Some("soulseek".to_string()), Some(outside.clone()), Some(key))
        }
        None => (None, None, None),
    };
    HeldEntry {
        library_id: entry.library_id.clone(),
        outside_id: outside.clone(),
        // The seed resolves under the registry id that minted it, which is the entry's own
        // outside id; an entry that kept no seed keeps no seed id either.
        seed_id: match (&seed, &outside) {
            (Some(_), Some(outside)) => Some(outside.clone()),
            _ => None,
        },
        seed: seed.map(Arc::new),
        provider,
        external_id,
        key,
        song: entry.song_snapshot.as_ref().map(snapshot_of_song),
        added,
    }
}

/// An `ext-<provider>-song-<id>` (or the older `ext-<provider>-<id>`) split into its provider
/// and the id the prefix names, as the library's own id parser reads it. None when the prefix
/// names nothing.
fn parse_ext_id(id: &str) -> Option<(String, String)> {
    let parts: Vec<&str> = id.split('-').collect();
    const KNOWN_TYPES: [&str; 3] = ["song", "album", "artist"];
    if parts.len() >= 4 && KNOWN_TYPES.contains(&parts[2]) {
        return Some((parts[1].to_string(), parts[3..].join("-")));
    }
    if parts.len() >= 3 {
        return Some((parts[1].to_string(), parts[2..].join("-")));
    }
    None
}

/// `personal-playlists.json.migrated-<YYYY-MM-DD>`, named after today.
fn migrated_name(source: &Path) -> PathBuf {
    let date = Utc::now().format("%Y-%m-%d");
    let mut name = source.as_os_str().to_owned();
    name.push(format!(".migrated-{date}"));
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::super::held_store::FileOp;
    use super::*;

    /// The old file exactly as the old store wrote it: PascalCase, a playlist of mixed entries
    /// (a seeded outside one, a seedless outside one, a purely local one, an `ext-` one with a
    /// library copy), a purely local playlist, and a playlist whose id is already held.
    const OLD_FILE_TEXT: &str = r#"[
        {
            "Id": "pl-mixed",
            "Owner": "alice",
            "Name": "Mixed",
            "Comment": "the base comment",
            "Public": true,
            "Folder": "Techno/Peak Time",
            "Entries": [
                {
                    "OccurrenceId": "occ-1",
                    "LibraryId": "lib-1",
                    "OutsideId": "Seed0123456789012345ab",
                    "YouTubeId": "vid-1",
                    "Source": "YouTube Music (video)",
                    "SongSnapshot": {
                        "Id": "Seed0123456789012345ab",
                        "Title": "Night Drive",
                        "Artist": "Neon Fox",
                        "Album": "Neon City",
                        "Duration": 200,
                        "Isrc": "DEXX62100001",
                        "MusicBrainzId": "mb-1"
                    },
                    "Seed": {
                        "Kind": 0,
                        "Artist": "Neon Fox",
                        "Title": "Night Drive",
                        "YouTubeId": "vid-1",
                        "Version": "clean",
                        "SuggestedBy": "YouTube Music (video)"
                    }
                },
                {
                    "OccurrenceId": "occ-2",
                    "OutsideId": "Seed999999999999999999zz",
                    "SongSnapshot": {
                        "Title": "No Seed Song",
                        "Artist": "The Orphans",
                        "Duration": 180
                    }
                },
                {
                    "OccurrenceId": "occ-3",
                    "LibraryId": "lib-2"
                },
                {
                    "OccurrenceId": "occ-4",
                    "LibraryId": "lib-3",
                    "OutsideId": "ext-deezer-song-77",
                    "SongSnapshot": {
                        "Title": "Catalog Song",
                        "Artist": "Deezer Act",
                        "Duration": 210
                    }
                }
            ],
            "Changed": "2026-11-01T10:00:00Z"
        },
        {
            "Id": "pl-local",
            "Owner": "alice",
            "Name": "Only local",
            "Comment": "",
            "Public": false,
            "Entries": [
                {
                    "OccurrenceId": "occ-9",
                    "LibraryId": "lib-9"
                }
            ],
            "Changed": "2026-11-02T10:00:00Z"
        },
        {
            "Id": "pl-taken",
            "Owner": "alice",
            "Name": "Already held",
            "Comment": "",
            "Public": false,
            "Entries": [
                {
                    "OccurrenceId": "occ-8",
                    "OutsideId": "Seed888888888888888888yy"
                }
            ],
            "Changed": "2026-11-03T10:00:00Z"
        }
    ]"#;

    fn write_old_file(dir: &Path) {
        std::fs::write(dir.join(OLD_FILE), OLD_FILE_TEXT).expect("the old file written");
    }

    fn migrated_names(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .expect("the dir read")
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(&format!("{OLD_FILE}.migrated")))
            .collect()
    }

    #[test]
    fn a_missing_file_is_left_alone_quietly() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let store = HeldStore::new(Some(dir.path().join("octo-playlists")));
        let counts = migrate_personal_playlists(&store, Some(dir.path()));
        assert_eq!(counts, MigrationCounts::default());
        assert!(!store.is_held("pl-mixed"));
        assert!(migrated_names(dir.path()).is_empty());
    }

    #[test]
    fn no_config_dir_is_left_alone_quietly() {
        let store = HeldStore::in_memory();
        let counts = migrate_personal_playlists(&store, None);
        assert_eq!(counts, MigrationCounts::default());
    }

    #[test]
    fn an_unreadable_file_is_kept_and_skipped() {
        let dir = tempfile::tempdir().expect("a temp dir");
        std::fs::write(dir.path().join(OLD_FILE), "[{\"Id\": 3}]").expect("the old file written");
        let store = HeldStore::new(Some(dir.path().join("octo-playlists")));
        let counts = migrate_personal_playlists(&store, Some(dir.path()));
        assert_eq!(counts, MigrationCounts::default());
        assert!(dir.path().join(OLD_FILE).exists(), "the file is never touched");
        assert!(migrated_names(dir.path()).is_empty());
    }

    #[test]
    fn the_whole_file_moves_onto_the_held_store() {
        let dir = tempfile::tempdir().expect("a temp dir");
        write_old_file(dir.path());
        let playlists = dir.path().join("octo-playlists");
        // The playlist `pl-taken` already lives in the store, as a crashed first run or a
        // hand-made file would leave it.
        let held_before = {
            let store = HeldStore::new(Some(&playlists));
            let (before, stored) = store.swap(
                "pl-taken",
                PlaylistState {
                    held: Some(HeldPlaylist {
                        id: "pl-taken".into(),
                        owner: "alice".into(),
                        entries: Vec::new(),
                        mirror: Vec::new(),
                        pending: None,
                        changed: Utc
                            .with_ymd_and_hms(2026, 11, 3, 9, 0, 0)
                            .single()
                            .expect("a time"),
                        gone_seen: false,
                        revision: 0,
                    }),
                    replaced: None,
                },
            );
            store.save("pl-taken", &before, &stored).expect("saved");
            stored.held.expect("held")
        };
        let store = HeldStore::new(Some(&playlists));
        let counts = migrate_personal_playlists(&store, Some(dir.path()));

        assert_eq!(counts.moved, 1, "only the mixed playlist moves");
        assert_eq!(counts.local, 1, "the purely local playlist is skipped");
        assert_eq!(counts.collisions, 1, "the already held id is not overwritten");
        assert_eq!(counts.with_seed, 1);
        assert_eq!(counts.without_seed, 2, "the seedless and the ext- entries");
        assert_eq!(counts.failed, 0);
        assert!(counts.renamed);

        // The moved playlist, exactly as it lands on disk and reloads.
        let moved = HeldStore::new(Some(&playlists));
        let mixed = moved.get("pl-mixed").expect("the mixed playlist held");
        assert_eq!(mixed.owner, "alice");
        assert_eq!(
            mixed.mirror,
            vec!["lib-1".to_string(), "lib-2".to_string(), "lib-3".to_string()]
        );
        assert_eq!(mixed.entries.len(), 4);
        assert_eq!(
            mixed.changed,
            Utc.with_ymd_and_hms(2026, 11, 1, 10, 0, 0)
                .single()
                .expect("a time")
        );
        assert!(mixed.revision > 0, "the store's counter stamped it");
        assert!(mixed.holds());

        let seeded = &mixed.entries[0];
        assert_eq!(seeded.library_id.as_deref(), Some("lib-1"));
        assert_eq!(seeded.outside_id.as_deref(), Some("Seed0123456789012345ab"));
        assert_eq!(seeded.seed_id.as_deref(), Some("Seed0123456789012345ab"));
        assert_eq!(seeded.provider.as_deref(), Some("soulseek"));
        assert_eq!(seeded.external_id.as_deref(), Some("Seed0123456789012345ab"));
        assert!(
            seeded
                .key
                .as_deref()
                .is_some_and(|key| key == "soulseek:Seed0123456789012345ab")
        );
        let seed = seeded.seed.as_ref().expect("the seed kept");
        assert_eq!(seed.you_tube_id.as_deref(), Some("vid-1"));
        assert_eq!(seed.version.as_deref(), Some("clean"));
        let snapshot = seeded.song.as_ref().expect("the snapshot kept");
        assert_eq!(snapshot.title, "Night Drive");
        assert_eq!(snapshot.artist, "Neon Fox");
        assert_eq!(snapshot.seconds, Some(200));
        assert_eq!(snapshot.isrcs, vec!["DEXX62100001".to_string()]);

        let seedless = &mixed.entries[1];
        assert_eq!(seedless.outside_id.as_deref(), Some("Seed999999999999999999zz"));
        assert!(seedless.seed.is_none());
        assert!(seedless.seed_id.is_none());
        assert_eq!(seedless.provider.as_deref(), Some("soulseek"));
        assert_eq!(
            seedless.song.as_ref().expect("the snapshot kept").title,
            "No Seed Song"
        );

        let library_only = &mixed.entries[2];
        assert_eq!(library_only.library_id.as_deref(), Some("lib-2"));
        assert!(library_only.outside_id.is_none());
        assert!(library_only.provider.is_none());
        assert!(library_only.song.is_none());

        let ext = &mixed.entries[3];
        assert_eq!(ext.outside_id.as_deref(), Some("ext-deezer-song-77"));
        assert_eq!(ext.provider.as_deref(), Some("deezer"));
        assert_eq!(ext.external_id.as_deref(), Some("77"));
        assert!(ext.key.as_deref().is_some_and(|key| key == "deezer:77"));
        assert!(ext.seed_id.is_none());

        // The purely local playlist and the collision: nothing was written for them, and the
        // collision's own state was not overwritten.
        assert!(!moved.is_held("pl-local"));
        assert!(!playlists.join("pl-local.json").exists());
        assert_eq!(moved.get("pl-taken").expect("held"), held_before);

        // The source was renamed, never deleted, and a second run is a no-op.
        assert!(!dir.path().join(OLD_FILE).exists());
        let names = migrated_names(dir.path());
        assert_eq!(names.len(), 1, "one renamed source: {names:?}");
        assert!(names[0].starts_with(&format!("{OLD_FILE}.migrated-")));
        let again = migrate_personal_playlists(&store, Some(dir.path()));
        assert_eq!(again, MigrationCounts::default());
        assert_eq!(migrated_names(dir.path()).len(), 1);
        assert_eq!(moved.get("pl-mixed").expect("held"), mixed, "untouched");
    }

    #[test]
    fn a_run_cut_short_by_a_failing_write_is_finished_by_the_next_one() {
        let dir = tempfile::tempdir().expect("a temp dir");
        write_old_file(dir.path());
        let playlists = dir.path().join("octo-playlists");
        let store = HeldStore::new(Some(&playlists));
        store.fail_writes_when(Some(Arc::new(|path: &Path, op: FileOp| {
            // The mixed playlist's main file refuses its write; everything else is allowed.
            op == FileOp::Write && path.file_name().is_some_and(|name| name == "pl-mixed.json")
        })));
        let counts = migrate_personal_playlists(&store, Some(dir.path()));
        // The mixed playlist refuses its write; the taken one (no hook on its file) moves, so
        // the run is cut short halfway, exactly as a crash would leave it.
        assert_eq!(counts.moved, 1);
        assert_eq!(counts.failed, 1);
        assert_eq!(counts.local, 1);
        assert!(!counts.renamed, "a failed playlist keeps the source in place");
        assert!(dir.path().join(OLD_FILE).exists());
        assert!(
            !playlists.join("pl-mixed.json").exists(),
            "the failed write left nothing behind"
        );
        assert!(playlists.join("pl-taken.json").exists());

        // The next run moves what is left: the failed playlist now, the already held one a
        // collision, and then the source is renamed away.
        store.fail_writes_when(None);
        let counts = migrate_personal_playlists(&store, Some(dir.path()));
        assert_eq!(counts.moved, 1);
        assert_eq!(counts.collisions, 1);
        assert_eq!(counts.local, 1);
        assert_eq!(counts.failed, 0);
        assert!(counts.renamed);
        assert!(playlists.join("pl-mixed.json").exists());
        assert!(!dir.path().join(OLD_FILE).exists());
    }
}
