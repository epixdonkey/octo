//! The personal playlists overlay's tests, through the controller harness: the REST and native
//! CRUD, the local-only mirror, the heart's own download paths (R4-T4).

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, StatusCode};
use octo_core::models::domain::Song;
use octo_core::settings::{AppSettings, SoulseekSettings, SubsonicSettings};
use octo_core::soulseek::soulseek_metadata_service::{RoutingKind, SoulseekRouting};
use serde_json::json;
use wiremock::matchers::{any, method, path};
use wiremock::{Mock, MockServer, Request as MockRequest, Respond, ResponseTemplate};

use crate::app::{AppState, TestServices};
use crate::controllers::subsonic::test_app::*;
use crate::http::pipeline::App;
use crate::services::i_music_metadata_service::IMusicMetadataService;
use crate::services::imports::test_support::row;
use crate::services::soulseek::ExternalIdRegistry;
use crate::services::subsonic::personal_playlist_reconciler::PersonalPlaylistReconciler;
use crate::services::subsonic::personal_playlist_store::{PersonalPlaylist, PersonalPlaylistEntry};

/// The real id Navidrome hands back for an empty create.
const NEW_PLAYLIST: &str = "nd-mix";

/// A plain playlist only Navidrome holds (never the overlay's), standing in its native list
/// with one local track — what adoption reads.
const PLAIN_PLAYLIST: &str = "nd-other";

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

    /// The app as `new`, with test services of the test's own: the registry the outside ids are
    /// minted in before the app exists, the download service the streams come from.
    async fn new_with_services(services: TestServices) -> PersonalApp {
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
        let state = AppState::for_tests_with(settings, services);
        PersonalApp {
            app: app(&state),
            state,
            navidrome,
        }
    }

    /// The app as above, with the service API's own admin identity (what adoption reads
    /// Navidrome's native rows by) and `PLAIN_PLAYLIST` standing in the native list with one
    /// local track: here adoption has everything it needs, so a body carrying a registry id can
    /// take the playlist over.
    async fn new_with_a_plain_playlist() -> PersonalApp {
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
            subsonic: SubsonicSettings {
                url: Some(navidrome.uri()),
                admin_username: Some("admin".into()),
                admin_password: Some("pw".into()),
                auto_detect_download_path: false,
                ..Default::default()
            },
            ..Default::default()
        };
        let state = state_with(settings, |_| {});
        let t = PersonalApp {
            app: app(&state),
            state,
            navidrome,
        };
        // Navidrome's native rows, answered over the base mock: the admin signs in, the plain
        // playlist is listed, and its one track with it.
        Mock::given(method("POST"))
            .and(path("/auth/login"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "token": "jwt-1", "isAdmin": true, "username": "admin"
            })))
            .with_priority(1)
            .mount(&t.navidrome)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/playlist"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([{
                "id": PLAIN_PLAYLIST, "name": "Plain List", "ownerName": "admin"
            }])))
            .with_priority(1)
            .mount(&t.navidrome)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/api/playlist/{PLAIN_PLAYLIST}/tracks")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                { "id": 1, "mediaFileId": "nd-1" }
            ])))
            .with_priority(1)
            .mount(&t.navidrome)
            .await;
        t
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

    /// The bodies of the requests Navidrome itself received as `method` on the playlist's own
    /// path, in order — the faithful relay's signature, what a passthrough leaves behind.
    async fn navidrome_relayed_bodies(&self, method: &str, path: &str) -> Vec<String> {
        self.navidrome
            .received_requests()
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|request| request.method.as_str() == method && request.url.path() == path)
            .map(|request| String::from_utf8_lossy(&request.body).to_string())
            .collect()
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

