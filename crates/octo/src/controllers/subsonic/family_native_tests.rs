//! The family playlists in Navidrome's own native API (the "Navidrome" mode Feishin and
//! friends connect in): the list carries the family's rows, the detail and tracks of an `of`
//! id answer instead of a radio-shaped 404, and a create whose name starts with the playlist
//! prefix founds a family playlist through the same core REST does — each witnessed through
//! the whole pipeline with a bearer Navidrome takes.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, StatusCode};
use base64::Engine;
use chrono::Utc;
use octo_core::family::abilities::Preset;
use octo_core::family::device::{Device, NetworkPlace};
use octo_core::models::domain::Song;
use octo_core::settings::{AppSettings, FamilySettings, GeneratedPlaylistSettings, SubsonicSettings};
use serde_json::{Value, json};
use tempfile::TempDir;
use wiremock::matchers::{method, path, path_regex, query_param};
use wiremock::{Mock, MockServer, Respond, ResponseTemplate};

use crate::app::{AppState, TestServices};
use crate::controllers::subsonic::route_tests::TestMetadata;
use crate::controllers::subsonic::test_app::{Reply, get, navidrome_json, query_of, received_paths, send};
use crate::http::pipeline::{App, build_with};
use crate::services::family::FamilyCaller;
use crate::services::family::family_playlists::{
    self, NewEntry, NoPlaylistAddRequests, SHARED_SCOPE, personal_scope,
};
use crate::services::family::member_credentials::tests::member;

const OUTSIDE: &str = "ext-deezer-song-77";

/// One library song: id, library, seconds.
struct LibrarySong {
    id: &'static str,
    library: i64,
    seconds: i64,
}

const SONGS: &[LibrarySong] = &[
    LibrarySong {
        id: "s-clean",
        library: 1,
        seconds: 200,
    },
    LibrarySong {
        id: "s-alex",
        library: 3,
        seconds: 100,
    },
];

/// Navidrome's `/rest` side: every sign-in checks out, a song detail answers its id, and the
/// playlist list carries one playlist per owner.
struct FakeNavidrome;

impl Respond for FakeNavidrome {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        let query = query_of(request);
        if request.url.path().contains("getSong") {
            let id = query.get("id").cloned().unwrap_or_default();
            return navidrome_json(&format!(
                r#"{{"subsonic-response":{{"status":"ok","version":"1.16.1","song":{{"id":"{id}","title":"{id}","artist":"A","album":"L","duration":200}}}}}}"#
            ));
        }
        if request.url.path().contains("getPlaylists") {
            return navidrome_json(
                r#"{"subsonic-response":{"status":"ok","version":"1.16.1","playlists":{"playlist":[
                    {"id":"p-own","name":"Mine","owner":"winters","public":false,"songCount":1},
                    {"id":"p-alex","name":"Alex's","owner":"alex","public":true,"songCount":1}]}}}"#,
            );
        }
        navidrome_json(r#"{"subsonic-response":{"status":"ok","version":"1.16.1","type":"navidrome"}}"#)
    }
}

/// Navidrome's native song detail: the song's id back in the native shape.
struct NativeSong;

impl Respond for NativeSong {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        let id = request.url.path().trim_start_matches("/api/song/").to_string();
        ResponseTemplate::new(200).set_body_json(json!({
            "id": id, "title": id, "artist": "Native artist", "album": "Native album",
            "duration": 200, "suffix": "flac"
        }))
    }
}

/// The app, the wiremock Navidrome under it, and the family: the owner `winters`, the member
/// alex and the listener sam. `refuse_native` answers the native playlist reads with a 401,
/// as Navidrome does a bearer it does not take.
struct Fixture {
    server: MockServer,
    _dir: TempDir,
    state: AppState,
    app: App,
}

