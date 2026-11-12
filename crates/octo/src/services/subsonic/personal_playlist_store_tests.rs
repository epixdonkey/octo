//! The personal playlist store's tests: the document round-trips with its three folder carriers,
//! the explicit download takes only what is still outside, and a position's seed carries the id
//! through a fresh registry's FULL `lookup` — the guard included (W1, R4-T3).

use std::collections::HashSet;
use std::sync::Arc;

use octo_core::models::domain::Song;
use octo_core::soulseek::{RoutingKind, SoulseekRouting};

use super::*;

/// A minted song row, as the search registers one: a video, a length, both from the start.
fn minted_row(registry: &ExternalIdRegistry, video: &str, artist: &str, title: &str, seconds: i32) -> String {
    registry.register(SoulseekRouting {
        kind: RoutingKind::Song,
        you_tube_id: Some(video.to_string()),
        artist: Some(artist.to_string()),
        title: Some(title.to_string()),
        duration: Some(seconds),
        ..Default::default()
    })
}

fn snapshot(artist: &str, title: &str, seconds: i32) -> Song {
    Song {
        artist: artist.to_string(),
        title: title.to_string(),
        duration: Some(seconds),
        ..Default::default()
    }
}

/// A playlist of one outside position with its seed found, on a store over `path`.
fn stored_outside_position(
    path: Option<std::path::PathBuf>,
    outside_id: &str,
    video: &str,
    artist: &str,
    title: &str,
    seconds: i32,
) -> PersonalPlaylistStore {
    let store = PersonalPlaylistStore::new(path);
    let seed = seed_matching(outside_id, Some(video), artist, title, Some(seconds));
    assert!(seed.is_some(), "the minted row's own fields hash to its id");
    store.upsert(PersonalPlaylist {
        id: "nd-playlist-1".to_string(),
        owner: "alice".to_string(),
        name: "Lost and found".to_string(),
        comment: String::new(),
        public: false,
        folder: None,
        changed: Utc::now(),
        entries: vec![PersonalPlaylistEntry {
            occurrence_id: "occ-1".to_string(),
            library_id: None,
            outside_id: Some(outside_id.to_string()),
            you_tube_id: Some(video.to_string()),
            source: None,
            song_snapshot: Some(snapshot(artist, title, seconds)),
            seed,
        }],
    });
    store
}

/// The document round-trips through `personal-playlists.json`, folder and all: the field keeps
/// the folder, the display name carries its prefix and the comment its tag (D9).
#[test]
fn the_document_round_trips_with_the_folder_on_all_three_carriers() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("personal-playlists.json");
    let store = PersonalPlaylistStore::new(Some(&path));
    let mut playlist = PersonalPlaylist {
        id: "nd-1".to_string(),
        owner: "alice".to_string(),
        name: "Peak Time".to_string(),
        comment: "from a radio evening".to_string(),
        public: true,
        folder: None,
        changed: Utc::now(),
        entries: vec![PersonalPlaylistEntry {
            occurrence_id: "occ-1".to_string(),
            library_id: Some("nd-song-9".to_string()),
            outside_id: None,
            you_tube_id: None,
            source: None,
            song_snapshot: None,
            seed: None,
        }],
    };
    playlist.set_folder(Some("Techno/Peak Time"));
    store.upsert(playlist);

    let read = PersonalPlaylistStore::new(Some(&path));
    let back = read.find("nd-1").expect("the playlist came back");
    assert_eq!(back.folder.as_deref(), Some("Techno/Peak Time"));
    assert_eq!(back.name, "Peak Time", "the base name keeps its own words");
    assert_eq!(back.display_name(), "Techno / Peak Time / Peak Time");
    assert_eq!(
        back.display_comment(),
        "from a radio evening #folder:Techno/Peak Time"
    );
    assert_eq!(back.entries[0].occurrence_id, "occ-1");
    assert_eq!(back.entries[0].library_id.as_deref(), Some("nd-song-9"));
}