/// A read heals the snapshots the add could not fill (O5): the answer's own mirror read — paid
/// for anyway, and this app configures no service account at all, so the caller's own sign-in
/// did the reading — fills a bare local position's snapshot right there, without a second
/// relay. And once the snapshot stands in the store, a native row that cannot reach the mirror
/// keeps its words from it (O6) instead of coming back a bare `id`-only line.
#[tokio::test]
async fn a_read_fills_a_bare_local_snapshot_from_the_mirror_it_already_read() {
    let t = PersonalApp::new().await;
    let outside = t.outside_song("Lost in Sound");
    t.create_mixed(&outside).await;
    // The add could not fill the local half: the base mock's getPlaylist carries no rows.
    let bare = t.held(NEW_PLAYLIST);
    let local = bare
        .entries
        .iter()
        .find(|entry| entry.library_id.is_some())
        .expect("the local position");
    assert!(local.song_snapshot.is_none(), "the add left the local half bare");

    // The mirror answers nd-2's row: the read itself heals the store, at no extra relay.
    t.mirror_answers(
        r#"[{"id":"nd-2","title":"Local FLAC song","artist":"Someone","album":"Somewhere","duration":200}]"#,
    )
    .await;
    let get_playlist_relays_before = t
        .navidrome
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|request| request.url.path().ends_with("/rest/getPlaylist"))
        .count();
    let reply = get(
        &t.app,
        &format!(
            "/rest/getPlaylist.view?{}&f=json&id={NEW_PLAYLIST}",
            auth("alice", "good", "octo")
        ),
    )
    .await;
    assert_eq!(reply.envelope()["status"], "ok", "{}", reply.text());
    let healed = t.held(NEW_PLAYLIST);
    let local = healed
        .entries
        .iter()
        .find(|entry| entry.library_id.is_some())
        .expect("the local position");
    let snapshot = local
        .song_snapshot
        .as_ref()
        .expect("the read filled the bare snapshot from the mirror it already read");
    assert_eq!(snapshot.title, "Local FLAC song");
    assert_eq!(snapshot.artist, "Someone");
    assert_eq!(snapshot.duration, Some(200));
    let get_playlist_relays_after = t
        .navidrome
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|request| request.url.path().ends_with("/rest/getPlaylist"))
        .count();
    assert_eq!(
        get_playlist_relays_after,
        get_playlist_relays_before + 1,
        "the healing cost no relay of its own — the answer's one read did it"
    );

    // O6: a native call that names nobody and no service account configured cannot ask the
    // mirror at all — the row keeps its words from the healed snapshot, not a bare line.
    let reply = get(&t.app, &format!("/api/playlist/{NEW_PLAYLIST}/tracks")).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    let rows = reply.json().as_array().cloned().expect("a tracks array");
    let local_row = rows
        .iter()
        .find(|row| row["mediaFileId"] == "nd-2")
        .expect("the local position's row");
    assert_eq!(local_row["title"], "Local FLAC song", "not a bare row");
    assert_eq!(local_row["artist"], "Someone");
    assert_eq!(local_row["duration"], 200);
    assert_eq!(t.get_song_relays().await, 0, "no per-position song reads");
}

/// A native whole-list replacement on a plain playlist the overlay does not hold, with purely
/// local rows (`tracks` or an empty `tracks`), is an ordinary edit: the faithful relay takes the
/// PUT to Navidrome exactly as it came and the store never learns the playlist. The gate is the
/// create's own — only a registry id takes a playlist over — so the 403 of an unreachable
/// adoption (this app has no service API identity to adopt by) never stands between the client
/// and an edit Navidrome could have made itself.
#[tokio::test]
async fn a_purely_local_replacement_on_a_plain_playlist_is_navidromes_own() {
    let t = PersonalApp::new().await;
    let playlist_path = format!("/api/playlist/{PLAIN_PLAYLIST}");

    let bodies = [
        r#"{"name":"Plain List","tracks":[{"mediaFileId":"nd-1"},{"mediaFileId":"nd-2"}]}"#,
        r#"{"tracks":[]}"#,
    ];
    for body in bodies {
        let reply = send(
            &t.app,
            Method::PUT,
            &format!("{playlist_path}?{}", auth("alice", "good", "octo")),
            &[("content-type", "application/json")],
            Body::from(body),
        )
        .await;

        // Navidrome's own answer came back (the mock's ok envelope, not an overlay detail row),
        // not a 403: the replacement was executed by Navidrome.
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
        assert_eq!(reply.envelope()["status"], "ok", "{}", reply.text());
    }

    // Both replacements reached Navidrome exactly as they were sent, the empty `tracks` too.
    assert_eq!(
        t.navidrome_relayed_bodies("PUT", &playlist_path).await,
        bodies.to_vec(),
        "the replacements are relayed to Navidrome unchanged"
    );

    // The overlay adopted nothing: the playlist stays Navidrome's alone.
    assert!(!t.state.personal_playlists.knows(PLAIN_PLAYLIST));
    assert!(t.nothing_downloads());
}

/// The same replacement with a registry id among the rows is the overlay's: Navidrome would
/// refuse that id whole (Fakt A1), so the playlist is adopted — its own local row kept first,
/// the mixed list stored, the local half mirrored alone — and the answer is the overlay's own
/// detail row, while the PUT itself never reaches Navidrome.
#[tokio::test]
async fn a_replacement_carrying_a_registry_id_adopts_the_plain_playlist() {
    let t = PersonalApp::new_with_a_plain_playlist().await;
    let outside = t.outside_song("Lost in Sound");
    let playlist_path = format!("/api/playlist/{PLAIN_PLAYLIST}");

    let body = format!(
        r#"{{"name":"Plain List","tracks":[{{"mediaFileId":"nd-1"}},{{"mediaFileId":"{outside}"}}]}}"#
    );
    let reply = send(
        &t.app,
        Method::PUT,
        &format!("{playlist_path}?{}", auth("alice", "good", "octo")),
        &[("content-type", "application/json")],
        Body::from(body),
    )
    .await;

    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(reply.json()["songCount"], 2, "{}", reply.text());
    assert_eq!(
        t.entries_of(PLAIN_PLAYLIST),
        [(Some("nd-1".to_string()), None), (None, Some(outside.clone())),]
    );
    for ids in t.song_ids_mirrored().await {
        assert!(
            ids.iter().all(|id| id.starts_with("nd-")),
            "the mirror carries local ids only: {ids:?}"
        );
    }
    assert!(
        t.navidrome_relayed_bodies("PUT", &playlist_path).await.is_empty(),
        "the replacement is the overlay's, not a passthrough"
    );
    assert!(t.nothing_downloads());
}

