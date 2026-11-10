//! The owner's streaming favorites: with every download source for a heart off, a star from a
//! caller Octo does not manage saves the song or album instead of fetching it, and the save is
//! visible everywhere favorites are read — the starred lists, the native song detail, Liked
//! Songs — and leaves again by unstar. A managed member keeps the gate's outcome.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Request, State};
use chrono::Utc;
use http_body_util::BodyExt;
use indexmap::IndexMap;
use octo_core::family::abilities::{Abilities, Preset};
use octo_core::models::domain::{Album, Song};
use octo_core::settings::AppSettings;
use octo_core::soulseek::soulseek_metadata_service::{RoutingKind, SoulseekRouting};
use octo_subsonic::SubsonicCredential;
use parking_lot::Mutex;
use serde_json::Value;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, Request as MockRequest, Respond, ResponseTemplate};

use super::{star, starred_time, unstar};
use crate::app::{AppState, TestServices};
use crate::controllers::subsonic::test_app::*;
use crate::http::pipeline::App;
use crate::services::common::test_fakes::FakeMetadata;
use crate::services::family::test_support::member;
use crate::services::family::{FamilyCaller, SavedSong};

/// The same in XML.
const STARRED2_XML: &str = r#"<subsonic-response xmlns="http://subsonic.org/restapi" status="ok" version="1.16.1" type="navidrome"><starred2><song id="nd-7" title="Library song" starred="2026-01-01T00:00:00Z"></song></starred2></subsonic-response>"#;

/// getStarred, the older list, with nothing starred.
const STARRED_EMPTY_JSON: &str =
    r#"{"subsonic-response":{"status":"ok","version":"1.16.1","type":"navidrome","starred":{}}}"#;

/// Navidrome's refusal of a star, as the `refused_star_id` knob answers it.
const NAVIDROME_REFUSED_STAR: &str = r#"{"subsonic-response":{"status":"failed","version":"1.16.1","error":{"code":70,"message":"data not found"}}}"#;

/// Navidrome for these tests: the token "good" signs in, the starred lists answer as above,
/// and everything else (a star's library part, a ping, an unstar) is a plain ok. The knobs:
/// `double_songs`/`double_albums` are ids whose rows Navidrome's starred answer already carries
/// (the caller saved them too — a row must not be doubled), and `refused_star_id` is a library
/// id Navidrome refuses to star.
#[derive(Default)]
struct SavedNavidrome {
    double_songs: Arc<Mutex<Vec<String>>>,
    double_albums: Arc<Mutex<Vec<String>>>,
    refused_star_id: Option<String>,
}

impl Respond for SavedNavidrome {
    fn respond(&self, request: &MockRequest) -> ResponseTemplate {
        let path = request.url.path();
        let query = |key: &str| {
            request
                .url
                .query_pairs()
                .filter(|(k, _)| k == key)
                .map(|(_, v)| v.into_owned())
                .collect::<Vec<String>>()
        };
        if !query("t").iter().any(|t| t == "good") {
            return navidrome_json(NAVIDROME_WRONG_PASSWORD);
        }
        if (path.ends_with("/rest/star") || path.ends_with("/rest/star.view"))
            && let Some(refused) = &self.refused_star_id
            && query("id").iter().any(|id| id == refused)
        {
            return navidrome_json(NAVIDROME_REFUSED_STAR);
        }
        if path.ends_with("/rest/getStarred2") || path.ends_with("/rest/getStarred2.view") {
            return if request.url.query_pairs().any(|(k, v)| k == "f" && v == "json") {
                let mut songs = vec![
                    r#"{"id":"nd-7","title":"Library song","starred":"2026-01-01T00:00:00Z"}"#.to_string(),
                ];
                let mut albums: Vec<String> = Vec::new();
                for id in self.double_songs.lock().iter() {
                    songs.push(format!(
                        r#"{{"id":"{id}","title":"Doubled song","starred":"2025-06-01T00:00:00Z"}}"#
                    ));
                }
                for id in self.double_albums.lock().iter() {
                    albums.push(format!(
                        r#"{{"id":"{id}","name":"Doubled album","starred":"2025-06-01T00:00:00Z"}}"#
                    ));
                }
                navidrome_json(&format!(
                    r#"{{"subsonic-response":{{"status":"ok","version":"1.16.1","type":"navidrome","starred2":{{"song":[{}],"album":[{}]}}}}}}"#,
                    songs.join(","),
                    albums.join(",")
                ))
            } else {
                ResponseTemplate::new(200).set_body_raw(STARRED2_XML.as_bytes().to_vec(), "application/xml")
            };
        }
        if path.ends_with("/rest/getStarred") || path.ends_with("/rest/getStarred.view") {
            return navidrome_json(STARRED_EMPTY_JSON);
        }
        navidrome_json(NAVIDROME_OK)
    }
}