/// The explicit download takes only the positions that are still outside: a position with a
/// library file is left alone, and a purely local one names nothing (R-5 operation 4).
#[test]
fn the_explicit_download_takes_only_what_is_still_outside() {
    let store = PersonalPlaylistStore::in_memory();
    store.upsert(PersonalPlaylist {
        id: "nd-1".to_string(),
        entries: vec![
            PersonalPlaylistEntry {
                occurrence_id: "occ-1".to_string(),
                library_id: Some("nd-song-1".to_string()),
                outside_id: Some("outside-kept".to_string()),
                ..Default::default()
            },
            PersonalPlaylistEntry {
                occurrence_id: "occ-2".to_string(),
                library_id: None,
                outside_id: Some("outside-missing".to_string()),
                ..Default::default()
            },
            PersonalPlaylistEntry {
                occurrence_id: "occ-3".to_string(),
                library_id: Some("nd-song-3".to_string()),
                outside_id: None,
                ..Default::default()
            },
        ],
        ..Default::default()
    });
    let playlist = store.find("nd-1").expect("found");
    assert_eq!(
        playlist.missing_tracks_for_explicit_download(),
        ["outside-missing".to_string()]
    );
}

/// W1, the key one: a position added from a minted routing keeps the routing that hashed to the
/// id, so after the registry forgets the entry (a restart with the LRU cold, an eviction), the
/// FULL `ExternalIdRegistry::lookup` — the `make_short_id` guard included — takes the entry back
/// and registers it again. Without the seed there is nothing to take back.
#[test]
fn a_positions_seed_carries_the_id_through_a_cold_registrys_full_lookup() {
    let registry = ExternalIdRegistry::in_memory();
    let outside_id = minted_row(&registry, "vBvDiLKTDWA", "K3KHA", "Lost in Sound", 619);
    let store = Arc::new(stored_outside_position(
        None,
        &outside_id,
        "vBvDiLKTDWA",
        "K3KHA",
        "Lost in Sound",
        619,
    ));

    // The registry as a restart leaves it: cold, with the playlist store behind its recovery.
    let cold = ExternalIdRegistry::in_memory();
    assert!(cold.lookup(&outside_id).is_none(), "nothing is held yet");
    let recovered = Arc::clone(&store);
    cold.set_recovery(Arc::new(move |id| recovered.recover_routing(id)));

    let routing = cold.lookup(&outside_id).expect("the seed carried the id");
    let routing = routing.snapshot();
    assert_eq!(routing.you_tube_id.as_deref(), Some("vBvDiLKTDWA"));
    assert_eq!(routing.duration, Some(619));
    assert_eq!(cold.count(), 1, "the lookup registered the entry again");
    // And the snapshot comes back with it, for getSong's overlay.
    let for_the_overlay = Arc::clone(&store);
    assert_eq!(
        for_the_overlay.recover_song(&outside_id).map(|song| song.title),
        Some("Lost in Sound".into())
    );
}

/// The negative case (W1): a position taken without a seed — no candidate hashed to the id —
/// cannot be recovered, and the full lookup honestly says None.
#[test]
fn a_position_without_a_seed_is_not_recovered_by_the_full_lookup() {
    let registry = ExternalIdRegistry::in_memory();
    let outside_id = minted_row(&registry, "rwqD5PrajEA", "K3KHA", "Lost in Sound", 619);
    // A foreign id no candidate hashes to: the position is kept, honestly, with no seed.
    assert!(seed_matching(&outside_id, Some("otherVideo"), "Someone", "Else", Some(12)).is_none());

    let store = PersonalPlaylistStore::in_memory();
    store.upsert(PersonalPlaylist {
        id: "nd-1".to_string(),
        entries: vec![PersonalPlaylistEntry {
            occurrence_id: "occ-1".to_string(),
            outside_id: Some("anIdNoCandidateHashesTo".to_string()),
            song_snapshot: Some(snapshot("K3KHA", "Lost in Sound", 619)),
            seed: None,
            ..Default::default()
        }],
        ..Default::default()
    });
    let cold = ExternalIdRegistry::in_memory();
    let warningless = Arc::new(PersonalPlaylistStore::in_memory());
    cold.set_recovery(Arc::new(move |id| warningless.recover_routing(id)));
    assert!(
        cold.lookup("anIdNoCandidateHashesTo").is_none(),
        "a position without a seed is not recovered"
    );
    assert_eq!(store.recover_routing("anIdNoCandidateHashesTo"), None);
}

