//! The personal playlists overlay's tests, through the controller harness: the REST and native
//! CRUD, the local-only mirror, the heart's own download paths (R4-T4).

use axum::body::Body;
use axum::http::{Method, StatusCode};
use octo_core::settings::{AppSettings, SoulseekSettings};
use octo_core::soulseek::soulseek_metadata_service::{RoutingKind, SoulseekRouting};
use wiremock::matchers::{any, path};
use wiremock::{Mock, MockServer, Request as MockRequest, Respond, ResponseTemplate};

use crate::app::AppState;
use crate::controllers::subsonic::test_app::*;
use crate::http::pipeline::App;

/// The real id Navidrome hands back for an empty create.
const NEW_PLAYLIST: &str = "nd-mix";

/// Navidrome for these tests: the token "good" signs in, a `getSong` answers one library song,
/// and an empty `createPlaylist` (no playlist id — the am-yy move that asks for the real id)
/// answers the new playlist. Everything else is a plain ok.
struct PlaylistNavidrome;

impl Respond for PlaylistNavidrome {
    fn respond(&self, request: &MockRequest) -> ResponseTemplate {
        let query = query_of(request);
        if query.get("t").map(String::as_str) != Some("good") {
            return navidrome_json(NAVIDROME_WRONG_PASSWORD);
        }
        let path = request.url.path();
        if path.ends_with("/rest/getSong") || path.ends_with("/rest/getSong.view") {
            let id = query.get("id").cloned().unwrap_or_default();
            return navidrome_json(&format!(
                r#"{{"subsonic-response":{{"status":"ok","version":"1.16.1","song":{{"id":"{id}","title":"Local song","artist":"Local Artist","album":"Local Album","duration":200}}}}}}"#
            ));
        }
        if (path.ends_with("/rest/createPlaylist") || path.ends_with("/rest/createPlaylist.view"))
            && !query.contains_key("playlistId")
        {
            return navidrome_json(&format!(
                r#"{{"subsonic-response":{{"status":"ok","version":"1.16.1","playlist":{{"id":"{NEW_PLAYLIST}","name":"Mix"}}}}}}"#
            ));
        }
        navidrome_json(NAVIDROME_OK)
    }
}

/// The app over the wiremock Navidrome, plus what these tests read off it.
struct PersonalApp {
    state: AppState,
    app: App,
    navidrome: MockServer,
}

impl PersonalApp {
    async fn new() -> PersonalApp {
        let navidrome = MockServer::builder().start().await;
        Mock::given(any())
            .respond_with(PlaylistNavidrome)
            .mount(&navidrome)
            .await;
        let settings = AppSettings {
            soulseek: SoulseekSettings {
                base_url: Some("http://127.0.0.1:1".into()),
                ..Default::default()
            },
            ..settings(&navidrome.uri())
        };
        let state = state_with(settings, |_| {});
        PersonalApp {
            app: app(&state),
            state,
            navidrome,
        }
    }

    /// One outside song in the registry, by its short id.
    fn outside_song(&self, title: &str) -> String {
        self.state.external_id_registry.register(SoulseekRouting {
            kind: RoutingKind::Song,
            artist: Some("K3KHA".into()),
            title: Some(title.into()),
            duration: Some(619),
            ..Default::default()
        })
    }

    /// One `createPlaylist` call, `query` beside the sign-in and the JSON format.
    async fn create(&self, query: &str) -> Reply {
        get(
            &self.app,
            &format!(
                "/rest/createPlaylist.view?{}&f=json&{query}",
                auth("alice", "good", "octo")
            ),
        )
        .await
    }

    /// A mixed create of one library song and one outside one, answered by the overlay.
    async fn create_mixed(&self, outside: &str) {
        let reply = self
            .create(&format!("name=Mix&songId=nd-2&songId={outside}"))
            .await;
        assert_eq!(reply.envelope()["status"], "ok", "{}", reply.text());
    }

    /// The held playlist itself.
    fn held(&self, id: &str) -> crate::services::subsonic::personal_playlist_store::PersonalPlaylist {
        self.state
            .personal_playlists
            .find(id)
            .expect("the playlist is held")
    }