/// An app with the heart's download sources off (DownloadOnStar and DownloadAlbumOnStar), a
/// registry song and album, and a metadata service that knows both. `family_on` adds members,
/// for the regression a managed caller pays.
struct SavedApp {
    state: AppState,
    app: App,
    navidrome: MockServer,
    song: String,
    other: String,
    album: String,
    /// Ids whose song rows Navidrome's starred answers already carry, set per test.
    double_songs: Arc<Mutex<Vec<String>>>,
    /// Ids whose album rows Navidrome's starred answers already carry, set per test.
    double_albums: Arc<Mutex<Vec<String>>>,
    _members: tempfile::TempDir,
}

impl SavedApp {
    async fn new(family_on: bool) -> SavedApp {
        SavedApp::with(family_on, false, false, None).await
    }

    /// An app with chosen heart download sources (the save paths need both off) and an optional
    /// library id Navidrome refuses to star.
    async fn with(
        family_on: bool,
        song_source: bool,
        album_source: bool,
        refused_star_id: Option<String>,
    ) -> SavedApp {
        let navidrome = MockServer::builder().start().await;
        let mock = SavedNavidrome {
            double_songs: Arc::new(Mutex::new(Vec::new())),
            double_albums: Arc::new(Mutex::new(Vec::new())),
            refused_star_id,
        };
        let double_songs = mock.double_songs.clone();
        let double_albums = mock.double_albums.clone();
        Mock::given(any()).respond_with(mock).mount(&navidrome).await;
        let members = tempfile::tempdir().expect("a temp dir");
        let mut settings = AppSettings {
            family: octo_core::settings::FamilySettings {
                enabled: family_on,
                members_path: members.path().to_string_lossy().into_owned(),
                ..Default::default()
            },
            ..settings(&navidrome.uri())
        };
        settings.subsonic.download_on_star = song_source;
        settings.subsonic.download_album_on_star = album_source;
        // A waiting heart is listed only when it becomes a favorite on landing.
        settings.subsonic.star_downloads_for_requester = true;
        // A source that can queue must not fetch anywhere real in a test.
        settings.soulseek.base_url = Some("http://127.0.0.1:1".into());
        let metadata = Arc::new(FakeMetadata::default());
        let state = AppState::for_tests_with(
            settings,
            TestServices {
                metadata: Some(metadata.clone()),
                ..Default::default()
            },
        );
        let song = state.external_id_registry.register(SoulseekRouting {
            kind: RoutingKind::Song,
            artist: Some("Massive Attack".into()),
            title: Some("Teardrop".into()),
            album: Some("Mezzanine".into()),
            duration: Some(330),
            ..Default::default()
        });
        let other = state.external_id_registry.register(SoulseekRouting {
            kind: RoutingKind::Song,
            artist: Some("Massive Attack".into()),
            title: Some("Angel".into()),
            album: Some("Mezzanine".into()),
            duration: Some(372),
            ..Default::default()
        });
        let album = state.external_id_registry.register(SoulseekRouting {
            kind: RoutingKind::Album,
            artist: Some("Massive Attack".into()),
            album: Some("Mezzanine".into()),
            ..Default::default()
        });
        metadata.songs.lock().insert(
            ("soulseek".into(), song.clone()),
            Song {
                id: song.clone(),
                title: "Teardrop".into(),
                artist: "Massive Attack".into(),
                album: "Mezzanine".into(),
                duration: Some(330),
                ..Default::default()
            },
        );
        metadata.songs.lock().insert(
            ("soulseek".into(), other.clone()),
            Song {
                id: other.clone(),
                title: "Angel".into(),
                artist: "Massive Attack".into(),
                album: "Mezzanine".into(),
                duration: Some(372),
                ..Default::default()
            },
        );
        metadata.albums.lock().insert(
            ("soulseek".into(), album.clone()),
            Album {
                id: album.clone(),
                title: "Mezzanine".into(),
                artist: "Massive Attack".into(),
                year: Some(1998),
                songs: vec![Song {
                    title: "Teardrop".into(),
                    artist: "Massive Attack".into(),
                    ..Default::default()
                }],
                ..Default::default()
            },
        );
        state.family_store.set_owner("winters");
        state
            .family_store
            .upsert_member(member("alex", Preset::Member, 3));
        SavedApp {
            app: app(&state),
            state,
            navidrome,
            song,
            other,
            album,
            double_songs,
            double_albums,
            _members: members,
        }
    }