/// The seed is found from the row's own halves: the video the position knows, the snapshot's
/// length, the clean-edit mark — and a catalog row minted with neither video nor length gets its
/// seed back too.
#[test]
fn the_seed_is_found_for_minted_rows_and_catalog_rows_alike() {
    let registry = ExternalIdRegistry::in_memory();
    let pinned = registry.register(SoulseekRouting {
        kind: RoutingKind::Song,
        you_tube_id: Some("vBvDiLKTDWA".into()),
        artist: Some("K3KHA".into()),
        title: Some("Lost in Sound".into()),
        duration: Some(619),
        ..Default::default()
    });
    let found = seed_matching(&pinned, Some("vBvDiLKTDWA"), "K3KHA", "Lost in Sound", Some(619))
        .expect("the pinned row's seed");
    assert_eq!(ExternalIdRegistry::make_short_id(&found), pinned);

    let bare = registry.register(SoulseekRouting {
        kind: RoutingKind::Song,
        artist: Some("K3KHA".into()),
        title: Some("Lost in Sound".into()),
        ..Default::default()
    });
    let found = seed_matching(&bare, None, "K3KHA", "Lost in Sound", None).expect("the catalog row's seed");
    assert_eq!(found.you_tube_id, None);
    assert_eq!(ExternalIdRegistry::make_short_id(&found), bare);

    let clean = registry.register(SoulseekRouting {
        kind: RoutingKind::Song,
        artist: Some("K3KHA".into()),
        title: Some("Lost in Sound".into()),
        version: Some("clean".into()),
        ..Default::default()
    });
    assert!(
        seed_matching(&clean, None, "K3KHA", "Lost in Sound", None)
            .is_some_and(|found| { ExternalIdRegistry::make_short_id(&found) == clean })
    );
}

/// `resolve_playback_sources` is PREFER_LOCAL both ways: a position whose file is one of the
/// available ids serves the library, the same position without its file serves the stream, and a
/// purely local position with neither its file nor a stream is left out (R-4).
#[test]
fn playback_sources_prefer_the_library_and_fall_back_to_the_stream() {
    let playlist = PersonalPlaylist {
        id: "nd-1".to_string(),
        entries: vec![
            PersonalPlaylistEntry {
                occurrence_id: "occ-1".to_string(),
                library_id: Some("nd-song-1".to_string()),
                outside_id: Some("outside-1".to_string()),
                you_tube_id: Some("video-1".to_string()),
                ..Default::default()
            },
            PersonalPlaylistEntry {
                occurrence_id: "occ-2".to_string(),
                library_id: Some("nd-song-gone".to_string()),
                outside_id: Some("outside-2".to_string()),
                you_tube_id: Some("video-2".to_string()),
                ..Default::default()
            },
            PersonalPlaylistEntry {
                occurrence_id: "occ-3".to_string(),
                library_id: Some("nd-song-3".to_string()),
                outside_id: None,
                ..Default::default()
            },
        ],
        ..Default::default()
    };

    let with_file = HashSet::from(["nd-song-1".to_string(), "nd-song-3".to_string()]);
    assert_eq!(
        playlist.resolve_playback_sources(&with_file),
        [
            ServeSource::Library {
                id: "nd-song-1".to_string(),
                outside: Some("outside-1".to_string()),
            },
            ServeSource::Outside {
                outside_id: "outside-2".to_string(),
                you_tube_id: Some("video-2".to_string()),
            },
            ServeSource::Library {
                id: "nd-song-3".to_string(),
                outside: None,
            },
        ]
    );

    // The file is gone (and the purely local position with it): the stream takes over, and the
    // position without one is left out rather than served broken.
    let empty = HashSet::new();
    assert_eq!(
        playlist.resolve_playback_sources(&empty),
        [
            ServeSource::Outside {
                outside_id: "outside-1".to_string(),
                you_tube_id: Some("video-1".to_string()),
            },
            ServeSource::Outside {
                outside_id: "outside-2".to_string(),
                you_tube_id: Some("video-2".to_string()),
            },
        ]
    );
}