    /// The entries of a held playlist, as their (library id, outside id) halves.
    fn entries_of(&self, id: &str) -> Vec<(Option<String>, Option<String>)> {
        self.held(id)
            .entries
            .iter()
            .map(|entry| (entry.library_id.clone(), entry.outside_id.clone()))
            .collect()
    }

    /// Every `songId` every request Navidrome received carried, one list per request.
    async fn song_ids_mirrored(&self) -> Vec<Vec<String>> {
        self.navidrome
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .map(|request| {
                request
                    .url
                    .query_pairs()
                    .filter(|(key, _)| key == "songId")
                    .map(|(_, value)| value.into_owned())
                    .collect()
            })
            .collect()
    }

    // The Boundary Rule of adding to a playlist: nothing was ever queued to download.
    fn nothing_downloads(&self) -> bool {
        self.state.acquisition_tracker.all().is_empty() && self.state.track_acquisition_queue.is_idle()
    }

    /// Polls until `external_id` is in flight on the acquisition queue (what a star on a
    /// playlist's missing positions leaves behind).
    async fn waits_in_flight(&self, external_id: &str) -> bool {
        for _ in 0..200 {
            if self
                .state
                .track_acquisition_queue
                .is_in_flight("soulseek", external_id)
            {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        false
    }

    /// How many `rest/getSong` relays Navidrome has seen: an answer of the overlay's costs none
    /// of them — the whole list comes from one read of the playlist's own mirror.
    async fn get_song_relays(&self) -> usize {
        self.navidrome
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter(|request| request.url.path().ends_with("/rest/getSong"))
            .count()
    }

    /// Makes the playlist's own mirror answer `rows` to the overlay's one bulk read. Mounted
    /// over the base mock with the highest priority, so it is the one Navidrome answers
    /// `rest/getPlaylist` with.
    async fn mirror_answers(&self, rows: &str) {
        Mock::given(path("/rest/getPlaylist"))
            .respond_with(navidrome_json(&format!(
                r#"{{"subsonic-response":{{"status":"ok","version":"1.16.1","playlist":{{"id":"{NEW_PLAYLIST}","entry":{rows}}}}}}}"#
            )))
            .with_priority(1)
            .mount(&self.navidrome)
            .await;
    }
}

/// A mixed create makes the empty playlist in Navidrome first (that call carries no songId —
/// am-yy's move), takes the real id back, keeps both halves of every position with the seed
/// that minted the outside one, and mirrors the local ids only.
#[tokio::test]
async fn a_mixed_create_adopts_navidromes_real_id_and_mirrors_only_local_ids() {
    let t = PersonalApp::new().await;
    let outside = t.outside_song("Lost in Sound");

    let reply = t.create(&format!("name=Mix&songId=nd-2&songId={outside}")).await;

    assert_eq!(reply.envelope()["status"], "ok", "{}", reply.text());
    assert_eq!(
        t.entries_of(NEW_PLAYLIST),
        [(Some("nd-2".to_string()), None), (None, Some(outside.clone())),]
    );
    let playlist = t.held(NEW_PLAYLIST);
    let entry = &playlist.entries[1];
    assert!(
        entry.seed.is_some(),
        "the position keeps the routing that minted its id"
    );
    assert_eq!(
        entry.song_snapshot.as_ref().and_then(|song| song.duration),
        Some(619)
    );
    // The first create carries no songId at all (the playlist is made empty, for its real id);
    // the mirror carries the local ids only — never the registry's — the metadata update and the
    // mirror's one read-back (the local snapshot's bulk fill) none.
    assert_eq!(
        t.song_ids_mirrored().await,
        [Vec::new(), vec!["nd-2".to_string()], Vec::new(), Vec::new()]
    );
    assert!(t.nothing_downloads());
}

/// `createPlaylist` with `playlistId` is Subsonic's replace-the-list call: a held playlist gets
/// a whole new list of positions, whatever it held.
#[tokio::test]
async fn a_create_with_a_playlist_id_replaces_the_whole_list() {
    let t = PersonalApp::new().await;
    let outside = t.outside_song("Lost in Sound");
    t.create_mixed(&outside).await;

    let reply = t.create(&format!("playlistId={NEW_PLAYLIST}&songId=nd-9")).await;

    assert_eq!(reply.envelope()["status"], "ok", "{}", reply.text());
    assert_eq!(t.entries_of(NEW_PLAYLIST), [(Some("nd-9".to_string()), None)]);
    assert!(t.nothing_downloads());
}

/// `updatePlaylist` renames (a folder in the name and the comment's tag comes off as the folder
/// field), removes a position by its index and adds one; the outside half survives it all.
#[tokio::test]
async fn an_update_renames_folders_adds_and_removes() {
    let t = PersonalApp::new().await;
    let outside = t.outside_song("Lost in Sound");
    t.create_mixed(&outside).await;

    let reply = get(
        &t.app,
        &format!(
            "/rest/updatePlaylist.view?{}&f=json&playlistId={NEW_PLAYLIST}&name=Techno%20%2F%20Peak%20Time%20%2F%20Mix&comment=%23folder%3ATechno%2FPeak%20Time&songIndexToRemove=0&songIdToAdd=nd-9",
            auth("alice", "good", "octo")
        ),
    )
    .await;

    assert_eq!(reply.envelope()["status"], "ok", "{}", reply.text());
    // The local first position is removed, the outside one stays, the new local one is last.
    assert_eq!(
        t.entries_of(NEW_PLAYLIST),
        [(None, Some(outside.clone())), (Some("nd-9".to_string()), None),]
    );
    let playlist = t.held(NEW_PLAYLIST);
    assert_eq!(playlist.name, "Mix");
    assert_eq!(playlist.folder.as_deref(), Some("Techno/Peak Time"));
    assert_eq!(playlist.display_name(), "Techno / Peak Time / Mix");
    for ids in t.song_ids_mirrored().await {
        assert!(
            ids.iter().all(|id| id.starts_with("nd-")),
            "the mirror carries local ids only: {ids:?}"
        );
    }
    assert!(t.nothing_downloads());
}

/// `deletePlaylist` takes the playlist in Navidrome and the overlay with it.
#[tokio::test]
async fn a_delete_removes_the_playlist_and_the_overlay() {
    let t = PersonalApp::new().await;
    let outside = t.outside_song("Lost in Sound");
    t.create_mixed(&outside).await;

    let reply = get(
        &t.app,
        &format!(
            "/rest/deletePlaylist.view?{}&f=json&id={NEW_PLAYLIST}",
            auth("alice", "good", "octo")
        ),
    )
    .await;

    assert_eq!(reply.envelope()["status"], "ok", "{}", reply.text());
    assert!(!t.state.personal_playlists.knows(NEW_PLAYLIST));
    assert!(
        t.navidrome
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .any(|request| request.url.path().ends_with("/rest/deletePlaylist")),
        "the playlist is deleted in Navidrome too"
    );
}

/// A native `POST /api/playlist/{id}/tracks` with a mixed body (`ids`, read off the request's
/// own JSON body) adds both positions, mirrors the local half only, and queues nothing to
/// download (the Boundary Rule).
#[tokio::test]
async fn a_native_tracks_post_adds_mixed_positions_without_downloading() {
    let t = PersonalApp::new().await;
    let outside = t.outside_song("Lost in Sound");
    t.create_mixed(&outside).await;
    let later = t.outside_song("Deep Minimal Techno Set");

    let reply = send(
        &t.app,
        Method::POST,
        &format!(
            "/api/playlist/{NEW_PLAYLIST}/tracks?{}",
            auth("alice", "good", "octo")
        ),
        &[("content-type", "application/json")],
        Body::from(format!(r#"{{"ids":["nd-9","{later}"]}}"#)),
    )
    .await;

    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(reply.json()["added"], 2, "{}", reply.text());
    assert_eq!(
        t.entries_of(NEW_PLAYLIST),
        [
            (Some("nd-2".to_string()), None),
            (None, Some(outside.clone())),
            (Some("nd-9".to_string()), None),
            (None, Some(later.clone())),
        ]
    );
    for ids in t.song_ids_mirrored().await {
        assert!(
            ids.iter().all(|id| id.starts_with("nd-")),
            "the mirror carries local ids only: {ids:?}"
        );
    }
    assert!(t.nothing_downloads());
}

/// A star on a held playlist is the explicit "download the missing ones" (R-5 operation 4):
/// only the position without a library file is queued, straight through the heart's
/// coordinator — and an unstar fetches nothing.
#[tokio::test]
async fn a_star_on_a_playlist_fetches_only_its_missing_positions() {
    let t = PersonalApp::new().await;
    let outside = t.outside_song("Lost in Sound");
    t.create_mixed(&outside).await;

    let reply = get(
        &t.app,
        &format!(
            "/rest/star.view?{}&f=json&id={NEW_PLAYLIST}",
            auth("alice", "good", "octo")
        ),
    )
    .await;
    assert_eq!(reply.envelope()["status"], "ok", "{}", reply.text());
    assert!(
        t.waits_in_flight(&outside).await,
        "the position without a library file is fetched"
    );
    assert!(
        !t.state.track_acquisition_queue.is_in_flight("soulseek", "nd-2"),
        "the position with a library file is left alone"
    );

    let reply = get(
        &t.app,
        &format!(
            "/rest/unstar.view?{}&f=json&id={NEW_PLAYLIST}",
            auth("alice", "good", "octo")
        ),
    )
    .await;
    assert_eq!(reply.envelope()["status"], "ok", "{}", reply.text());
    assert!(
        !t.state.track_acquisition_queue.is_in_flight("soulseek", "nd-2"),
        "an unstar fetches nothing"
    );
}

/// The drag-and-drop move carries its positions with it: the occurrence ids stay the very same,
/// only their order changes — and the tracks answer numbers them 1..N with the source each
/// plays by.
#[tokio::test]
async fn a_native_move_carries_the_occurrence_ids_along() {
    let t = PersonalApp::new().await;
    let outside = t.outside_song("Lost in Sound");
    let reply = t
        .create(&format!("name=Mix&songId=nd-2&songId={outside}&songId=nd-3"))
        .await;
    assert_eq!(reply.envelope()["status"], "ok", "{}", reply.text());
    let before: Vec<String> = t
        .held(NEW_PLAYLIST)
        .entries
        .iter()
        .map(|entry| entry.occurrence_id.clone())
        .collect();
    assert_eq!(before.len(), 3);

    let reply = send(
        &t.app,
        Method::PUT,
        &format!(
            "/api/playlist/{NEW_PLAYLIST}/tracks/3?{}",
            auth("alice", "good", "octo")
        ),
        &[("content-type", "application/json")],
        Body::from(r#"{"insert_before":"1"}"#),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());

    // The same three positions, the last one first: nothing was merged or re-minted.
    let playlist = t.held(NEW_PLAYLIST);
    let after: Vec<String> = playlist
        .entries
        .iter()
        .map(|entry| entry.occurrence_id.clone())
        .collect();
    assert_eq!(
        after,
        [before[2].clone(), before[0].clone(), before[1].clone()],
        "the occurrence ids move, they are not re-created"
    );
    assert_eq!(
        t.entries_of(NEW_PLAYLIST),
        [
            (Some("nd-3".to_string()), None),
            (Some("nd-2".to_string()), None),
            (None, Some(outside.clone())),
        ]
    );

    let reply = get(
        &t.app,
        &format!(
            "/api/playlist/{NEW_PLAYLIST}/tracks?{}",
            auth("alice", "good", "octo")
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(reply.header("x-total-count"), Some("3"));
    let rows = reply.json().as_array().expect("a tracks array").clone();
    let ids: Vec<u64> = rows
        .iter()
        .map(|row| row["id"].as_u64().expect("a 1-based position"))
        .collect();
    assert_eq!(ids, [1, 2, 3]);
    let media: Vec<String> = rows
        .iter()
        .map(|row| row["mediaFileId"].as_str().expect("a source id").to_string())
        .collect();
    assert_eq!(media, ["nd-3", "nd-2", outside.as_str()]);
}

/// One read of the playlist's own mirror decides every position's source at the moment of the
/// answer, in both protocols at once: the row the mirror still holds plays the library (and the
/// REST answer is that live row) — and not one `rest/getSong` is relayed for any of it, however
/// many positions the list holds (the store's `resolve_playback_sources` is the one mechanism
/// both answers go by).
#[tokio::test]
async fn both_protocols_pick_their_sources_from_one_mirror_read() {
    let t = PersonalApp::new().await;
    let outside = t.outside_song("Lost in Sound");
    t.create_mixed(&outside).await;
    // A mixed first position: one place holding the library file and the stream it can fall
    // back to (what a download landing, then the file leaving, leaves behind — R-4).
    let mut held = t.held(NEW_PLAYLIST);
    held.entries[0].outside_id = Some(outside.clone());
    t.state.personal_playlists.upsert(held);

    // The mirror still holds nd-2's row: the file is there.
    t.mirror_answers(
        r#"[{"id":"nd-2","title":"Local FLAC song","artist":"Someone","album":"Somewhere","duration":200}]"#,
    )
    .await;

    let reply = get(
        &t.app,
        &format!(
            "/rest/getPlaylist.view?{}&f=json&id={NEW_PLAYLIST}",
            auth("alice", "good", "octo")
        ),
    )
    .await;
    assert_eq!(reply.envelope()["status"], "ok", "{}", reply.text());
    let entries = reply.envelope()["playlist"]["entry"]
        .as_array()
        .cloned()
        .expect("the playlist's entries");
    assert_eq!(entries.len(), 2, "{}", reply.text());
    assert_eq!(entries[0]["id"], "nd-2", "the library's own live row");
    assert_eq!(entries[0]["title"], "Local FLAC song");
    assert_eq!(entries[1]["id"], outside.as_str(), "the outside position");

    let reply = get(
        &t.app,
        &format!(
            "/api/playlist/{NEW_PLAYLIST}/tracks?{}",
            auth("alice", "good", "octo")
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    let media: Vec<String> = reply
        .json()
        .as_array()
        .expect("a tracks array")
        .iter()
        .map(|row| row["mediaFileId"].as_str().expect("a source id").to_string())
        .collect();
    assert_eq!(media, ["nd-2", outside.as_str()]);

    // The whole exchange — create and answers — never relayed a single `rest/getSong`.
    assert_eq!(t.get_song_relays().await, 0, "no per-position song reads");
}

/// The mirror's one read no longer lists a position's row the very moment its file went, and
/// both protocols take the position to the stream at once — the position never disappears, and
/// no reconciler minute sits in between (the native answer used to hand out the dead library id
/// until the reconciler's pass).
#[tokio::test]
async fn a_file_that_went_takes_its_position_to_the_stream_at_once() {
    let t = PersonalApp::new().await;
    let outside = t.outside_song("Lost in Sound");
    t.create_mixed(&outside).await;
    // A mixed first position, as above: the library file it had is gone with the row below.
    let mut held = t.held(NEW_PLAYLIST);
    held.entries[0].outside_id = Some(outside.clone());
    t.state.personal_playlists.upsert(held);

    // The mirror holds nothing: the file is gone, and the store still names it.
    t.mirror_answers("[]").await;

    let reply = get(
        &t.app,
        &format!(
            "/rest/getPlaylist.view?{}&f=json&id={NEW_PLAYLIST}",
            auth("alice", "good", "octo")
        ),
    )
    .await;
    assert_eq!(reply.envelope()["status"], "ok", "{}", reply.text());
    let entries = reply.envelope()["playlist"]["entry"]
        .as_array()
        .cloned()
        .expect("the playlist's entries");
    assert_eq!(entries.len(), 2, "{}", reply.text());
    assert_eq!(entries[0]["id"], outside.as_str(), "the stream takes over");
    assert_eq!(entries[1]["id"], outside.as_str());

    let reply = get(
        &t.app,
        &format!(
            "/api/playlist/{NEW_PLAYLIST}/tracks?{}",
            auth("alice", "good", "octo")
        ),
    )
    .await;
    assert_eq!(reply.header("x-total-count"), Some("2"));
    let media: Vec<String> = reply
        .json()
        .as_array()
        .expect("a tracks array")
        .iter()
        .map(|row| row["mediaFileId"].as_str().expect("a source id").to_string())
        .collect();
    assert_eq!(media, [outside.as_str(), outside.as_str()]);
    assert_eq!(t.get_song_relays().await, 0, "no per-position song reads");
}