    /// Every id value of every request Navidrome received at `endpoint`, one list per request.
    async fn ids_sent_to(&self, endpoint: &str) -> Vec<Vec<String>> {
        self.navidrome
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter(|r| {
                let path = r.url.path();
                path.ends_with(&format!("/rest/{endpoint}"))
                    || path.ends_with(&format!("/rest/{endpoint}.view"))
            })
            .map(|r| {
                r.url
                    .query_pairs()
                    .filter(|(k, _)| matches!(k.as_ref(), "id" | "albumId" | "artistId"))
                    .map(|(_, v)| v.into_owned())
                    .collect()
            })
            .collect()
    }

    /// Nothing waits anywhere: no download queued, none held, none tracked.
    fn nothing_fetching(&self) {
        assert!(self.state.track_acquisition_queue.is_idle(), "no download queued");
        assert!(self.state.acquisition_tracker.all().is_empty(), "nothing tracked");
        assert_eq!(self.state.star_on_arrival.held(), 0, "nothing held");
    }
}

fn owner() -> String {
    auth("winters", "good", "Symfonium")
}

/// A star with every download source off, from the owner's own account, saves the outside song
/// as a streaming favorite: nothing is fetched, and Navidrome never sees the registry id.
#[tokio::test]
async fn an_owners_star_with_downloads_off_saves_the_song() {
    let t = SavedApp::new(false).await;
    let star = get(
        &t.app,
        &format!("/rest/star.view?{}&f=json&id={}", owner(), t.song),
    )
    .await;
    assert_eq!(star.envelope()["status"], "ok", "{}", star.text());

    let saved = t.state.family_saved.list("winters");
    assert_eq!(saved.len(), 1, "{saved:?}");
    assert_eq!(saved[0].id, t.song);
    assert_eq!(saved[0].title, "Teardrop");
    assert_eq!(saved[0].artist, "Massive Attack");
    assert_eq!(saved[0].duration, 330);
    t.nothing_fetching();
    assert!(
        t.ids_sent_to("star").await.is_empty(),
        "the registry id never reached Navidrome"
    );
    // A star of the same song again is the same save, not a second one.
    let again = get(
        &t.app,
        &format!("/rest/star.view?{}&f=json&id={}", owner(), t.song),
    )
    .await;
    assert_eq!(again.envelope()["status"], "ok", "{}", again.text());
    assert_eq!(t.state.family_saved.list("winters").len(), 1);
}

/// A star of an outside album with album downloads off saves the album as a link.
#[tokio::test]
async fn an_owners_album_star_with_album_downloads_off_saves_the_album() {
    let t = SavedApp::new(false).await;
    let star = get(
        &t.app,
        &format!("/rest/star.view?{}&f=json&albumId={}", owner(), t.album),
    )
    .await;
    assert_eq!(star.envelope()["status"], "ok", "{}", star.text());

    let albums = t.state.family_saved.albums("winters");
    assert_eq!(albums.len(), 1, "{albums:?}");
    assert_eq!(albums[0].id, t.album);
    assert_eq!(albums[0].name, "Mezzanine");
    assert_eq!(albums[0].song_count, 1);
    t.nothing_fetching();
    assert!(t.ids_sent_to("star").await.is_empty());
}