// ---- The heart's own download doors (D15, R-5 operations 3 and 4) -----------------------

/// A song snapshot as a position keeps it: what the reconciler matches and the counts sum.
fn snapshot_song(artist: &str, title: &str, seconds: i32) -> Song {
    Song {
        artist: artist.to_string(),
        title: title.to_string(),
        duration: Some(seconds),
        ..Default::default()
    }
}

/// A `[download]` tag in a held playlist's comment (the REST door) is the explicit "fetch the
/// missing ones" (R-5 operation 4): only the position without a library file is queued, the tag
/// itself never stays in the stored comment, and the edit changes no positions.
#[tokio::test]
async fn a_download_tag_on_a_held_playlist_fetches_only_its_missing_positions() {
    let t = PersonalApp::new().await;
    let outside = t.outside_song("Lost in Sound");
    t.create_mixed(&outside).await;

    let reply = get(
        &t.app,
        &format!(
            "/rest/updatePlaylist.view?{}&f=json&playlistId={NEW_PLAYLIST}&comment=please%20%5Bdownload%5D",
            auth("alice", "good", "octo")
        ),
    )
    .await;

    assert_eq!(reply.envelope()["status"], "ok", "{}", reply.text());
    assert!(
        t.waits_in_flight(&outside).await,
        "the tag fetches the position without a library file"
    );
    assert!(
        !t.state.track_acquisition_queue.is_in_flight("soulseek", "nd-2"),
        "the position with a library file is left alone"
    );
    let held = t.held(NEW_PLAYLIST);
    assert_eq!(
        held.comment, "please",
        "the tag is read off the comment, not kept in it"
    );
    assert_eq!(held.entries.len(), 2, "the comment edit changes no positions");
}