async fn fixture_with(enabled: bool, prefix: Option<&str>, refuse_native: bool) -> Fixture {
    let server = crate::services::family::navidrome_admin::tests::navidrome().await;
    Mock::given(path_regex("^/rest/"))
        .respond_with(FakeNavidrome)
        .mount(&server)
        .await;
    let rows: Vec<Value> = SONGS
        .iter()
        .map(|song| {
            let base = format!("/{}", if song.library == 1 { "music" } else { "members" });
            json!({
                "id": song.id, "albumId": "al", "artistId": "ar",
                "albumArtistId": "ar", "libraryId": song.library, "libraryPath": base,
                "path": format!("{}/{}.flac", base, song.id),
                "size": 1000, "title": song.id,
                "duration": song.seconds as f64 + 0.4,
                "genre": "Rock", "genres": [{"id": "g-rock", "name": "Rock"}]
            })
        })
        .collect();
    Mock::given(method("GET"))
        .and(path("/api/song"))
        .and(query_param("_start", "0"))
        .respond_with(ResponseTemplate::new(200).set_body_json(Value::Array(rows)))
        .mount(&server)
        .await;
    // The native playlist collection: the list the native branch relays (and the bearer a
    // family create probes with).
    let list = if refuse_native {
        ResponseTemplate::new(401).set_body_json(json!({ "error": "Unauthorized" }))
    } else {
        ResponseTemplate::new(200).set_body_json(json!([
            { "id": "p-own", "name": "Mine", "ownerName": "winters", "public": false, "songCount": 1 },
            { "id": "p-alex", "name": "Alex's", "ownerName": "alex", "public": true, "songCount": 1 },
        ]))
    };
    Mock::given(method("GET"))
        .and(path("/api/playlist"))
        .respond_with(list)
        .mount(&server)
        .await;
    // A create that is not the family's relays here and gets Navidrome's own answer.
    Mock::given(method("POST"))
        .and(path("/api/playlist"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "nd-made", "name": "Made", "ownerName": "winters"
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path_regex("^/api/song/"))
        .respond_with(NativeSong)
        .mount(&server)
        .await;

    let dir = TempDir::new().expect("a temp dir");
    let mut family = FamilySettings {
        enabled,
        ..Default::default()
    };
    if let Some(prefix) = prefix {
        family.playlist_prefix = prefix.into();
    }
    let metadata = TestMetadata::default();
    metadata.songs.lock().insert(
        "77".into(),
        Song {
            id: OUTSIDE.into(),
            title: "Outside".into(),
            artist: "Somebody".into(),
            album: "Elsewhere".into(),
            duration: Some(180),
            is_local: false,
            external_provider: Some("deezer".into()),
            external_id: Some("77".into()),
            ..Default::default()
        },
    );
    let state = AppState::for_tests_with(
        AppSettings {
            subsonic: SubsonicSettings {
                url: Some(server.uri()),
                admin_username: Some("admin".into()),
                admin_password: Some("pw".into()),
                auto_detect_download_path: false,
                enable_search_discovery: false,
                ..Default::default()
            },
            generated_playlists: GeneratedPlaylistSettings {
                enabled: false,
                new_releases: false,
                rediscover: false,
                deep_cuts: false,
                popular_now: false,
                charts: String::new(),
                ..Default::default()
            },
            family,
            ..Default::default()
        },
        TestServices {
            config_dir: Some(dir.path().to_path_buf()),
            metadata: Some(Arc::new(metadata)),
            ..Default::default()
        },
    );
    assert!(state.family_vault.create_key_if_missing().expect("a key"));
    let store = &state.family_store;
    store.set_owner("winters");
    for (name, preset, library) in [("alex", Preset::Member, 3), ("sam", Preset::Listener, 4)] {
        let mut person = member(name, state.family_vault.seal("hidden").expect("sealed"));
        person.preset = preset;
        person.personal_library_id = Some(library);
        person.account_password =
            crate::services::family::passwords::seal(&state.family_vault, &format!("{name}-secret"))
                .expect("sealed");
        store.upsert_member(person);
        store.add_device(Device {
            id: format!("d_{name}"),
            username: name.into(),
            name: String::new(),
            created: Utc::now(),
            last_seen: None,
            app: String::new(),
            place: NetworkPlace::Home,
            seen_apps: Vec::new(),
            navidrome_jwt: None,
            quality: Default::default(),
        });
    }
    assert_eq!(state.library_index.refresh().await.expect("read"), SONGS.len());
    let app = build_with(state.clone(), crate::controllers::subsonic::routes());
    Fixture {
        server,
        _dir: dir,
        state,
        app,
    }
}

async fn fixture(enabled: bool) -> Fixture {
    fixture_with(enabled, None, false).await
}

async fn prefixed() -> Fixture {
    fixture_with(true, Some("#family"), false).await
}

impl Fixture {
    /// One native request with `user`'s bearer.
    async fn native(&self, method: Method, uri: &str, body: &str, user: &str) -> Reply {
        let bearer = bearer_of(user);
        send(
            &self.app,
            method,
            uri,
            &[
                ("x-nd-authorization", bearer.as_str()),
                ("content-type", "application/json"),
            ],
            Body::from(body.to_string()),
        )
        .await
    }

    /// The same request with no bearer at all.
    async fn native_unsigned(&self, method: Method, uri: &str, body: &str) -> Reply {
        send(
            &self.app,
            method,
            uri,
            &[("content-type", "application/json")],
            Body::from(body.to_string()),
        )
        .await
    }

    /// One REST call with `user`'s Subsonic sign-in, as JSON.
    async fn rest(&self, user: &str, rest: &str) -> Value {
        let joiner = if rest.contains('?') { '&' } else { '?' };
        let auth = match user {
            "winters" | "guest" => "u={user}&t=token&s=salt&v=1.16.1&c=test".replace("{user}", user),
            other => format!("u={other}&p={other}-secret&v=1.16.1&c=test"),
        };
        get(&self.app, &format!("/rest/{rest}{joiner}{auth}&f=json"))
            .await
            .envelope()
    }

    fn caller(&self, user: &str) -> FamilyCaller {
        match self.state.family_store.member(user) {
            Some(member) => FamilyCaller::member(user, member.preset, member.abilities()),
            None => FamilyCaller::unmanaged(user, self.state.family_store.is_owner(user)),
        }
    }

    /// A family playlist made by the owner with these songs.
    fn playlist(&self, songs: Vec<NewEntry>) -> String {
        let owner = self.caller("winters");
        let made = family_playlists::create(
            &self.state.family_playlists,
            &owner,
            "Road trip",
            false,
            Utc::now(),
        )
        .expect("made");
        family_playlists::add(
            &self.state.family_playlists,
            &NoPlaylistAddRequests,
            &owner,
            &made.id,
            songs,
            Utc::now(),
        )
        .expect("added");
        made.id
    }

    /// How many POSTs reached the native playlist collection.
    async fn native_creates_seen(&self) -> usize {
        self.server
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter(|request| request.method.as_str() == "POST" && request.url.path() == "/api/playlist")
            .count()
    }
}

/// A Navidrome-shaped bearer whose payload names `user`: no `sub` claim, so the family gate
/// (which reads only Octo's own signed tokens and `sub` subjects) leaves it for Navidrome to
/// check, exactly as a real client's token flows.
fn bearer_of(user: &str) -> String {
    let payload =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json!({ "username": user }).to_string());
    format!("Bearer x.{payload}.y")
}