/// The owner's saved song and album join their starred lists, starred when they were saved,
/// beside Navidrome's own hearts; the older getStarred carries them too.
#[tokio::test]
async fn saved_songs_and_albums_join_the_owners_starred_lists() {
    let t = SavedApp::new(false).await;
    for query in [
        format!("id={}", t.song),
        format!("albumId={}", t.album),
        format!("id=nd-1&id={}", t.song),
    ] {
        let star = get(&t.app, &format!("/rest/star.view?{}&f=json&{query}", owner())).await;
        assert_eq!(star.envelope()["status"], "ok", "{}", star.text());
    }
    let saved = t.state.family_saved.list("winters");
    let saved_album = t.state.family_saved.albums("winters");

    let reply = get(&t.app, &format!("/rest/getStarred2.view?{}&f=json", owner())).await;
    let starred = reply.envelope()["starred2"].clone();
    let songs = starred["song"].as_array().expect("songs");
    assert_eq!(songs.len(), 2, "{songs:?}");
    assert_eq!(songs[0]["id"], "nd-7");
    assert_eq!(songs[1]["id"], t.song.as_str());
    assert_eq!(songs[1]["title"], "Teardrop");
    assert_eq!(songs[1]["starred"], starred_time(&saved[0].saved).as_str());
    let albums = starred["album"].as_array().expect("albums");
    assert_eq!(albums.len(), 1, "{albums:?}");
    assert_eq!(albums[0]["id"], t.album.as_str());
    assert_eq!(albums[0]["name"], "Mezzanine");
    assert_eq!(albums[0]["starred"], starred_time(&saved_album[0].saved).as_str());

    let older = get(&t.app, &format!("/rest/getStarred.view?{}&f=json", owner())).await;
    let starred = older.envelope()["starred"].clone();
    assert_eq!(starred["song"][0]["id"], t.song.as_str());
    assert_eq!(starred["album"][0]["id"], t.album.as_str());
}

/// A song that is both saved as a streaming favorite and still waiting for its download holds
/// one row in the starred lists — the save's row, starred at the save moment — in JSON and in
/// XML alike.
#[tokio::test]
async fn a_song_both_saved_and_waiting_is_listed_once() {
    let t = SavedApp::new(false).await;
    // The save: a star with every download source off.
    get(
        &t.app,
        &format!("/rest/star.view?{}&f=json&id={}", owner(), t.song),
    )
    .await;
    let saved = t.state.family_saved.list("winters");
    assert_eq!(saved.len(), 1, "{saved:?}");
    // The waiting heart: a star from before the sources went off, its download still pending.
    let (_, provider, external_id) = t.state.local_library.parse_song_id(&t.song);
    let mut sign_in = IndexMap::new();
    sign_in.insert("u".to_string(), "winters".to_string());
    sign_in.insert("t".to_string(), "good".to_string());
    sign_in.insert("s".to_string(), "salt".to_string());
    let credential = SubsonicCredential::from(&sign_in).expect("the owner's sign-in");
    t.state.star_on_arrival.hold_starred_song(
        provider.as_deref().unwrap_or_default(),
        external_id.as_deref().unwrap_or_default(),
        &t.song,
        &credential,
        Some("winters"),
    );

    let reply = get(&t.app, &format!("/rest/getStarred2.view?{}&f=json", owner())).await;
    let starred = reply.envelope()["starred2"].clone();
    let songs = starred["song"].as_array().expect("songs");
    assert_eq!(songs.len(), 2, "{}", reply.text());
    let rows: Vec<&Value> = songs.iter().filter(|row| row["id"] == t.song.as_str()).collect();
    assert_eq!(rows.len(), 1, "one row per id: {songs:?}");
    assert_eq!(rows[0]["starred"], starred_time(&saved[0].saved).as_str());

    let xml = get(&t.app, &format!("/rest/getStarred2.view?{}&f=xml", owner())).await;
    let text = xml.text();
    assert_eq!(
        text.matches(&format!("id=\"{}\"", t.song)).count(),
        1,
        "one row per id: {text}"
    );
    assert!(
        text.contains(&format!("starred=\"{}\"", starred_time(&saved[0].saved))),
        "the save's row, at the save moment: {text}"
    );
}