/// The same `[download]` tag through the native door (a PUT on the playlist's metadata): the
/// missing position is fetched and the local one is not — both doors of R-5 operation 4 lead to
/// the heart's one queue.
#[tokio::test]
async fn a_native_put_with_the_download_tag_fetches_only_its_missing_positions() {
    let t = PersonalApp::new().await;
    let outside = t.outside_song("Lost in Sound");
    t.create_mixed(&outside).await;

    let reply = send(
        &t.app,
        Method::PUT,
        &format!("/api/playlist/{NEW_PLAYLIST}?{}", auth("alice", "good", "octo")),
        &[("content-type", "application/json")],
        Body::from(r#"{"comment":"please [download]"}"#),
    )
    .await;

    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert!(
        t.waits_in_flight(&outside).await,
        "the tag fetches the missing position"
    );
    assert!(
        !t.state.track_acquisition_queue.is_in_flight("soulseek", "nd-2"),
        "the local position is left alone"
    );
    assert_eq!(t.held(NEW_PLAYLIST).comment, "please");
}

/// `/rest/download?id=<outside>&save=1` (R-5 operation 3): the fetch is queued beside the
/// stream, and the answer is the very bytes the plain download streams — `save` changes what is
/// queued, never what is answered.
#[tokio::test]
async fn a_save_one_download_fetches_the_outside_song_and_still_streams_it() {
    let registry = Arc::new(ExternalIdRegistry::in_memory());
    let outside = registry.register(SoulseekRouting {
        kind: RoutingKind::Song,
        artist: Some("K3KHA".into()),
        title: Some("Lost in Sound".into()),
        duration: Some(619),
        ..Default::default()
    });
    let bytes: &'static [u8] = &[7, 7, 7];
    let downloads = Arc::new(FakeStreams::streaming("soulseek", &outside, bytes, "audio/mp4"));
    let t = PersonalApp::new_with_services(TestServices {
        external_id_registry: Some(registry),
        download_service: Some(downloads.clone()),
        ..Default::default()
    })
    .await;

    let plain = get(
        &t.app,
        &format!("/rest/download?{}&id={outside}", auth("alice", "good", "octo")),
    )
    .await;
    assert_eq!(plain.status, StatusCode::OK, "{}", plain.text());
    assert_eq!(plain.body.as_ref(), bytes);
    assert!(t.nothing_downloads(), "a plain download queues nothing");

    let saved = get(
        &t.app,
        &format!(
            "/rest/download?{}&id={outside}&save=1",
            auth("alice", "good", "octo")
        ),
    )
    .await;
    assert_eq!(saved.status, StatusCode::OK, "{}", saved.text());
    assert_eq!(
        saved.body.as_ref(),
        bytes,
        "the answer is the stream's own bytes, save or not"
    );
    assert!(t.waits_in_flight(&outside).await, "save=1 queues the fetch");
}

/// What the owner puts on their own "Request a copy in FLAC" playlist is fetched straight away
/// through the heart's queue (R-5 operation 3, D15 — not a family request): the outside song
/// waiting on the playlist and nothing else, the library songs untouched, and the playlist
/// emptied afterwards as the fetch is the record. The playlist never becomes the overlay's own.
/// The caller is the playlist's owner (O2's gate: the row names them).
#[tokio::test]
async fn what_the_owner_puts_on_their_flac_playlist_is_fetched_straight_away() {
    let t = PersonalApp::new().await;
    let outside = t.outside_song("Lost in Sound");
    Mock::given(path("/rest/getPlaylist"))
        .respond_with(navidrome_json(&format!(
            r#"{{"subsonic-response":{{"status":"ok","playlist":{{"id":"nd-flac","owner":"alice","name":"Request a copy in FLAC","entry":[{{"id":"{outside}"}},{{"id":"nd-1"}}]}}}}}}"#
        )))
        .with_priority(1)
        .mount(&t.navidrome)
        .await;

    let reply = get(
        &t.app,
        &format!(
            "/rest/updatePlaylist.view?{}&f=json&playlistId=nd-flac&songIdToAdd=nd-9",
            auth("alice", "good", "octo")
        ),
    )
    .await;

    assert_eq!(reply.envelope()["status"], "ok", "{}", reply.text());
    assert!(
        t.waits_in_flight(&outside).await,
        "the outside song carried by the playlist is fetched"
    );
    assert!(
        !t.state.track_acquisition_queue.is_in_flight("soulseek", "nd-9")
            && !t.state.track_acquisition_queue.is_in_flight("soulseek", "nd-1"),
        "library songs need no fetch"
    );
    assert!(
        t.navidrome
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .any(|request| {
                request.url.path().ends_with("/rest/updatePlaylist")
                    && request
                        .url
                        .query_pairs()
                        .any(|(key, _)| key == "songIndexToRemove")
            }),
        "the carried songs are taken off the playlist again: the fetch is the record"
    );
    assert!(
        !t.state.personal_playlists.knows("nd-flac"),
        "the owner's FLAC playlist never becomes one of the overlay's own"
    );
}

/// Taking a position OFF the FLAC playlist — or deleting the playlist outright — is an
/// ordinary edit, never a fetch (O1): nothing is queued for a song the person just took off,
/// the removal is Navidrome's own to do (the client's call relayed as it came), and Octo adds
/// no emptying sweep of its own. The members' request playlists keep their own semantics.
#[tokio::test]
async fn taking_a_position_off_the_flac_playlist_queues_no_fetch() {
    let t = PersonalApp::new().await;
    let outside = t.outside_song("Lost in Sound");
    Mock::given(path("/rest/getPlaylist"))
        .respond_with(navidrome_json(&format!(
            r#"{{"subsonic-response":{{"status":"ok","playlist":{{"id":"nd-flac","owner":"alice","name":"Request a copy in FLAC","entry":[{{"id":"{outside}"}},{{"id":"nd-1"}}]}}}}}}"#
        )))
        .with_priority(1)
        .mount(&t.navidrome)
        .await;

    // A removal-only update: the position comes off, nothing is fetched for it.
    let reply = get(
        &t.app,
        &format!(
            "/rest/updatePlaylist.view?{}&f=json&playlistId=nd-flac&songIndexToRemove=0",
            auth("alice", "good", "octo")
        ),
    )
    .await;
    assert_eq!(reply.envelope()["status"], "ok", "{}", reply.text());
    assert!(t.nothing_downloads(), "a removal queues no fetch");
    let updates = t
        .navidrome
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|request| request.url.path().ends_with("/rest/updatePlaylist"))
        .collect::<Vec<_>>();
    assert_eq!(
        updates.len(),
        1,
        "exactly the client's own removal is relayed — no Octo-made sweep"
    );
    assert!(
        updates[0]
            .url
            .query_pairs()
            .any(|(key, value)| key == "songIndexToRemove" && value == "0")
    );

    // Deleting the playlist is the same ordinary edit: relayed, nothing fetched (the
    // provisioner brings the playlist back on the next listing, as it always does).
    let reply = get(
        &t.app,
        &format!(
            "/rest/deletePlaylist.view?{}&f=json&id=nd-flac",
            auth("alice", "good", "octo")
        ),
    )
    .await;
    assert_eq!(reply.envelope()["status"], "ok", "{}", reply.text());
    assert!(t.nothing_downloads(), "a delete queues no fetch");
    assert!(
        t.navidrome
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .any(|request| request.url.path().ends_with("/rest/deletePlaylist")),
        "the delete itself is Navidrome's to do"
    );
}