fn library(id: &str, scope: &str, outside: Option<&str>) -> NewEntry {
    NewEntry {
        library_id: Some(id.into()),
        scope: scope.into(),
        outside_id: outside.map(str::to_string),
        ..NewEntry::default()
    }
}

fn outside(id: &str) -> NewEntry {
    NewEntry {
        outside_id: Some(id.into()),
        ..NewEntry::default()
    }
}

/// The row of `id` in a native list answer.
fn native_row<'a>(rows: &'a Value, id: &str) -> Option<&'a Value> {
    rows.as_array()?.iter().find(|row| row["id"] == id)
}

// ---- the list ----

/// The native list carries the family's rows beside the caller's own playlists, for the
/// family alone: the owner and every member see them, an account outside the family does
/// not, and with Family off nobody does.
#[tokio::test]
async fn the_native_list_serves_the_family_rows_to_the_family_alone() {
    let f = fixture(true).await;
    let id = f.playlist(vec![library("s-clean", SHARED_SCOPE, None), outside(OUTSIDE)]);
    for user in ["winters", "alex", "sam"] {
        let reply = f.native(Method::GET, "/api/playlist", "", user).await;
        assert_eq!(reply.status, StatusCode::OK, "{user}: {}", reply.text());
        let rows = reply.json();
        let row = native_row(&rows, &id).unwrap_or_else(|| panic!("{user} sees it: {rows}"));
        assert_eq!(row["ownerName"], "Family", "{user}");
        assert_eq!(row["public"], true, "{user}");
        assert_eq!(row["songCount"], 2, "{user}");
        assert_eq!(row["duration"], 380, "{user}");
        assert_eq!(row["octoFamily"], true, "{user}");
        // The caller's own Navidrome playlists keep their place beside the family's.
        assert!(native_row(&rows, "p-own").is_some(), "{user}: {rows}");
    }
    // The whole merged list is the page and the count.
    let reply = f.native(Method::GET, "/api/playlist", "", "alex").await;
    assert_eq!(reply.header("x-total-count"), Some("3"));
    assert_eq!(reply.json().as_array().expect("rows").len(), 3);

    // An account outside the family gets Navidrome's rows alone.
    let rows = f.native(Method::GET, "/api/playlist", "", "guest").await.json();
    assert!(native_row(&rows, &id).is_none(), "{rows}");

    // With Family off the rows are gone for everyone, as the REST list's are.
    let f = fixture(false).await;
    let id = f.playlist(vec![library("s-clean", SHARED_SCOPE, None)]);
    let rows = f.native(Method::GET, "/api/playlist", "", "alex").await.json();
    assert!(native_row(&rows, &id).is_none(), "{rows}");
}