/// An unstar takes the owner's save back: the id is answered by Octo and relayed nowhere.
#[tokio::test]
async fn an_unstar_removes_the_owners_save() {
    let t = SavedApp::new(false).await;
    get(
        &t.app,
        &format!("/rest/star.view?{}&f=json&id={}", owner(), t.song),
    )
    .await;
    assert_eq!(t.state.family_saved.list("winters").len(), 1);

    let unstar = get(
        &t.app,
        &format!("/rest/unstar.view?{}&f=json&id={}", owner(), t.song),
    )
    .await;
    assert_eq!(unstar.envelope()["status"], "ok", "{}", unstar.text());
    assert!(t.state.family_saved.list("winters").is_empty());
    assert!(t.ids_sent_to("unstar").await.is_empty());
    t.nothing_fetching();

    // What is gone stays gone from the starred lists.
    let reply = get(&t.app, &format!("/rest/getStarred2.view?{}&f=json", owner())).await;
    let starred = reply.envelope()["starred2"].clone();
    let songs = starred["song"].as_array().expect("songs");
    assert_eq!(songs.len(), 1, "{songs:?}");
    assert_eq!(songs[0]["id"], "nd-7");
}

/// A managed member's unstar of an outside id is answered by Octo whether a save of it stands
/// or not — also for a repeated unstar, with the save already gone — and the registry id is
/// relayed nowhere.
#[tokio::test]
async fn a_managed_members_unstar_is_answered_by_octo_without_a_relay() {
    let t = SavedApp::new(true).await;
    let caller = FamilyCaller::member("alex", Preset::Member, Abilities::preset(Preset::Member));
    let member_unstar = |song: &str| {
        let mut request = Request::builder()
            .uri(format!(
                "/rest/unstar.view?id={song}&u=alex&t=good&s=salt&v=1.16.1&c=test&f=json"
            ))
            .body(Body::empty())
            .expect("a request");
        request.extensions_mut().insert(caller.clone());
        request
    };

    // No save stands: a repeated unstar, or one of an id never saved.
    let response = unstar(State(t.state.clone()), member_unstar(&t.song)).await;
    let body = response.into_body().collect().await.expect("read").to_bytes();
    let answer: Value = serde_json::from_slice(&body).expect("JSON");
    assert_eq!(answer["subsonic-response"]["status"], "ok", "{answer}");
    assert!(
        t.ids_sent_to("unstar").await.is_empty(),
        "the registry id never reached Navidrome"
    );
    t.nothing_fetching();

    // With a save standing: it is taken back by Octo, still without a relay.
    t.state.family_saved.add(
        "alex",
        SavedSong {
            id: t.song.clone(),
            title: "Teardrop".into(),
            artist: "Massive Attack".into(),
            album: "Mezzanine".into(),
            cover_art: t.song.clone(),
            duration: 330,
            explicit: 0,
            saved: Utc::now(),
        },
    );
    let response = unstar(State(t.state.clone()), member_unstar(&t.song)).await;
    let body = response.into_body().collect().await.expect("read").to_bytes();
    let answer: Value = serde_json::from_slice(&body).expect("JSON");
    assert_eq!(answer["subsonic-response"]["status"], "ok", "{answer}");
    assert!(
        t.state.family_saved.list("alex").is_empty(),
        "the save is taken back"
    );
    assert!(
        t.ids_sent_to("unstar").await.is_empty(),
        "the registry id never reached Navidrome"
    );
    t.nothing_fetching();
}