/// A FLAC-named playlist that is not the caller's own never reaches the fetch queue (O2): the
/// fetch is a download into the shared library, so the branch is the playlist owner's alone.
/// Someone else's FLAC-named playlist (public playlists can be read by any account) and a row
/// that names no owner at all are ordinary edits — relayed to Navidrome as they came, nothing
/// fetched, nothing emptied.
#[tokio::test]
async fn someone_elses_flac_playlist_is_an_ordinary_edit() {
    let t = PersonalApp::new().await;
    let outside = t.outside_song("Lost in Sound");
    for row_owner in ["bob", ""] {
        let owner_field = match row_owner {
            "" => String::new(),
            owner => format!(r#""owner":"{owner}","#),
        };
        Mock::given(path("/rest/getPlaylist"))
            .respond_with(navidrome_json(&format!(
                r#"{{"subsonic-response":{{"status":"ok","playlist":{{"id":"nd-flac",{owner_field}"name":"Request a copy in FLAC","entry":[{{"id":"{outside}"}}]}}}}}}"#
            )))
            .with_priority(1)
            .mount(&t.navidrome)
            .await;

        let reply = get(
            &t.app,
            &format!(
                "/rest/updatePlaylist.view?{}&f=json&playlistId=nd-flac&songIdToAdd={outside}",
                auth("alice", "good", "octo")
            ),
        )
        .await;
        assert_eq!(
            reply.envelope()["status"],
            "ok",
            "owner '{row_owner}': {}",
            reply.text()
        );
        assert!(
            t.nothing_downloads(),
            "owner '{row_owner}': no fetch for a playlist that is not the caller's own"
        );
        // The client's own add was relayed for Navidrome to do — the intercepted branch would
        // have answered without a relay and emptied the playlist instead.
        assert!(
            t.navidrome
                .received_requests()
                .await
                .unwrap_or_default()
                .iter()
                .any(|request| {
                    request.url.path().ends_with("/rest/updatePlaylist")
                        && request.url.query_pairs().any(|(key, _)| key == "songIdToAdd")
                }),
            "owner '{row_owner}': the add is Navidrome's to do"
        );
    }
}

// ---- The reconciler, over the container's own parts (R-4, both ways) ---------------------

/// How many mirror writes (a DELETE or a POST of positions) the playlist's own tracks path has
/// seen: what an idempotent pass leaves untouched.
async fn mirror_writes(t: &PersonalApp, playlist_id: &str) -> usize {
    let path = format!("/api/playlist/{playlist_id}/tracks");
    t.navidrome
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|request| {
            request.url.path() == path
                && (request.method.as_str() == "DELETE" || request.method.as_str() == "POST")
        })
        .count()
}