// ---- the prefixed create ----

/// A native create whose name starts with the prefix founds a family playlist through the
/// same core REST's createPlaylist does: the songs asked for (a library id and an outside id
/// alike, `ids` and `tracks` rows both carried) go in with their maker, Navidrome never sees
/// the create, and the playlist then stands in the native list and in another member's REST
/// list alike.
#[tokio::test]
async fn a_native_prefixed_create_makes_a_family_playlist_the_family_sees() {
    let f = prefixed().await;
    let reply = f
        .native(
            Method::POST,
            "/api/playlist",
            r##"{"name":"#family Road trip","ids":["s-clean"],"tracks":[{"mediaFileId":"ext-deezer-song-77"}]}"##,
            "alex",
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    let made = reply.json();
    let id = made["id"].as_str().expect("an id").to_string();
    assert!(id.starts_with("of"), "{made}");
    assert_eq!(made["name"], "Road trip", "{made}");
    assert_eq!(made["ownerName"], "Family", "{made}");
    assert_eq!(made["octoFamilyCreatedBy"], "alex", "{made}");
    assert_eq!(made["songCount"], 2, "{made}");
    assert_eq!(made["duration"], 380, "{made}");
    let stored = f.state.family_playlists.get(&id).expect("there");
    assert_eq!(stored.name, "Road trip");
    assert_eq!(stored.created_by, "alex");
    assert_eq!(stored.entries.len(), 2);
    assert!(
        f.native_creates_seen().await == 0,
        "Navidrome never saw the create"
    );

    // The native list of the maker carries it at once…
    let rows = f.native(Method::GET, "/api/playlist", "", "alex").await.json();
    let listed = native_row(&rows, &id).expect("listed");
    assert_eq!(listed["ownerName"], "Family");
    // …and so does another member's REST getPlaylists.
    let sam = f.rest("sam", "getPlaylists").await;
    let rest_row = sam["playlists"]["playlist"]
        .as_array()
        .expect("rows")
        .iter()
        .find(|row| row["id"] == id)
        .unwrap_or_else(|| panic!("sam sees it: {sam}"));
    assert_eq!(rest_row["owner"], "Family");
    assert_eq!(rest_row["songCount"], 2);
}

/// The blank-name answer names the prefix, a server setting, so it is said to a caller whose
/// sign-in was taken alone: a bearer Navidrome refuses is answered with Navidrome's own
/// refusal, passed through as it came, and no word of the prefix rides on it — with or
/// without a bearer at all.
#[tokio::test]
async fn a_native_prefixed_create_without_a_taken_bearer_never_names_the_prefix() {
    let f = fixture_with(true, Some("#family"), true).await;
    for reply in [
        f.native(Method::POST, "/api/playlist", r##"{"name":"#family"}"##, "alex")
            .await,
        f.native_unsigned(Method::POST, "/api/playlist", r##"{"name":"#family X"}"##)
            .await,
    ] {
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{}", reply.text());
        assert!(
            !reply.text().contains("#family"),
            "no word of the prefix before the sign-in is taken: {}",
            reply.text()
        );
    }
    assert!(f.state.family_playlists.list().is_empty(), "nothing was made");
}

/// A name that is nothing but the prefix (or spaces after it) is refused with the prefix in
/// the message, once the caller is settled — and nothing is made.
#[tokio::test]
async fn a_native_prefixed_create_with_no_name_after_the_prefix_is_refused() {
    let f = prefixed().await;
    for name in ["#family", "#family  "] {
        let reply = f
            .native(
                Method::POST,
                "/api/playlist",
                &format!(r#"{{"name":"{name}"}}"#),
                "alex",
            )
            .await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{name}: {}", reply.text());
        let answer = reply.json();
        assert!(
            answer["error"]
                .as_str()
                .is_some_and(|message| message.contains("#family")),
            "{name}: {answer}"
        );
    }
    assert!(f.state.family_playlists.list().is_empty(), "nothing was made");
}

/// A create without the prefix (or with the prefix unset) is none of the family's: it relays
/// to Navidrome as it came and no family playlist appears.
#[tokio::test]
async fn a_native_create_without_the_prefix_is_navidromes_own() {
    let f = prefixed().await;
    let reply = f
        .native(Method::POST, "/api/playlist", r#"{"name":"Road trip"}"#, "alex")
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(reply.json()["id"], "nd-made", "Navidrome's own answer");
    assert_eq!(f.native_creates_seen().await, 1, "the create was relayed");
    assert!(f.state.family_playlists.list().is_empty());

    let f = fixture(true).await;
    let reply = f
        .native(
            Method::POST,
            "/api/playlist",
            r##"{"name":"#family Road trip"}"##,
            "alex",
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(f.native_creates_seen().await, 1, "the create was relayed");
    assert!(f.state.family_playlists.list().is_empty());
}

/// A caller outside the family, or a profile without playlist-editing rights, is refused
/// (the native kin of Subsonic's 50) and nothing is made.
#[tokio::test]
async fn a_native_prefixed_create_is_refused_outside_the_family() {
    let f = prefixed().await;
    let reply = f
        .native(
            Method::POST,
            "/api/playlist",
            r##"{"name":"#family Mine"}"##,
            "guest",
        )
        .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN, "{}", reply.text());
    let answer = reply.json();
    assert_eq!(
        answer["error"], "This account cannot change family playlists.",
        "{answer}"
    );
    assert!(f.state.family_playlists.list().is_empty(), "nothing was made");
}

// ---- the identity the family trusts ----

/// The family surfaces take their caller from the bearer the probe or the relayed page had
/// Navidrome take, never from the request's `u` parameter: a valid sign-in of an account
/// outside the family cannot found, list or read the family as the owner by adding `?u=` —
/// every surface refuses the pair, with no word of the family in the refusal — while a `u`
/// that names the bearer's own account changes nothing.
#[tokio::test]
async fn the_family_caller_comes_from_the_bearer_never_the_u_parameter() {
    let f = prefixed().await;
    let id = f.playlist(vec![library("s-clean", SHARED_SCOPE, None)]);

    // A create that would found a family playlist as the owner by parameter alone.
    let reply = f
        .native(
            Method::POST,
            "/api/playlist?u=winters",
            r##"{"name":"#family Spoofed"}"##,
            "guest",
        )
        .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN, "{}", reply.text());
    assert!(
        !reply.text().contains("#family"),
        "no word of the prefix for a sign-in that names no one: {}",
        reply.text()
    );
    assert!(
        f.state
            .family_playlists
            .list()
            .iter()
            .all(|made| made.name != "Spoofed"),
        "nothing was made"
    );

    // The list, the detail and the tracks answer none of the family for the mismatched pair.
    for uri in [
        "/api/playlist?u=winters".to_string(),
        format!("/api/playlist/{id}?u=winters"),
        format!("/api/playlist/{id}/tracks?u=winters"),
    ] {
        let reply = f.native(Method::GET, &uri, "", "guest").await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{uri}: {}", reply.text());
    }

    // A `u` that names the bearer's own account changes nothing.
    let reply = f.native(Method::GET, "/api/playlist?u=alex", "", "alex").await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    let rows = reply.json();
    let row = native_row(&rows, &id).expect("the family row beside the member's own");
    assert_eq!(row["ownerName"], "Family");
}

// ---- the detail and the tracks ----

/// The native detail of a family playlist serves its object; an `of` id nobody holds, or one
/// asked at by an account outside the family, is a clean 404 — not a radio-shaped one.
#[tokio::test]
async fn the_native_family_detail_answers_and_404s_the_rest() {
    let f = fixture(true).await;
    let id = f.playlist(vec![library("s-clean", SHARED_SCOPE, None), outside(OUTSIDE)]);
    let reply = f
        .native(Method::GET, &format!("/api/playlist/{id}"), "", "alex")
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    let detail = reply.json();
    assert_eq!(detail["id"], id, "{detail}");
    assert_eq!(detail["name"], "Road trip", "{detail}");
    assert_eq!(detail["ownerName"], "Family", "{detail}");
    assert_eq!(detail["public"], true, "{detail}");
    assert_eq!(detail["songCount"], 2, "{detail}");
    assert_eq!(detail["duration"], 380, "{detail}");

    let unknown = format!("of{}", "A".repeat(20));
    for user in ["alex", "guest"] {
        let reply = f
            .native(Method::GET, &format!("/api/playlist/{unknown}"), "", user)
            .await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{user}");
        assert_eq!(reply.json()["error"], "Playlist not found", "{user}");
    }
    let reply = f
        .native(Method::GET, &format!("/api/playlist/{id}"), "", "guest")
        .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND, "a guest: {}", reply.text());
    assert_eq!(reply.json()["error"], "Playlist not found");
}

/// The native tracks of a family playlist serve each viewer their own positions — a library
/// song read as the caller (Navidrome's own native row), a personal-library song falling
/// back to its outside version for everyone else, an outside id in the native row shape —
/// with the whole list's count and the page the client asked for.
#[tokio::test]
async fn the_native_family_tracks_serve_each_viewers_positions_with_paging() {
    let f = fixture(true).await;
    let id = f.playlist(vec![
        library("s-clean", SHARED_SCOPE, None),
        library("s-alex", &personal_scope("alex"), Some(OUTSIDE)),
        outside(OUTSIDE),
    ]);
    let reply = f
        .native(Method::GET, &format!("/api/playlist/{id}/tracks"), "", "alex")
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    let rows = reply.json();
    let ids: Vec<&str> = rows
        .as_array()
        .expect("rows")
        .iter()
        .map(|row| row["id"].as_str().expect("an id"))
        .collect();
    assert_eq!(ids, ["s-clean", "s-alex", OUTSIDE], "{rows}");
    assert_eq!(reply.header("x-total-count"), Some("3"));

    // Sam is not shown alex's own library song: its outside version serves in its place.
    let reply = f
        .native(Method::GET, &format!("/api/playlist/{id}/tracks"), "", "sam")
        .await;
    let rows = reply.json();
    let ids: Vec<&str> = rows
        .as_array()
        .expect("rows")
        .iter()
        .map(|row| row["id"].as_str().expect("an id"))
        .collect();
    assert_eq!(ids, ["s-clean", OUTSIDE, OUTSIDE], "{rows}");
    assert_eq!(reply.header("x-total-count"), Some("3"));

    // The page the client asked for, with the whole list's count above it.
    let reply = f
        .native(
            Method::GET,
            &format!("/api/playlist/{id}/tracks?_start=1&_end=2"),
            "",
            "sam",
        )
        .await;
    let rows = reply.json();
    let ids: Vec<&str> = rows
        .as_array()
        .expect("rows")
        .iter()
        .map(|row| row["id"].as_str().expect("an id"))
        .collect();
    assert_eq!(ids, [OUTSIDE], "{rows}");
    assert_eq!(reply.header("x-total-count"), Some("3"));
}

// ---- the mutations that stay out ----

/// Changing a family playlist from the native API is out of scope on purpose, and the answer
/// says where changes go — not the generated playlists' read-only word, which stays the
/// stations' own.
#[tokio::test]
async fn a_native_mutation_of_a_family_playlist_gets_the_honest_message() {
    let f = fixture(true).await;
    let id = f.playlist(vec![library("s-clean", SHARED_SCOPE, None)]);
    for (method, uri, body) in [
        (
            Method::POST,
            format!("/api/playlist/{id}/tracks"),
            r#"{"ids":["s-clean"]}"#.to_string(),
        ),
        (
            Method::PUT,
            format!("/api/playlist/{id}"),
            r#"{"name":"Renamed"}"#.to_string(),
        ),
        (Method::DELETE, format!("/api/playlist/{id}"), String::new()),
    ] {
        let reply = f.native(method, &uri, &body, "alex").await;
        assert_eq!(reply.status, StatusCode::METHOD_NOT_ALLOWED, "{}", reply.text());
        let answer = reply.json();
        assert_eq!(
            answer["error"], "Family playlists are changed from the panel or a Subsonic-mode connection",
            "{answer}"
        );
        assert!(
            !reply.text().contains("read-only"),
            "not the stations' word: {}",
            reply.text()
        );
    }
    assert_eq!(f.state.family_playlists.get(&id).expect("there").entries.len(), 1);

    // A generated playlist keeps its own read-only word.
    let generated = format!("oi{}", "A".repeat(20));
    let reply = f
        .native(
            Method::PUT,
            &format!("/api/playlist/{generated}"),
            r#"{"name":"X"}"#,
            "alex",
        )
        .await;
    assert_eq!(reply.status, StatusCode::METHOD_NOT_ALLOWED, "{}", reply.text());
    assert_eq!(reply.json()["error"], "Octo's generated playlists are read-only");
    assert!(
        !received_paths(&f.server)
            .await
            .iter()
            .any(|p| p.contains("playlist")),
        "nothing reached Navidrome"
    );
}