/// Binding the library half keeps the outside half: the snapshot, the video and the seed all
/// survive, so the position falls back to the stream when the file goes again.
#[test]
fn binding_the_library_half_keeps_the_outside_half() {
    let store = PersonalPlaylistStore::in_memory();
    let seed = SoulseekRouting {
        kind: RoutingKind::Song,
        you_tube_id: Some("video-1".into()),
        artist: Some("K3KHA".into()),
        title: Some("Lost in Sound".into()),
        duration: Some(619),
        ..Default::default()
    };
    store.upsert(PersonalPlaylist {
        id: "nd-1".to_string(),
        entries: vec![PersonalPlaylistEntry {
            occurrence_id: "occ-1".to_string(),
            library_id: None,
            outside_id: Some("outside-1".to_string()),
            you_tube_id: Some("video-1".to_string()),
            source: None,
            song_snapshot: Some(snapshot("K3KHA", "Lost in Sound", 619)),
            seed: Some(seed),
        }],
        ..Default::default()
    });

    store.set_library_id("nd-1", "occ-1", Some("nd-song-1".into()));
    let bound = store.find("nd-1").expect("found");
    assert_eq!(bound.entries[0].library_id.as_deref(), Some("nd-song-1"));
    assert_eq!(bound.entries[0].outside_id.as_deref(), Some("outside-1"));
    assert_eq!(bound.entries[0].you_tube_id.as_deref(), Some("video-1"));
    assert!(bound.entries[0].song_snapshot.is_some());
    assert!(bound.entries[0].seed.is_some());

    // The file leaves again: one call takes the binding away, and the outside half is still there.
    store.set_library_id("nd-1", "occ-1", None);
    let released = store.find("nd-1").expect("found");
    assert_eq!(released.entries[0].library_id, None);
    assert_eq!(released.entries[0].outside_id.as_deref(), Some("outside-1"));
}

/// O3: parallel edits of one playlist all land. Every `edit` is ONE read-modify-write under the
/// store's own lock, so however many mutations run at once on the same playlist, none is lost —
/// the caller-side `find` → change → `upsert` this replaces could interleave two of them and
/// drop the loser's change.
#[test]
fn parallel_edits_of_one_playlist_all_land() {
    let store = Arc::new(PersonalPlaylistStore::in_memory());
    store.upsert(PersonalPlaylist {
        id: "nd-1".to_string(),
        owner: "alice".to_string(),
        name: "Mix".to_string(),
        entries: Vec::new(),
        ..Default::default()
    });
    const THREADS: usize = 4;
    const EDITS_PER_THREAD: usize = 250;
    let mut joins = Vec::new();
    for thread in 0..THREADS {
        let store = Arc::clone(&store);
        joins.push(std::thread::spawn(move || {
            for edit in 0..EDITS_PER_THREAD {
                let tag = format!("t{thread}-e{edit}");
                store
                    .edit("nd-1", |list| {
                        list.entries.push(PersonalPlaylistEntry {
                            occurrence_id: tag,
                            library_id: Some("nd-song-1".to_string()),
                            ..Default::default()
                        });
                    })
                    .expect("the playlist is held for every edit");
            }
        }));
    }
    for join in joins {
        join.join().expect("no thread panicked");
    }
    let held = store.find("nd-1").expect("the playlist is held");
    assert_eq!(
        held.entries.len(),
        THREADS * EDITS_PER_THREAD,
        "no parallel edit was lost"
    );
    let mut occurrence_ids: Vec<&str> = held
        .entries
        .iter()
        .map(|entry| entry.occurrence_id.as_str())
        .collect();
    occurrence_ids.sort_unstable();
    occurrence_ids.dedup();
    assert_eq!(
        occurrence_ids.len(),
        THREADS * EDITS_PER_THREAD,
        "every edit's own position is there exactly once"
    );
}