/// The reconciler's pass, over the container's own parts (the store, the imports' matcher with a
/// library behind it, the admin playlist API): a position whose song landed in the library gets
/// its `library_id`; one whose file went loses it and falls back to the stream — neither loses
/// the outside half it keeps for that fallback, and the mirror carries local ids only (R-4, both
/// ways in one pass).
#[tokio::test]
async fn the_reconciler_binds_a_landed_file_and_releases_a_gone_one() {
    let t = PersonalApp::new_with_a_plain_playlist().await;
    let landed = t.outside_song("Lost in Sound");
    let gone = t.outside_song("Gone Song");
    t.create_mixed(&landed).await;
    let mut held = t.held(NEW_PLAYLIST);
    held.entries = vec![
        PersonalPlaylistEntry {
            occurrence_id: "occ-landed".into(),
            library_id: None,
            outside_id: Some(landed.clone()),
            you_tube_id: Some("video-1".into()),
            song_snapshot: Some(snapshot_song("K3KHA", "Lost in Sound", 619)),
            seed: None,
        },
        PersonalPlaylistEntry {
            occurrence_id: "occ-gone".into(),
            library_id: Some("nd-gone".into()),
            outside_id: Some(gone.clone()),
            you_tube_id: Some("video-2".into()),
            song_snapshot: Some(snapshot_song("Gone Artist", "Gone Song", 111)),
            seed: None,
        },
    ];
    t.state.personal_playlists.upsert(held);

    // The library: the landed song is there now, the gone one is not.
    let pages = Arc::new(parking_lot::Mutex::new(vec![row(
        "nd-landed",
        "K3KHA",
        "Lost in Sound",
        Some(619),
        Some("Peak Hours"),
        "flac",
        900,
    )]));
    t.state
        .imports
        .matcher()
        .set_read_page(Arc::new(move |start, count| {
            let page: Vec<_> = pages
                .lock()
                .iter()
                .skip(start as usize)
                .take(count as usize)
                .cloned()
                .collect();
            let n = page.len();
            Box::pin(async move { Some((page, n)) })
        }));
    // The playlist's mirror as Navidrome holds it: the one local row the create mirrored.
    let tracks_path = format!("/api/playlist/{NEW_PLAYLIST}/tracks");
    Mock::given(method("GET"))
        .and(path(&tracks_path))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            { "id": "1", "mediaFileId": "nd-2" }
        ])))
        .with_priority(1)
        .mount(&t.navidrome)
        .await;
    for verb in ["DELETE", "POST"] {
        Mock::given(method(verb))
            .and(path(&tracks_path))
            .respond_with(ResponseTemplate::new(200))
            .with_priority(1)
            .mount(&t.navidrome)
            .await;
    }

    let reconciler = PersonalPlaylistReconciler::new(
        t.state.personal_playlists.clone(),
        t.state.imports.matcher().clone(),
        t.state.navidrome_playlist_api.clone(),
    );
    let changed = reconciler.reconcile_once().await;
    assert!(changed, "the pass moved both positions");

    let held = t.held(NEW_PLAYLIST);
    assert_eq!(
        held.entries[0].library_id.as_deref(),
        Some("nd-landed"),
        "the landed file is bound"
    );
    assert_eq!(held.entries[0].outside_id.as_deref(), Some(landed.as_str()));
    assert!(held.entries[0].song_snapshot.is_some());
    assert_eq!(
        held.entries[1].library_id, None,
        "the gone file's binding is released"
    );
    assert_eq!(held.entries[1].outside_id.as_deref(), Some(gone.as_str()));
    assert!(held.entries[1].song_snapshot.is_some());

    // The mirror carries the local half only: the stale row is dropped, the landed one added,
    // and never a registry id.
    let requests = t.navidrome.received_requests().await.unwrap_or_default();
    assert!(
        requests.iter().any(|request| {
            request.url.path() == tracks_path
                && request.method.as_str() == "DELETE"
                && request.url.query_pairs().any(|(key, _)| key == "id")
        }),
        "the stale mirror row is dropped"
    );
    let added: Vec<String> = requests
        .into_iter()
        .filter(|request| request.url.path() == tracks_path && request.method.as_str() == "POST")
        .map(|request| String::from_utf8_lossy(&request.body).to_string())
        .collect();
    assert_eq!(
        added,
        [r#"{"ids":["nd-landed"]}"#.to_string()],
        "the mirror adds the landed local id, and nothing else"
    );
    assert!(!added[0].contains(&landed), "never a registry id in the mirror");
}

/// The pass is idempotent: a playlist the last pass already settled moves nothing, writes
/// nothing and mirrors nothing.
#[tokio::test]
async fn a_reconciler_pass_that_moved_nothing_writes_nothing() {
    let t = PersonalApp::new_with_a_plain_playlist().await;
    let landed = t.outside_song("Lost in Sound");
    t.create_mixed(&landed).await;
    let mut held = t.held(NEW_PLAYLIST);
    held.entries = vec![PersonalPlaylistEntry {
        occurrence_id: "occ-landed".into(),
        library_id: None,
        outside_id: Some(landed.clone()),
        song_snapshot: Some(snapshot_song("K3KHA", "Lost in Sound", 619)),
        ..Default::default()
    }];
    t.state.personal_playlists.upsert(held);

    let pages = Arc::new(parking_lot::Mutex::new(vec![row(
        "nd-landed",
        "K3KHA",
        "Lost in Sound",
        Some(619),
        Some("Peak Hours"),
        "flac",
        900,
    )]));
    t.state
        .imports
        .matcher()
        .set_read_page(Arc::new(move |start, count| {
            let page: Vec<_> = pages
                .lock()
                .iter()
                .skip(start as usize)
                .take(count as usize)
                .cloned()
                .collect();
            let n = page.len();
            Box::pin(async move { Some((page, n)) })
        }));
    let tracks_path = format!("/api/playlist/{NEW_PLAYLIST}/tracks");
    Mock::given(method("GET"))
        .and(path(&tracks_path))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            { "id": "1", "mediaFileId": "nd-landed" }
        ])))
        .with_priority(1)
        .mount(&t.navidrome)
        .await;

    let reconciler = PersonalPlaylistReconciler::new(
        t.state.personal_playlists.clone(),
        t.state.imports.matcher().clone(),
        t.state.navidrome_playlist_api.clone(),
    );
    assert!(reconciler.reconcile_once().await, "the first pass binds the file");

    let writes = mirror_writes(&t, NEW_PLAYLIST).await;
    let halves_of = |t: &PersonalApp| {
        t.held(NEW_PLAYLIST)
            .entries
            .iter()
            .map(|entry| {
                (
                    entry.occurrence_id.clone(),
                    entry.library_id.clone(),
                    entry.outside_id.clone(),
                )
            })
            .collect::<Vec<_>>()
    };
    let settled = halves_of(&t);
    let again = reconciler.reconcile_once().await;
    assert!(!again, "an unchanged playlist moves nothing");
    assert_eq!(
        mirror_writes(&t, NEW_PLAYLIST).await,
        writes,
        "and is neither written nor mirrored again"
    );
    assert_eq!(halves_of(&t), settled, "the store keeps exactly what it held");
}