/// The native song detail carries the save moment as Navidrome would (starred and starredAt)
/// for a saved song, and neither field for one nobody saved.
#[tokio::test]
async fn the_native_song_detail_carries_the_save_moment() {
    let t = SavedApp::new(false).await;
    get(
        &t.app,
        &format!("/rest/star.view?{}&f=json&id={}", owner(), t.song),
    )
    .await;
    let saved = t.state.family_saved.list("winters");

    let detail = get(&t.app, &format!("/api/song/{}?{}&f=json", t.song, owner())).await;
    let song = detail.json();
    assert_eq!(song["id"], t.song.as_str(), "{}", detail.text());
    assert_eq!(song["title"], "Teardrop");
    let expected = starred_time(&saved[0].saved);
    assert_eq!(song["starred"], expected.as_str(), "{}", detail.text());
    assert_eq!(song["starredAt"], expected.as_str());

    let untouched = get(&t.app, &format!("/api/song/{}?{}&f=json", t.other, owner())).await;
    let song = untouched.json();
    assert_eq!(song["id"], t.other.as_str(), "{}", untouched.text());
    assert!(song.get("starred").is_none(), "no save, no star: {song}");
    assert!(song.get("starredAt").is_none(), "no save, no star: {song}");
}

/// Liked Songs carries the saved song beside Navidrome's hearts, with its save moment.
#[tokio::test]
async fn liked_songs_carries_the_saved_song() {
    let t = SavedApp::new(false).await;
    get(
        &t.app,
        &format!("/rest/star.view?{}&f=json&id={}", owner(), t.song),
    )
    .await;
    let saved = t.state.family_saved.list("winters");
    let liked = t.state.generated_playlists.liked_id("winters");

    let reply = get(
        &t.app,
        &format!("/rest/getPlaylist.view?{}&f=json&id={liked}", owner()),
    )
    .await;
    let playlist = reply.envelope()["playlist"].clone();
    let entries = playlist["entry"].as_array().expect("entries");
    assert_eq!(entries.len(), 2, "{}", reply.text());
    let ids: Vec<&str> = entries.iter().map(|e| e["id"].as_str().unwrap_or("")).collect();
    assert!(ids.contains(&t.song.as_str()), "{ids:?}");
    let saved_row = entries
        .iter()
        .find(|e| e["id"] == t.song.as_str())
        .expect("the saved song's row");
    assert_eq!(saved_row["starred"], starred_time(&saved[0].saved).as_str());
    assert_eq!(saved_row["title"], "Teardrop");
}

/// A managed member keeps the gate's outcome with the sources off: their star of an outside
/// song still goes to Navidrome as it always did (and errors there), saving nothing, and their
/// album star is ignored, not saved.
#[tokio::test]
async fn a_managed_members_star_with_downloads_off_goes_where_it_always_went() {
    let t = SavedApp::new(true).await;
    let caller = FamilyCaller::member("alex", Preset::Member, Abilities::preset(Preset::Member));
    let mut request = Request::builder()
        .uri(format!(
            "/rest/star.view?id={}&u=alex&t=good&s=salt&v=1.16.1&c=test&f=json",
            t.song
        ))
        .body(Body::empty())
        .expect("a request");
    request.extensions_mut().insert(caller.clone());
    let response = star(State(t.state.clone()), request).await.expect("answered");
    let body = response.into_body().collect().await.expect("read").to_bytes();
    let answer: Value = serde_json::from_slice(&body).expect("JSON");
    assert_eq!(answer["subsonic-response"]["status"], "ok", "{answer}");

    assert!(
        t.state.family_saved.list("alex").is_empty(),
        "a member's heart is not a save here"
    );
    assert_eq!(
        t.ids_sent_to("star").await,
        [vec![t.song.clone()]],
        "relayed as before"
    );
    t.nothing_fetching();

    // The album the member stars with album downloads off is ignored, not saved.
    let mut request = Request::builder()
        .uri(format!(
            "/rest/star.view?albumId={}&u=alex&t=good&s=salt&v=1.16.1&c=test&f=json",
            t.album
        ))
        .body(Body::empty())
        .expect("a request");
    request.extensions_mut().insert(caller);
    let response = star(State(t.state.clone()), request).await.expect("answered");
    let body = response.into_body().collect().await.expect("read").to_bytes();
    let answer: Value = serde_json::from_slice(&body).expect("JSON");
    assert_eq!(answer["subsonic-response"]["status"], "ok", "{answer}");
    assert!(t.state.family_saved.albums("alex").is_empty());
    t.nothing_fetching();
}