/// O3: two parallel adoptions of one plain playlist converge on whichever landed first:
/// `insert_if_absent` keeps the standing playlist, the second adoption does not overwrite it,
/// and the second caller's own edit lands ON the standing one — so neither side's change is
/// lost. An id the store does not hold answers `None` without writing anything.
#[test]
fn parallel_adoptions_converge_on_the_first_adoption() {
    let store = PersonalPlaylistStore::in_memory();
    let first = PersonalPlaylist {
        id: "nd-1".to_string(),
        owner: "alice".to_string(),
        name: "first".to_string(),
        entries: vec![PersonalPlaylistEntry {
            occurrence_id: "occ-first".to_string(),
            library_id: Some("nd-song-1".to_string()),
            ..Default::default()
        }],
        ..Default::default()
    };
    let mut second = first.clone();
    second.name = "second".to_string();
    second.entries = vec![PersonalPlaylistEntry {
        occurrence_id: "occ-second".to_string(),
        library_id: Some("nd-song-2".to_string()),
        ..Default::default()
    }];

    assert!(store.insert_if_absent(first), "the first adoption is taken");
    assert!(
        !store.insert_if_absent(second),
        "the second adoption does not overwrite the standing playlist"
    );
    assert!(
        store.edit("nd-absent", |_| ()).is_none(),
        "an id the store does not hold edits nothing"
    );

    let (held, ()) = store
        .edit("nd-1", |list| {
            list.entries.push(PersonalPlaylistEntry {
                occurrence_id: "occ-second-edit".to_string(),
                library_id: Some("nd-song-2".to_string()),
                ..Default::default()
            });
        })
        .expect("the standing playlist is held");
    assert_eq!(held.name, "first", "the first adoption stands");
    assert_eq!(
        held.entries
            .iter()
            .map(|entry| entry.occurrence_id.as_str())
            .collect::<Vec<_>>(),
        ["occ-first", "occ-second-edit"],
        "the second caller's change landed beside the first adoption's, none lost"
    );
}

/// A position's kept source survives the document's round trip: the label is stored beside the
/// snapshot (`Song.suggested_by` never is), so the badge on a playlist row outlives a restart
/// or an eviction with the position itself (SOAK-3b).
#[test]
fn a_positions_source_survives_the_document_round_trip() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("personal-playlists.json");
    let store = PersonalPlaylistStore::new(Some(&path));
    store.upsert(PersonalPlaylist {
        id: "nd-1".to_string(),
        entries: vec![PersonalPlaylistEntry {
            occurrence_id: "occ-1".to_string(),
            library_id: None,
            outside_id: Some("outside-1".to_string()),
            you_tube_id: Some("video-1".to_string()),
            source: Some("YouTube Music (video)".to_string()),
            song_snapshot: Some(snapshot("K3KHA", "Lost in Sound", 619)),
            seed: None,
        }],
        ..Default::default()
    });

    // The store as a restart leaves it: read back from the same file.
    let read = PersonalPlaylistStore::new(Some(&path));
    let back = read.find("nd-1").expect("the playlist came back");
    assert_eq!(
        back.entries[0].source.as_deref(),
        Some("YouTube Music (video)"),
        "the position's source survives the round trip"
    );
    assert_eq!(back.entries[0].outside_id.as_deref(), Some("outside-1"));
    assert!(back.entries[0].song_snapshot.is_some());
}