// ---- getSong's snapshot overlay (D6, the W1 read side) ------------------------------------

/// `getSong` of a position's outside id answers from the snapshot the position kept: the fields
/// the routing never had come from the store — and the routing's own words are never overwritten
/// by the snapshot's (the overlay fills, it does not replace).
#[tokio::test]
async fn get_song_answers_the_snapshot_a_position_kept_without_overwriting_the_routing() {
    let t = PersonalApp::new().await;
    let thin = t.state.external_id_registry.register(SoulseekRouting {
        kind: RoutingKind::Song,
        artist: Some("K3KHA".into()),
        title: Some("Thin Routing".into()),
        ..Default::default()
    });
    let wordy = t.state.external_id_registry.register(SoulseekRouting {
        kind: RoutingKind::Song,
        artist: Some("K3KHA".into()),
        title: Some("Wordy Routing".into()),
        album: Some("Routing Album".into()),
        duration: Some(619),
        ..Default::default()
    });
    t.state.personal_playlists.upsert(PersonalPlaylist {
        id: "nd-1".to_string(),
        entries: vec![
            PersonalPlaylistEntry {
                occurrence_id: "occ-thin".into(),
                outside_id: Some(thin.clone()),
                song_snapshot: Some(Song {
                    artist: "K3KHA".into(),
                    title: "Thin Routing".into(),
                    album: "Snapshot Album".into(),
                    duration: Some(619),
                    isrc: Some("GB-ABC-25-00001".into()),
                    year: Some(2025),
                    genre: Some("Techno".into()),
                    music_brainz_recording_id: Some("mbid-1".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            PersonalPlaylistEntry {
                occurrence_id: "occ-wordy".into(),
                outside_id: Some(wordy.clone()),
                song_snapshot: Some(Song {
                    artist: "K3KHA".into(),
                    title: "Wordy Routing".into(),
                    album: "Snapshot Album".into(),
                    duration: Some(700),
                    ..Default::default()
                }),
                ..Default::default()
            },
        ],
        ..Default::default()
    });

    let filled = t
        .state
        .music_metadata
        .get_song("soulseek", &thin)
        .await
        .expect("the thin routing answers");
    assert_eq!(
        filled.album, "Snapshot Album",
        "the snapshot's album fills the empty one"
    );
    assert_eq!(filled.isrc.as_deref(), Some("GB-ABC-25-00001"));
    assert_eq!(filled.music_brainz_recording_id.as_deref(), Some("mbid-1"));
    assert_eq!(filled.year, Some(2025));
    assert_eq!(filled.genre.as_deref(), Some("Techno"));
    assert_eq!(
        filled.duration,
        Some(619),
        "the snapshot's length fills the routing's missing one"
    );

    let kept = t
        .state
        .music_metadata
        .get_song("soulseek", &wordy)
        .await
        .expect("the wordy routing answers");
    assert_eq!(
        kept.album, "Routing Album",
        "the routing's own words are never overwritten"
    );
    assert_eq!(kept.duration, Some(619));

    // And through the wire: getSong carries the snapshot's words to a client.
    let reply = get(
        &t.app,
        &format!(
            "/rest/getSong.view?{}&f=json&id={thin}",
            auth("alice", "good", "octo")
        ),
    )
    .await;
    assert_eq!(reply.envelope()["status"], "ok", "{}", reply.text());
    assert_eq!(reply.envelope()["song"]["album"], "Snapshot Album");
    assert_eq!(reply.envelope()["song"]["year"], 2025);
}

// ---- The lists' counts (Defekt 2's fix, both protocols) ------------------------------------

/// The native playlist list carries the overlay's counts: a held playlist's row answers the
/// mixed positions it holds, not the local-only mirror's — and a plain playlist's row keeps
/// Navidrome's own counts.
#[tokio::test]
async fn the_native_playlist_list_carries_the_overlays_counts() {
    let t = PersonalApp::new().await;
    let outside = t.outside_song("Lost in Sound");
    t.create_mixed(&outside).await;

    Mock::given(method("GET"))
        .and(path("/api/playlist"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            { "id": NEW_PLAYLIST, "name": "Mix", "songCount": 1, "duration": 200 },
            { "id": PLAIN_PLAYLIST, "name": "Plain List", "songCount": 3, "duration": 300 }
        ])))
        .with_priority(1)
        .mount(&t.navidrome)
        .await;

    let reply = get(
        &t.app,
        &format!("/api/playlist?{}", auth("alice", "good", "octo")),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    let rows = reply.json().as_array().cloned().expect("a rows array");
    let row_of = |id: &str| {
        rows.iter()
            .find(|row| row["id"] == id)
            .cloned()
            .unwrap_or_else(|| panic!("no row for {id}: {}", reply.text()))
    };
    assert_eq!(row_of(NEW_PLAYLIST)["songCount"], 2, "{}", reply.text());
    assert_eq!(row_of(NEW_PLAYLIST)["duration"], 619);
    assert_eq!(
        row_of(PLAIN_PLAYLIST)["songCount"],
        3,
        "a plain playlist keeps Navidrome's own counts"
    );
    assert_eq!(reply.header("x-total-count"), Some("2"));
}

/// The REST `getPlaylists` answer carries the overlay's counts too — the REST twin of the native
/// list's correction.
#[tokio::test]
async fn the_rest_playlists_list_carries_the_overlays_counts() {
    let t = PersonalApp::new().await;
    let outside = t.outside_song("Lost in Sound");
    t.create_mixed(&outside).await;

    Mock::given(path("/rest/getPlaylists"))
        .respond_with(navidrome_json(&format!(
            r#"{{"subsonic-response":{{"status":"ok","playlists":{{"playlist":[{{"id":"{NEW_PLAYLIST}","name":"Mix","songCount":1,"duration":200}},{{"id":"{PLAIN_PLAYLIST}","name":"Plain List","songCount":3,"duration":300}}]}}}}}}"#
        )))
        .with_priority(1)
        .mount(&t.navidrome)
        .await;

    let reply = get(
        &t.app,
        &format!("/rest/getPlaylists.view?{}&f=json", auth("alice", "good", "octo")),
    )
    .await;
    assert_eq!(reply.envelope()["status"], "ok", "{}", reply.text());
    let rows = reply.envelope()["playlists"]["playlist"]
        .as_array()
        .cloned()
        .expect("playlist rows");
    let row_of = |id: &str| {
        rows.iter()
            .find(|row| row["id"] == id)
            .cloned()
            .unwrap_or_else(|| panic!("no row for {id}: {}", reply.text()))
    };
    assert_eq!(row_of(NEW_PLAYLIST)["songCount"], 2, "{}", reply.text());
    assert_eq!(row_of(NEW_PLAYLIST)["duration"], 619);
    assert_eq!(
        row_of(PLAIN_PLAYLIST)["songCount"],
        3,
        "a plain playlist keeps Navidrome's own counts"
    );
}

// ---- The REST door of the adoption gate ----------------------------------------------------

/// The REST door of the same gate as the native PUT's: an update on a plain playlist the
/// overlay does not hold, with purely local song ids, is an ordinary edit — the relay takes it
/// to Navidrome as it came, the overlay's mirror never runs, and the store never learns the
/// playlist.
#[tokio::test]
async fn a_purely_local_rest_update_on_a_plain_playlist_stays_navidromes_own() {
    let t = PersonalApp::new().await;

    let reply = get(
        &t.app,
        &format!(
            "/rest/updatePlaylist.view?{}&f=json&playlistId={PLAIN_PLAYLIST}&name=Plain%20List&songIdToAdd=nd-9",
            auth("alice", "good", "octo")
        ),
    )
    .await;

    assert_eq!(reply.envelope()["status"], "ok", "{}", reply.text());
    assert!(
        t.navidrome
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .any(|request| {
                request.url.path().ends_with("/rest/updatePlaylist")
                    && request
                        .url
                        .query_pairs()
                        .any(|(key, value)| key == "playlistId" && value == PLAIN_PLAYLIST)
            }),
        "the edit is Navidrome's to do"
    );
    assert!(
        !t.navidrome
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .any(|request| {
                request.url.path().ends_with("/rest/createPlaylist")
                    && request
                        .url
                        .query_pairs()
                        .any(|(key, value)| key == "playlistId" && value == PLAIN_PLAYLIST)
            }),
        "the overlay's mirror never ran"
    );
    assert!(!t.state.personal_playlists.knows(PLAIN_PLAYLIST));
    assert!(t.nothing_downloads());
}