/// The default heart semantics stand: with the song source on, the owner's star of an outside
/// song queues the download and holds the heart — it saves nothing.
#[tokio::test]
async fn a_star_with_the_song_source_on_queues_the_download_and_saves_nothing() {
    let t = SavedApp::with(false, true, false, None).await;
    let star = get(
        &t.app,
        &format!("/rest/star.view?{}&f=json&id={}", owner(), t.song),
    )
    .await;
    assert_eq!(star.envelope()["status"], "ok", "{}", star.text());

    assert!(
        t.state.family_saved.list("winters").is_empty(),
        "a heart with its source on is not a save"
    );
    let downloads: Vec<String> = t
        .state
        .acquisition_tracker
        .for_user("winters")
        .iter()
        .map(|row| row.id.clone())
        .collect();
    assert!(
        downloads.contains(&t.song),
        "the download is queued: {downloads:?}"
    );
    assert_eq!(t.state.star_on_arrival.held(), 1, "the heart is held");
    assert!(
        t.ids_sent_to("star").await.is_empty(),
        "the registry id never reached Navidrome"
    );
}

/// The sources are read per kind: with songs on but albums off, the owner's album star is a
/// streaming save, and nothing of the album is queued or held.
#[tokio::test]
async fn with_songs_on_and_albums_off_an_album_star_still_saves_the_album() {
    let t = SavedApp::with(false, true, false, None).await;
    let star = get(
        &t.app,
        &format!("/rest/star.view?{}&f=json&albumId={}", owner(), t.album),
    )
    .await;
    assert_eq!(star.envelope()["status"], "ok", "{}", star.text());

    let albums = t.state.family_saved.albums("winters");
    assert_eq!(albums.len(), 1, "{albums:?}");
    assert_eq!(albums[0].id, t.album);
    assert_eq!(albums[0].name, "Mezzanine");
    assert!(
        t.state.acquisition_tracker.all().is_empty(),
        "the album is not queued"
    );
    assert_eq!(t.state.star_on_arrival.held(), 0);
}

/// A star Navidrome refuses (its library part failed) saves none of the outside ids it carried:
/// the refusal is the whole answer, nothing is held, nothing fetched.
#[tokio::test]
async fn a_star_navidrome_refuses_saves_nothing_of_its_outside_ids() {
    let t = SavedApp::with(false, false, false, Some("nd-404".into())).await;
    let star = get(
        &t.app,
        &format!("/rest/star.view?{}&f=json&id=nd-404&id={}", owner(), t.song),
    )
    .await;
    assert_eq!(star.envelope()["status"], "failed", "{}", star.text());
    assert_eq!(star.envelope()["error"]["code"], 70);

    assert!(
        t.state.family_saved.list("winters").is_empty(),
        "a refused star leaves nothing saved"
    );
    t.nothing_fetching();
    assert_eq!(t.ids_sent_to("star").await, [vec!["nd-404".to_string()]]);
}

/// An id the caller saved that Navidrome's own starred answer already lists keeps one row: the
/// list Navidrome sent, without a second row for the save — songs and albums alike.
#[tokio::test]
async fn a_saved_id_navidrome_already_lists_is_not_doubled() {
    let t = SavedApp::new(false).await;
    get(
        &t.app,
        &format!("/rest/star.view?{}&f=json&id={}", owner(), t.song),
    )
    .await;
    get(
        &t.app,
        &format!("/rest/star.view?{}&f=json&albumId={}", owner(), t.album),
    )
    .await;
    t.double_songs.lock().push(t.song.clone());
    t.double_albums.lock().push(t.album.clone());

    let reply = get(&t.app, &format!("/rest/getStarred2.view?{}&f=json", owner())).await;
    let starred = reply.envelope()["starred2"].clone();
    let songs = starred["song"].as_array().expect("songs");
    let song_rows = songs.iter().filter(|row| row["id"] == t.song.as_str()).count();
    assert_eq!(song_rows, 1, "one row per id: {songs:?}");
    assert_eq!(songs.len(), 2, "nd-7 and the saved id once each: {songs:?}");
    let albums = starred["album"].as_array().expect("albums");
    let album_rows = albums.iter().filter(|row| row["id"] == t.album.as_str()).count();
    assert_eq!(album_rows, 1, "one row per id: {albums:?}");
}

/// The saved song and album join the XML starred list too, each with its save moment.
#[tokio::test]
async fn the_saved_song_and_album_join_the_xml_starred_lists() {
    let t = SavedApp::new(false).await;
    get(
        &t.app,
        &format!("/rest/star.view?{}&f=json&id={}", owner(), t.song),
    )
    .await;
    get(
        &t.app,
        &format!("/rest/star.view?{}&f=json&albumId={}", owner(), t.album),
    )
    .await;
    let saved = t.state.family_saved.list("winters");
    let saved_album = t.state.family_saved.albums("winters");

    let xml = get(&t.app, &format!("/rest/getStarred2.view?{}&f=xml", owner())).await;
    let text = xml.text();
    assert_eq!(
        text.matches(&format!("id=\"{}\"", t.song)).count(),
        1,
        "the saved song, once: {text}"
    );
    assert!(
        text.contains(&format!("starred=\"{}\"", starred_time(&saved[0].saved))),
        "starred at the save moment: {text}"
    );
    assert_eq!(
        text.matches(&format!("<album id=\"{}\"", t.album)).count(),
        1,
        "the saved album, once: {text}"
    );
    assert!(
        text.contains(&format!("starred=\"{}\"", starred_time(&saved_album[0].saved))),
        "the album starred at its save moment: {text}"
    );
}

/// An unstar of a save beside a library id takes the save back and relays only the library id:
/// the registry id never reaches Navidrome.
#[tokio::test]
async fn an_unstar_of_a_save_and_a_library_id_relays_only_the_library_id() {
    let t = SavedApp::new(false).await;
    get(
        &t.app,
        &format!("/rest/star.view?{}&f=json&id={}", owner(), t.song),
    )
    .await;
    assert_eq!(t.state.family_saved.list("winters").len(), 1);

    let unstar = get(
        &t.app,
        &format!("/rest/unstar.view?{}&f=json&id=nd-7&id={}", owner(), t.song),
    )
    .await;
    assert_eq!(unstar.envelope()["status"], "ok", "{}", unstar.text());
    assert!(t.state.family_saved.list("winters").is_empty());
    assert_eq!(t.ids_sent_to("unstar").await, [vec!["nd-7".to_string()]]);
}

/// An unstar takes a saved album back the same way: answered by Octo, relayed nowhere.
#[tokio::test]
async fn an_unstar_takes_the_saved_album_back() {
    let t = SavedApp::new(false).await;
    get(
        &t.app,
        &format!("/rest/star.view?{}&f=json&albumId={}", owner(), t.album),
    )
    .await;
    assert_eq!(t.state.family_saved.albums("winters").len(), 1);

    let unstar = get(
        &t.app,
        &format!("/rest/unstar.view?{}&f=json&albumId={}", owner(), t.album),
    )
    .await;
    assert_eq!(unstar.envelope()["status"], "ok", "{}", unstar.text());
    assert!(t.state.family_saved.albums("winters").is_empty());
    assert!(t.ids_sent_to("unstar").await.is_empty());
}

/// The native song detail shows a save of the requesting user only: another listener's save of
/// the same song leaves the starred fields away.
#[tokio::test]
async fn the_native_song_detail_does_not_show_another_users_save() {
    let t = SavedApp::new(false).await;
    get(
        &t.app,
        &format!("/rest/star.view?{}&f=json&id={}", owner(), t.song),
    )
    .await;
    assert_eq!(t.state.family_saved.list("winters").len(), 1);

    let other = get(
        &t.app,
        &format!(
            "/api/song/{}?{}&f=json",
            t.song,
            auth("alex", "good", "Symfonium")
        ),
    )
    .await;
    let song = other.json();
    assert_eq!(song["id"], t.song.as_str(), "{}", other.text());
    assert!(song.get("starred").is_none(), "someone else's save: {song}");
    assert!(song.get("starredAt").is_none(), "someone else's save: {song}");
}
