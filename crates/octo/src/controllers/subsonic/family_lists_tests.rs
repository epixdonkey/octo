//! Family lists through the whole pipeline: family playlists, kid profiles, the owner's
//! privacy and members' own songs marked, over a wiremock Navidrome whose library index holds
//! a shared library (1) and the own libraries of alex (3), sam (4) and kid (5).

use std::sync::Arc;

use axum::http::Method;
use chrono::Utc;
use octo_core::family::abilities::Preset;
use octo_core::family::device::{Device, NetworkPlace};
use octo_core::models::domain::Song;
use octo_core::settings::{AppSettings, FamilySettings, GeneratedPlaylistSettings, SubsonicSettings};
use octo_subsonic::subsonic_response_builder::json_shape_to_xml;
use parking_lot::Mutex;
use serde_json::{Value, json};
use tempfile::TempDir;
use wiremock::matchers::{method, path, path_regex, query_param};
use wiremock::{Mock, MockServer, Respond, ResponseTemplate};

use super::route_tests::{TestMetadata, query_value};
use super::test_app::{Reply, received_paths, send};
use crate::app::{AppState, TestServices};
use crate::http::pipeline::{App, build_with};
use crate::services::family::family_playlists::{
    self, NewEntry, NoPlaylistAddRequests, PlaylistAddRequests, SHARED_SCOPE, personal_scope,
};
use crate::services::family::member_credentials::tests::member;
use crate::services::family::{FamilyCaller, FamilyPlaylist, FamilyPlaylistEntry, kid_filter};

const NS: &str = "http://subsonic.org/restapi";
const OUTSIDE: &str = "ext-deezer-song-77";
const OUTSIDE_EXPLICIT: &str = "ext-deezer-song-78";

/// One library song: id, library, explicit mark, album, artist, seconds.
struct LibrarySong {
    id: &'static str,
    library: i64,
    explicit: &'static str,
    album: &'static str,
    artist: &'static str,
    seconds: i64,
}

const SONGS: &[LibrarySong] = &[
    LibrarySong {
        id: "s-clean",
        library: 1,
        explicit: "clean",
        album: "al-shared",
        artist: "ar-shared",
        seconds: 200,
    },
    LibrarySong {
        id: "s-explicit",
        library: 1,
        explicit: "explicit",
        album: "al-shared",
        artist: "ar-shared",
        seconds: 150,
    },
    LibrarySong {
        id: "s-unrated",
        library: 1,
        explicit: "",
        album: "al-shared",
        artist: "ar-shared",
        seconds: 250,
    },
    LibrarySong {
        id: "s-alex",
        library: 3,
        explicit: "",
        album: "al-alex",
        artist: "ar-shared",
        seconds: 100,
    },
    LibrarySong {
        id: "s-alex-only",
        library: 3,
        explicit: "",
        album: "al-alex2",
        artist: "ar-alex",
        seconds: 120,
    },
    LibrarySong {
        id: "s-sam",
        library: 4,
        explicit: "",
        album: "al-sam",
        artist: "ar-sam",
        seconds: 90,
    },
];

fn library_song(id: &str) -> Option<&'static LibrarySong> {
    SONGS.iter().find(|song| song.id == id)
}

/// Every song is Rock but Alex's own album of one, which is Pop.
fn genre_of(id: &str) -> &'static str {
    if id == "s-alex-only" { "Pop" } else { "Rock" }
}

/// A song as Navidrome answers it. Older clients (DSub) get no `explicitStatus`.
fn song_json(song: &LibrarySong, legacy: bool) -> Value {
    let mut fields = json!({
        "id": song.id, "parent": song.album, "isDir": false, "title": song.id,
        "album": song.album, "albumId": song.album, "artist": song.artist,
        "artistId": song.artist, "duration": song.seconds, "suffix": "flac", "type": "music"
    });
    if !legacy {
        fields["explicitStatus"] = json!(song.explicit);
    }
    fields
}

/// The library as Navidrome would answer each endpoint.
struct FakeNavidrome;

fn ok(mut fields: Value) -> Value {
    fields["status"] = json!("ok");
    fields["version"] = json!("1.16.1");
    fields
}

impl FakeNavidrome {
    fn answer(&self, endpoint: &str, request: &wiremock::Request) -> Value {
        let legacy = query_value(request, "c").as_deref() == Some("DSub");
        let songs = |ids: &[&str]| -> Vec<Value> {
            ids.iter()
                .filter_map(|id| library_song(id))
                .map(|song| song_json(song, legacy))
                .collect()
        };
        match endpoint {
            "ping" => ok(json!({})),
            "getSong" if query_value(request, "id").as_deref() == Some("s-busy") => {
                json!({"status": "failed", "version": "1.16.1",
                    "error": {"code": 0, "message": "busy"}})
            }
            "getSong" => match query_value(request, "id").as_deref().and_then(library_song) {
                Some(song) => ok(json!({ "song": song_json(song, legacy) })),
                None => json!({"status": "failed", "version": "1.16.1",
                    "error": {"code": 70, "message": "Song not found"}}),
            },
            "search3" => ok(json!({"searchResult3": {
                "artist": [{"id": "ar-shared", "name": "Shared"}, {"id": "ar-alex", "name": "Alex only"}],
                "album": [{"id": "al-shared", "name": "Shared"}, {"id": "al-alex2", "name": "Alex only"}],
                "song": songs(&["s-clean", "s-explicit", "s-unrated", "s-alex", "s-alex-only"]),
            }})),
            "getAlbum" => ok(json!({"album": {
                "id": "al-shared", "name": "Shared", "artist": "Shared", "artistId": "ar-shared",
                "songCount": 3, "duration": 600,
                "song": songs(&["s-clean", "s-explicit", "s-unrated"]),
            }})),
            "getArtist" => ok(json!({"artist": {
                "id": "ar-shared", "name": "Shared", "albumCount": 2,
                "album": [
                    {"id": "al-shared", "name": "Shared", "artistId": "ar-shared", "songCount": 3},
                    {"id": "al-alex", "name": "Alex's copy", "artistId": "ar-shared", "songCount": 1},
                ],
            }})),
            "getArtists" => ok(json!({"artists": {"ignoredArticles": "The", "index": [
                {"name": "A", "artist": [{"id": "ar-alex", "name": "Alex only", "albumCount": 1}]},
                {"name": "S", "artist": [{"id": "ar-shared", "name": "Shared", "albumCount": 2}]},
            ]}})),
            "getMusicFolders" => ok(json!({"musicFolders": {"musicFolder": [
                {"id": 1, "name": "Music"}, {"id": 3, "name": "Alex's library"},
                {"id": 4, "name": "Sam's library"}, {"id": 5, "name": "Kid's library"},
            ]}})),
            "getPlaylists" => ok(json!({"playlists": {"playlist": [
                {"id": "p-own", "name": "Mine", "owner": "winters", "public": false, "songCount": 1},
                {"id": "p-alex", "name": "Alex's", "owner": "alex", "public": true, "songCount": 1},
            ]}})),
            "getRandomSongs" => ok(json!({"randomSongs": {
                "song": songs(&["s-clean", "s-explicit", "s-alex"]),
            }})),
            // Navidrome counts every library for an admin: Rock is the three shared songs, Alex's
            // and Sam's (albums al-shared, al-alex, al-sam), Pop is Alex's own album alone.
            "getGenres" => ok(json!({"genres": {"genre": [
                {"value": "Rock", "songCount": 5, "albumCount": 3},
                {"value": "Pop", "songCount": 1, "albumCount": 1},
            ]}})),
            "getPlaylist" | "updatePlaylist" | "deletePlaylist" => json!({"status": "failed",
                "version": "1.16.1", "error": {"code": 70, "message": "Playlist not found"}}),
            _ => ok(json!({})),
        }
    }
}

impl Respond for FakeNavidrome {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        if request.url.path().contains("getSong") && query_value(request, "id").as_deref() == Some("s-down") {
            return ResponseTemplate::new(503);
        }
        let endpoint = request
            .url
            .path()
            .trim_start_matches("/rest/")
            .trim_end_matches(".view")
            .to_string();
        let response = self.answer(&endpoint, request);
        let format = query_value(request, "f");
        if format.as_deref() == Some("json") {
            let body = json!({ "subsonic-response": response }).to_string();
            return ResponseTemplate::new(200).set_body_raw(body, "application/json");
        }
        if format.as_deref() == Some("jsonp") {
            let callback = query_value(request, "callback").unwrap_or_default();
            let body = format!("{callback}({});", json!({ "subsonic-response": response }));
            return ResponseTemplate::new(200).set_body_raw(body, "application/javascript");
        }
        let mut root = json_shape_to_xml(Some(NS), "subsonic-response", &response);
        root.set_attr("xmlns", NS);
        let body = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n{}",
            root.to_xml_string()
        );
        ResponseTemplate::new(200).set_body_raw(body, "application/xml")
    }
}

/// Every addition that waited, as the request hook heard it.
#[derive(Default)]
struct HeardAdditions(Mutex<Vec<(String, usize, String)>>);

impl PlaylistAddRequests for HeardAdditions {
    fn addition_waits(&self, playlist: &FamilyPlaylist, index: usize, _: &FamilyPlaylistEntry, by: &str) {
        self.0.lock().push((playlist.id.clone(), index, by.to_string()));
    }
}

struct Fixture {
    _server: MockServer,
    _dir: TempDir,
    state: AppState,
    app: App,
}

fn auth(user: &str) -> String {
    match user {
        "winters" | "guest" => format!("u={user}&t=token&s=salt&v=1.16.1&c=test"),
        _ => format!("u={user}&p={user}-secret&v=1.16.1&c=test"),
    }
}

/// The owner `winters`, the member alex, the listener sam and the kid profile kid (each with
/// the password `<name>-secret` and an Octo app device), with the library index read.
async fn fixture(enabled: bool) -> Fixture {
    fixture_with(enabled, |_| {}).await
}

async fn fixture_with(enabled: bool, change: impl FnOnce(&mut FamilySettings)) -> Fixture {
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
                "id": song.id, "albumId": song.album, "artistId": song.artist,
                "albumArtistId": song.artist, "libraryId": song.library, "libraryPath": base,
                "path": format!("{}/{}.flac", song.album, song.id),
                "explicitStatus": song.explicit, "size": 1000, "title": song.id,
                "duration": song.seconds as f64 + 0.4,
                "genre": genre_of(song.id),
                "genres": [{"id": format!("g-{}", genre_of(song.id)), "name": genre_of(song.id)}]
            })
        })
        .collect();
    Mock::given(method("GET"))
        .and(path("/api/song"))
        .and(query_param("_start", "0"))
        .respond_with(ResponseTemplate::new(200).set_body_json(Value::Array(rows)))
        .mount(&server)
        .await;
    let dir = TempDir::new().expect("a temp dir");
    let mut family = FamilySettings {
        enabled,
        ..Default::default()
    };
    change(&mut family);
    let metadata = TestMetadata::default();
    for (id, title, explicit) in [("77", "Outside", 0), ("78", "Outside explicit", 1)] {
        metadata.songs.lock().insert(
            id.to_string(),
            Song {
                id: format!("ext-deezer-song-{id}"),
                title: title.into(),
                artist: "Somebody".into(),
                album: "Elsewhere".into(),
                duration: Some(180),
                is_local: false,
                external_provider: Some("deezer".into()),
                external_id: Some(id.into()),
                explicit_content_lyrics: Some(explicit),
                ..Default::default()
            },
        );
    }
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
    for (name, preset, library) in [
        ("alex", Preset::Member, 3),
        ("sam", Preset::Listener, 4),
        ("kid", Preset::Kid, 5),
    ] {
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
    let app = build_with(state.clone(), super::routes());
    Fixture {
        _server: server,
        _dir: dir,
        state,
        app,
    }
}

impl Fixture {
    async fn get(&self, user: &str, rest: &str) -> Reply {
        let joiner = if rest.contains('?') { '&' } else { '?' };
        send(
            &self.app,
            Method::GET,
            &format!("/rest/{rest}{joiner}{}", auth(user)),
            &[],
            "",
        )
        .await
    }

    async fn json(&self, user: &str, rest: &str) -> Value {
        let joiner = if rest.contains('?') { '&' } else { '?' };
        self.get(user, &format!("{rest}{joiner}f=json")).await.envelope()
    }

    fn caller(&self, user: &str) -> FamilyCaller {
        match self.state.family_store.member(user) {
            Some(member) => FamilyCaller::member(user, member.preset, member.abilities()),
            None => FamilyCaller::unmanaged(user, self.state.family_store.is_owner(user)),
        }
    }

    /// A family playlist made by the owner with these songs.
    fn playlist(&self, approve: bool, songs: Vec<NewEntry>) -> String {
        let owner = self.caller("winters");
        let made = family_playlists::create(
            &self.state.family_playlists,
            &owner,
            "Road trip",
            approve,
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

    fn entries(&self, id: &str) -> Vec<FamilyPlaylistEntry> {
        self.state.family_playlists.get(id).expect("there").entries
    }
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

/// The ids under `list` (an array of objects, or one object).
fn ids(list: &Value) -> Vec<String> {
    match list {
        Value::Array(items) => items
            .iter()
            .map(|item| {
                item["id"]
                    .as_str()
                    .map_or_else(|| item["id"].to_string(), str::to_string)
            })
            .collect(),
        Value::Object(_) => vec![list["id"].as_str().unwrap_or("").to_string()],
        _ => Vec::new(),
    }
}

fn row<'a>(playlists: &'a Value, id: &str) -> Option<&'a Value> {
    playlists["playlists"]["playlist"]
        .as_array()?
        .iter()
        .find(|row| row["id"] == id)
}

// ---- family playlists ----

#[tokio::test]
async fn family_playlist_lists_for_every_member() {
    let f = fixture(true).await;
    let id = f.playlist(
        false,
        vec![library("s-clean", SHARED_SCOPE, None), outside(OUTSIDE)],
    );
    for user in ["winters", "alex", "sam", "kid"] {
        let answer = f.json(user, "getPlaylists").await;
        let row = row(&answer, &id).unwrap_or_else(|| panic!("{user} sees it: {answer}"));
        assert_eq!(row["owner"], "Family", "{user}");
        assert_eq!(row["public"], true, "{user}");
        assert_eq!(row["songCount"], 2, "{user}");
        assert_eq!(row["readonly"], user == "kid", "{user}");
    }
    let guest = f.json("guest", "getPlaylists").await;
    assert!(row(&guest, &id).is_none(), "{guest}");
}

#[tokio::test]
async fn get_playlists_unchanged_when_family_off() {
    let f = fixture(false).await;
    let owner = FamilyCaller::unmanaged("winters", true);
    family_playlists::create(&f.state.family_playlists, &owner, "Road trip", false, Utc::now())
        .expect("made");
    let answer = f.json("alex", "getPlaylists").await;
    assert_eq!(
        ids(&answer["playlists"]["playlist"]),
        ["p-own", "p-alex"],
        "{answer}"
    );
}

#[tokio::test]
async fn personal_entry_falls_back_to_outside_for_others() {
    let f = fixture(true).await;
    let id = f.playlist(
        false,
        vec![
            library("s-clean", SHARED_SCOPE, None),
            library("s-alex", &personal_scope("alex"), Some(OUTSIDE)),
            library("s-alex-only", &personal_scope("alex"), None),
        ],
    );
    let alex = f.json("alex", &format!("getPlaylist?id={id}")).await;
    let entries = &alex["playlist"]["entry"];
    assert_eq!(ids(entries), ["s-clean", "s-alex", "s-alex-only"], "{alex}");
    assert_eq!(entries[1]["octoPersonal"], true, "alex's own song is marked");
    assert!(entries[0].get("octoPersonal").is_none());
    assert_eq!(alex["playlist"]["songCount"], 3);

    let sam = f.json("sam", &format!("getPlaylist?id={id}")).await;
    assert_eq!(ids(&sam["playlist"]["entry"]), ["s-clean", OUTSIDE], "{sam}");
    assert!(sam["playlist"]["entry"][1].get("octoPersonal").is_none());
    assert_eq!(sam["playlist"]["owner"], "Family");

    // In XML too, as entries of the playlist.
    let xml = f.get("sam", &format!("getPlaylist?id={id}")).await.text();
    assert!(xml.contains(&format!("id=\"{OUTSIDE}\"")), "{xml}");
    assert!(!xml.contains("id=\"s-alex\""), "{xml}");
}

#[tokio::test]
async fn pending_entries_hidden_from_others() {
    let f = fixture(true).await;
    let id = f.playlist(true, vec![library("s-clean", SHARED_SCOPE, None)]);
    let added = f
        .json(
            "sam",
            &format!("updatePlaylist?playlistId={id}&songIdToAdd={OUTSIDE}"),
        )
        .await;
    assert_eq!(added["status"], "ok", "{added}");
    let seen = |answer: Value| ids(&answer["playlist"]["entry"]);
    assert_eq!(
        seen(f.json("sam", &format!("getPlaylist?id={id}")).await),
        ["s-clean", OUTSIDE]
    );
    assert_eq!(
        seen(f.json("alex", &format!("getPlaylist?id={id}")).await),
        ["s-clean"]
    );
    assert_eq!(
        seen(f.json("winters", &format!("getPlaylist?id={id}")).await),
        ["s-clean", OUTSIDE]
    );
    let alex_rows = f.json("alex", "getPlaylists").await;
    assert_eq!(row(&alex_rows, &id).expect("listed")["songCount"], 1);
}

#[tokio::test]
async fn approval_mode_holds_additions() {
    let f = fixture(true).await;
    let heard = Arc::new(HeardAdditions::default());
    f.state.family_playlist_requests.set(heard.clone());
    let id = f.playlist(true, Vec::new());
    f.json(
        "sam",
        &format!("updatePlaylist?playlistId={id}&songIdToAdd={OUTSIDE}&songIdToAdd=s-clean"),
    )
    .await;
    f.json(
        "winters",
        &format!("updatePlaylist?playlistId={id}&songIdToAdd=s-unrated"),
    )
    .await;
    let entries = f.entries(&id);
    assert_eq!(
        entries.iter().map(|e| e.pending).collect::<Vec<_>>(),
        [true, true, false]
    );
    assert_eq!(
        *heard.0.lock(),
        [
            (id.clone(), 0, "sam".to_string()),
            (id.clone(), 1, "sam".to_string())
        ]
    );
}

#[tokio::test]
async fn member_adds_outside_song() {
    let f = fixture(true).await;
    let id = f.playlist(false, Vec::new());
    let answer = f
        .json(
            "alex",
            &format!(
                "updatePlaylist?playlistId={id}&songIdToAdd={OUTSIDE}&songIdToAdd=s-alex&songIdToAdd=s-clean"
            ),
        )
        .await;
    assert_eq!(answer["status"], "ok", "{answer}");
    let entries = f.entries(&id);
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[0].library_id, None);
    assert_eq!(entries[0].outside_id.as_deref(), Some(OUTSIDE));
    assert_eq!(entries[1].library_id.as_deref(), Some("s-alex"));
    assert_eq!(entries[1].scope, "Personal:alex");
    assert_eq!(entries[2].scope, "Shared");
    assert!(entries.iter().all(|e| e.added_by == "alex" && !e.pending));
    // Nothing reached Navidrome's own updatePlaylist.
    let sam = f.json("sam", &format!("getPlaylist?id={id}")).await;
    assert_eq!(ids(&sam["playlist"]["entry"]), [OUTSIDE, "s-clean"], "{sam}");
}

#[tokio::test]
async fn index_maps_from_viewer_list() {
    let f = fixture(true).await;
    let id = f.playlist(
        false,
        vec![
            library("s-alex-only", &personal_scope("alex"), None),
            outside(OUTSIDE),
            outside(OUTSIDE_EXPLICIT),
        ],
    );
    // Sam is shown the two outside songs only; their second is stored third.
    let answer = f
        .json(
            "sam",
            &format!("updatePlaylist?playlistId={id}&songIndexToRemove=1"),
        )
        .await;
    assert_eq!(answer["status"], "ok", "{answer}");
    let left: Vec<Option<String>> = f
        .entries(&id)
        .into_iter()
        .map(|e| e.library_id.or(e.outside_id))
        .collect();
    assert_eq!(left, [Some("s-alex-only".to_string()), Some(OUTSIDE.to_string())]);
}

#[tokio::test]
async fn removal_skips_an_entry_octo_cannot_read() {
    let f = fixture(true).await;
    let id = f.playlist(
        false,
        vec![
            library("s-missing", SHARED_SCOPE, None),
            outside(OUTSIDE),
            library("s-clean", SHARED_SCOPE, None),
        ],
    );
    // Navidrome refuses the first entry's song, so alex is shown the other two.
    let list = f.json("alex", &format!("getPlaylist?id={id}")).await;
    assert_eq!(ids(&list["playlist"]["entry"]), [OUTSIDE, "s-clean"], "{list}");
    let answer = f
        .json(
            "alex",
            &format!("updatePlaylist?playlistId={id}&songIndexToRemove=1"),
        )
        .await;
    assert_eq!(answer["status"], "ok", "{answer}");
    let left: Vec<Option<String>> = f
        .entries(&id)
        .into_iter()
        .map(|e| e.library_id.or(e.outside_id))
        .collect();
    assert_eq!(left, [Some("s-missing".to_string()), Some(OUTSIDE.to_string())]);
}

#[tokio::test]
async fn removal_counts_over_a_clean_only_listeners_answer() {
    let f = fixture(true).await;
    // The kid profile hears clean songs only; here it may also edit family playlists.
    let mut editor = f.state.family_store.member("kid").expect("a member");
    editor.edits.insert("familyPlaylistsEdit".into(), json!(true));
    f.state.family_store.upsert_member(editor);
    let id = f.playlist(
        false,
        vec![
            outside(OUTSIDE_EXPLICIT),
            outside(OUTSIDE),
            library("s-clean", SHARED_SCOPE, None),
        ],
    );
    let list = f.json("kid", &format!("getPlaylist?id={id}")).await;
    assert_eq!(ids(&list["playlist"]["entry"]), [OUTSIDE, "s-clean"], "{list}");
    let answer = f
        .json(
            "kid",
            &format!("updatePlaylist?playlistId={id}&songIndexToRemove=1"),
        )
        .await;
    assert_eq!(answer["status"], "ok", "{answer}");
    let left: Vec<Option<String>> = f
        .entries(&id)
        .into_iter()
        .map(|e| e.library_id.or(e.outside_id))
        .collect();
    assert_eq!(
        left,
        [Some(OUTSIDE_EXPLICIT.to_string()), Some(OUTSIDE.to_string())]
    );
}

/// The ids of a stored playlist's entries, library song or outside song.
fn left_ids(f: &Fixture, id: &str) -> Vec<Option<String>> {
    f.entries(id)
        .into_iter()
        .map(|e| e.library_id.or(e.outside_id))
        .collect()
}

#[tokio::test]
async fn removal_skips_a_song_the_index_marks_explicit_for_a_kid() {
    let f = fixture(true).await;
    let mut editor = f.state.family_store.member("kid").expect("a member");
    editor.edits.insert("familyPlaylistsEdit".into(), json!(true));
    f.state.family_store.upsert_member(editor);
    let id = f.playlist(
        false,
        vec![
            library("s-explicit", SHARED_SCOPE, None),
            outside(OUTSIDE),
            library("s-clean", SHARED_SCOPE, None),
        ],
    );
    let list = f.json("kid", &format!("getPlaylist?id={id}")).await;
    assert_eq!(ids(&list["playlist"]["entry"]), [OUTSIDE, "s-clean"], "{list}");
    let answer = f
        .json(
            "kid",
            &format!("updatePlaylist?playlistId={id}&songIndexToRemove=1"),
        )
        .await;
    assert_eq!(answer["status"], "ok", "{answer}");
    assert_eq!(
        left_ids(&f, &id),
        [Some("s-explicit".to_string()), Some(OUTSIDE.to_string())]
    );
}

#[tokio::test]
async fn removal_counts_unrated_songs_as_explicit_when_set() {
    let f = fixture_with(true, |family| family.kid_unrated_is_explicit = true).await;
    let mut editor = f.state.family_store.member("kid").expect("a member");
    editor.edits.insert("familyPlaylistsEdit".into(), json!(true));
    f.state.family_store.upsert_member(editor);
    let id = f.playlist(
        false,
        vec![
            library("s-unrated", SHARED_SCOPE, None),
            library("s-clean", SHARED_SCOPE, None),
        ],
    );
    let list = f.json("kid", &format!("getPlaylist?id={id}")).await;
    assert_eq!(ids(&list["playlist"]["entry"]), ["s-clean"], "{list}");
    let answer = f
        .json(
            "kid",
            &format!("updatePlaylist?playlistId={id}&songIndexToRemove=0"),
        )
        .await;
    assert_eq!(answer["status"], "ok", "{answer}");
    assert_eq!(left_ids(&f, &id), [Some("s-unrated".to_string())]);
}

#[tokio::test]
async fn removal_counts_over_the_owners_answer() {
    let f = fixture(true).await;
    let id = f.playlist(
        false,
        vec![
            library("s-alex-only", SHARED_SCOPE, None),
            outside(OUTSIDE),
            library("s-clean", SHARED_SCOPE, None),
        ],
    );
    let list = f.json("winters", &format!("getPlaylist?id={id}")).await;
    assert_eq!(ids(&list["playlist"]["entry"]), [OUTSIDE, "s-clean"], "{list}");
    let answer = f
        .json(
            "winters",
            &format!("updatePlaylist?playlistId={id}&songIndexToRemove=1"),
        )
        .await;
    assert_eq!(answer["status"], "ok", "{answer}");
    assert_eq!(
        left_ids(&f, &id),
        [Some("s-alex-only".to_string()), Some(OUTSIDE.to_string())]
    );
}

/// A removal asked while Navidrome cannot read `unreadable` is refused and changes nothing,
/// not even the name.
async fn assert_removal_refused_when_unread(unreadable: &str) {
    let f = fixture(true).await;
    // The playlist is made by the owner, who may also rename it.
    let id = f.playlist(
        false,
        vec![
            library(unreadable, SHARED_SCOPE, Some(OUTSIDE)),
            library("s-clean", SHARED_SCOPE, None),
        ],
    );
    let answer = f
        .json(
            "winters",
            &format!("updatePlaylist?playlistId={id}&name=New&songIndexToRemove=0"),
        )
        .await;
    assert_eq!(answer["status"], "failed", "{answer}");
    assert_eq!(answer["error"]["code"], 0, "{answer}");
    assert_eq!(
        answer["error"]["message"],
        "Octo can't reach Navidrome to read this playlist's songs, so nothing was taken out.",
        "{answer}"
    );
    assert_eq!(f.entries(&id).len(), 2);
    assert_eq!(
        f.state.family_playlists.get(&id).expect("there").name,
        "Road trip"
    );
}

#[tokio::test]
async fn removal_refuses_when_navidrome_cannot_read_a_song() {
    assert_removal_refused_when_unread("s-down").await;
}

#[tokio::test]
async fn getsong_errors_other_than_not_found_refuse_removal() {
    assert_removal_refused_when_unread("s-busy").await;
}

#[tokio::test]
async fn kid_cannot_edit() {
    let f = fixture(true).await;
    let id = f.playlist(false, vec![outside(OUTSIDE)]);
    let refused = f
        .json(
            "kid",
            &format!("updatePlaylist?playlistId={id}&songIdToAdd=s-clean"),
        )
        .await;
    assert_eq!(refused["error"]["code"], 50, "{refused}");
    let refused = f
        .json(
            "kid",
            &format!("updatePlaylist?playlistId={id}&songIndexToRemove=0"),
        )
        .await;
    assert_eq!(refused["error"]["code"], 50, "{refused}");
    assert_eq!(f.entries(&id).len(), 1);
    // Renaming needs the creator or a manager, deleting a manager.
    let refused = f
        .json("alex", &format!("updatePlaylist?playlistId={id}&name=Mine"))
        .await;
    assert_eq!(refused["error"]["code"], 50, "{refused}");
    let refused = f.json("alex", &format!("deletePlaylist?id={id}")).await;
    assert_eq!(refused["error"]["code"], 50, "{refused}");
    let renamed = f
        .json("winters", &format!("updatePlaylist?playlistId={id}&name=Summer"))
        .await;
    assert_eq!(renamed["status"], "ok", "{renamed}");
    assert_eq!(f.state.family_playlists.get(&id).expect("there").name, "Summer");
    let replaced = f
        .json(
            "winters",
            &format!("createPlaylist?playlistId={id}&songId=s-clean"),
        )
        .await;
    assert_eq!(replaced["error"]["code"], 50, "{replaced}");
    let deleted = f.json("winters", &format!("deletePlaylist?id={id}")).await;
    assert_eq!(deleted["status"], "ok", "{deleted}");
    assert!(f.state.family_playlists.get(&id).is_none());
    let gone = f.json("alex", &format!("getPlaylist?id={id}")).await;
    assert_eq!(gone["error"]["code"], 70, "{gone}");
}

// ---- family playlists founded from an app (the playlist prefix) ----

/// The fixture with the playlist prefix set: a create whose name starts with it becomes a
/// family playlist.
async fn prefixed(prefix: &str) -> Fixture {
    fixture_with(true, |family| family.playlist_prefix = prefix.into()).await
}

#[tokio::test]
async fn a_prefixed_create_makes_a_family_playlist_the_family_sees() {
    let f = prefixed("#family").await;
    let made = f
        .json("alex", "createPlaylist?name=%23family%20Road%20trip")
        .await;
    assert_eq!(made["status"], "ok", "{made}");
    let playlist = &made["playlist"];
    let id = playlist["id"].as_str().expect("an id").to_string();
    assert!(id.starts_with("of"), "{made}");
    assert_eq!(playlist["name"], "Road trip", "{made}");
    assert_eq!(playlist["owner"], "Family", "{made}");
    assert_eq!(playlist["octoFamily"], true, "{made}");
    assert_eq!(playlist["octoFamilyCreatedBy"], "alex", "{made}");
    assert_eq!(playlist["songCount"], 0, "{made}");
    // The maker is the caller, the store holds it, and nothing reached Navidrome.
    let stored = f.state.family_playlists.get(&id).expect("there");
    assert_eq!(stored.name, "Road trip");
    assert_eq!(stored.created_by, "alex");
    assert!(
        !received_paths(&f._server)
            .await
            .iter()
            .any(|p| p.contains("createPlaylist")),
        "Navidrome never saw the create"
    );
    // Another member's playlist list carries it at once.
    let sam = f.json("sam", "getPlaylists").await;
    let listed = row(&sam, &id).unwrap_or_else(|| panic!("sam sees it: {sam}"));
    assert_eq!(listed["owner"], "Family");
}

#[tokio::test]
async fn a_prefixed_create_takes_the_songs_asked_for_at_the_making() {
    let f = prefixed("#family").await;
    let made = f
        .json(
            "alex",
            &format!("createPlaylist?name=%23family%20Road%20trip&songId=s-clean&songId={OUTSIDE}"),
        )
        .await;
    assert_eq!(made["status"], "ok", "{made}");
    // The call's order is the playlist's order: the library song and the outside song alike.
    assert_eq!(ids(&made["playlist"]["entry"]), ["s-clean", OUTSIDE], "{made}");
    assert_eq!(made["playlist"]["songCount"], 2, "{made}");
    let id = made["playlist"]["id"].as_str().expect("an id");
    let served = f.json("sam", &format!("getPlaylist?id={id}")).await;
    assert_eq!(
        ids(&served["playlist"]["entry"]),
        ["s-clean", OUTSIDE],
        "{served}"
    );
}

#[tokio::test]
async fn a_prefixed_create_without_a_name_after_the_prefix_is_refused() {
    let f = prefixed("#family").await;
    for name in ["%23family", "%23family%20%20"] {
        let answer = f.json("alex", &format!("createPlaylist?name={name}")).await;
        assert_eq!(answer["status"], "failed", "{answer}");
        assert_eq!(answer["error"]["code"], 10, "{answer}");
        assert!(
            answer["error"]["message"]
                .as_str()
                .is_some_and(|m| m.contains("#family")),
            "{answer}"
        );
    }
    assert!(f.state.family_playlists.list().is_empty(), "nothing was made");
}

#[tokio::test]
async fn a_prefixed_create_is_refused_outside_the_family_and_without_edit_rights() {
    let f = prefixed("#family").await;
    for user in ["guest", "kid"] {
        let answer = f.json(user, "createPlaylist?name=%23family%20Mine").await;
        assert_eq!(answer["status"], "failed", "{user}: {answer}");
        assert_eq!(answer["error"]["code"], 50, "{user}: {answer}");
    }
    assert!(f.state.family_playlists.list().is_empty(), "nothing was made");
}

#[tokio::test]
async fn a_create_without_the_prefix_or_with_the_prefix_unset_is_navidromes_own() {
    // The prefix unset: nothing intercepts, whatever the name.
    let f = fixture(true).await;
    let answer = f
        .json("alex", "createPlaylist?name=%23family%20Road%20trip")
        .await;
    assert_eq!(answer["status"], "ok", "{answer}");
    assert!(
        received_paths(&f._server)
            .await
            .iter()
            .any(|p| p.contains("createPlaylist")),
        "the create was relayed"
    );
    assert!(f.state.family_playlists.list().is_empty());
    // The prefix set, but the name does not start with it.
    let f = prefixed("#family").await;
    let answer = f.json("alex", "createPlaylist?name=Road%20trip").await;
    assert_eq!(answer["status"], "ok", "{answer}");
    assert!(f.state.family_playlists.list().is_empty());
}

#[tokio::test]
async fn only_a_founding_create_is_captured_never_an_edit() {
    let f = prefixed("#family").await;
    // A rename of a plain playlist to the prefixed name: Navidrome's own answer (error 70
    // here), and no family playlist appears.
    let renamed = f
        .json("alex", "updatePlaylist?playlistId=p-own&name=%23family%20Renamed")
        .await;
    assert_eq!(renamed["error"]["code"], 70, "{renamed}");
    assert!(f.state.family_playlists.list().is_empty());
    // createPlaylist WITH a playlistId replaces an existing playlist's songs — an edit, not a
    // birth — so it is relayed too.
    let replaced = f
        .json(
            "alex",
            "createPlaylist?playlistId=p-own&name=%23family%20Again&songId=s-clean",
        )
        .await;
    assert_eq!(replaced["status"], "ok", "{replaced}");
    assert!(f.state.family_playlists.list().is_empty());
}

#[tokio::test]
async fn of_ids_are_relayed_when_family_off() {
    let f = fixture(false).await;
    let owner = FamilyCaller::unmanaged("winters", true);
    let made = family_playlists::create(&f.state.family_playlists, &owner, "Road trip", false, Utc::now())
        .expect("made");
    let answer = f.json("winters", &format!("getPlaylist?id={}", made.id)).await;
    assert_eq!(
        answer["error"]["message"], "Playlist not found",
        "Navidrome's answer: {answer}"
    );
    let answer = f
        .json(
            "winters",
            &format!("updatePlaylist?playlistId={}&songIdToAdd=s-clean", made.id),
        )
        .await;
    assert_eq!(answer["error"]["code"], 70, "{answer}");
    assert!(f.entries(&made.id).is_empty());
}

#[tokio::test]
async fn rows_carry_family_rights() {
    let f = fixture(true).await;
    let made = family_playlists::create(
        &f.state.family_playlists,
        &f.caller("alex"),
        "Alex's trip",
        false,
        Utc::now(),
    )
    .expect("made");
    let id = made.id;
    for (user, may_rename, may_delete) in [
        ("alex", true, false),
        ("sam", false, false),
        ("winters", true, true),
    ] {
        let answer = f.json(user, "getPlaylists").await;
        let listed = row(&answer, &id).unwrap_or_else(|| panic!("{user} sees it: {answer}"));
        assert_eq!(listed["octoFamily"], true, "{user}");
        assert_eq!(listed["octoFamilyCreatedBy"], "alex", "{user}");
        assert_eq!(listed["octoFamilyMayRename"], may_rename, "{user}");
        assert_eq!(listed["octoFamilyMayDelete"], may_delete, "{user}");
        let one = f.json(user, &format!("getPlaylist?id={id}")).await;
        assert_eq!(one["playlist"]["octoFamily"], true, "{user}: {one}");
        assert_eq!(one["playlist"]["octoFamilyCreatedBy"], "alex", "{user}");
        assert_eq!(one["playlist"]["octoFamilyMayRename"], may_rename, "{user}");
        assert_eq!(one["playlist"]["octoFamilyMayDelete"], may_delete, "{user}");
    }
    let xml = f.get("alex", "getPlaylists").await.text();
    for attribute in [
        "octoFamily=\"true\"",
        "octoFamilyCreatedBy=\"alex\"",
        "octoFamilyMayRename=\"true\"",
        "octoFamilyMayDelete=\"false\"",
    ] {
        assert!(xml.contains(attribute), "{attribute} in {xml}");
    }
    let xml = f.get("alex", &format!("getPlaylist?id={id}")).await.text();
    assert!(xml.contains("octoFamilyMayRename=\"true\""), "{xml}");
}

#[tokio::test]
async fn removal_by_song_takes_that_song() {
    let f = fixture(true).await;
    let id = f.playlist(
        false,
        vec![
            library("s-clean", SHARED_SCOPE, None),
            library("s-unrated", SHARED_SCOPE, None),
            library("s-clean", SHARED_SCOPE, None),
        ],
    );
    let answer = f
        .json(
            "alex",
            &format!("updatePlaylist?playlistId={id}&octoSongToRemove=s-clean@2"),
        )
        .await;
    assert_eq!(answer["status"], "ok", "{answer}");
    assert_eq!(
        left_ids(&f, &id),
        [Some("s-clean".to_string()), Some("s-unrated".to_string())]
    );
    let answer = f
        .json(
            "alex",
            &format!("updatePlaylist?playlistId={id}&octoSongToRemove=zzz@1"),
        )
        .await;
    assert_eq!(answer["status"], "ok", "{answer}");
    assert_eq!(f.entries(&id).len(), 2);
}

#[tokio::test]
async fn removal_by_song_reads_no_navidrome_and_ignores_unreadable_entries() {
    let f = fixture(true).await;
    let id = f.playlist(
        false,
        vec![
            library("s-missing", SHARED_SCOPE, None),
            outside(OUTSIDE),
            library("s-clean", SHARED_SCOPE, None),
        ],
    );
    let answer = f
        .json(
            "alex",
            &format!("updatePlaylist?playlistId={id}&octoSongToRemove=s-clean@1"),
        )
        .await;
    assert_eq!(answer["status"], "ok", "{answer}");
    assert_eq!(
        left_ids(&f, &id),
        [Some("s-missing".to_string()), Some(OUTSIDE.to_string())]
    );
}

#[tokio::test]
async fn removal_by_song_works_for_clean_only_editors() {
    let f = fixture(true).await;
    let mut editor = f.state.family_store.member("kid").expect("a member");
    editor.edits.insert("familyPlaylistsEdit".into(), json!(true));
    f.state.family_store.upsert_member(editor);
    let id = f.playlist(
        false,
        vec![
            outside(OUTSIDE_EXPLICIT),
            outside(OUTSIDE),
            library("s-clean", SHARED_SCOPE, None),
        ],
    );
    let answer = f
        .json(
            "kid",
            &format!("updatePlaylist?playlistId={id}&octoSongToRemove={OUTSIDE}@1"),
        )
        .await;
    assert_eq!(answer["status"], "ok", "{answer}");
    assert_eq!(
        left_ids(&f, &id),
        [Some(OUTSIDE_EXPLICIT.to_string()), Some("s-clean".to_string())]
    );
}

#[tokio::test]
async fn both_removal_kinds_refused() {
    let f = fixture(true).await;
    let id = f.playlist(
        false,
        vec![outside(OUTSIDE), library("s-clean", SHARED_SCOPE, None)],
    );
    let answer = f
        .json(
            "alex",
            &format!("updatePlaylist?playlistId={id}&octoSongToRemove=s-clean@1&songIndexToRemove=0"),
        )
        .await;
    assert_eq!(answer["status"], "failed", "{answer}");
    assert_eq!(answer["error"]["code"], 10, "{answer}");
    assert_eq!(
        answer["error"]["message"],
        "Use octoSongToRemove or songIndexToRemove, not both."
    );
    assert_eq!(f.entries(&id).len(), 2);
}

#[tokio::test]
async fn rename_moves_the_row_change_time() {
    let f = fixture(true).await;
    let id = f.playlist(false, vec![outside(OUTSIDE)]);
    let changed = |answer: &Value| row(answer, &id).expect("listed")["changed"].clone();
    let before = changed(&f.json("alex", "getPlaylists").await);
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let renamed = f
        .json("winters", &format!("updatePlaylist?playlistId={id}&name=Summer"))
        .await;
    assert_eq!(renamed["status"], "ok", "{renamed}");
    let after = changed(&f.json("alex", "getPlaylists").await);
    assert!(
        after.as_str() > before.as_str(),
        "changed moved from {before} to {after}"
    );
}

#[tokio::test]
async fn extension_listed_with_family_on() {
    for on in [true, false] {
        let f = fixture(on).await;
        let answer = f.json("alex", "getOpenSubsonicExtensions").await;
        let listed = answer["openSubsonicExtensions"]
            .as_array()
            .map(|list| {
                list.iter()
                    .any(|item| item["name"] == "octoFamilyPlaylists" && item["versions"] == json!([1]))
            })
            .unwrap_or(false);
        assert_eq!(listed, on, "{answer}");
    }
}

// ---- kid profiles ----

#[tokio::test]
async fn search3_json_drops_explicit() {
    let f = fixture(true).await;
    let kid = f.json("kid", "search3?query=").await;
    let result = &kid["searchResult3"];
    assert_eq!(
        ids(&result["song"]),
        ["s-clean", "s-unrated", "s-alex", "s-alex-only"],
        "{kid}"
    );
    // Albums and artists are left alone.
    assert_eq!(ids(&result["album"]).len(), 2);
    assert_eq!(ids(&result["artist"]).len(), 2);
}

#[tokio::test]
async fn kid_jsonp_answer_is_filtered() {
    let f = fixture(true).await;
    let body = f.get("kid", "search3?query=&f=jsonp&callback=cb").await.text();
    assert!(body.starts_with("cb("), "{body}");
    assert!(!body.contains("s-explicit"), "{body}");
    assert!(body.contains("s-clean"), "{body}");
}

#[tokio::test]
async fn get_album_xml_drops_explicit_children() {
    let f = fixture(true).await;
    let xml = f.get("kid", "getAlbum?id=al-shared").await.text();
    assert!(!xml.contains("s-explicit"), "{xml}");
    assert!(xml.contains("id=\"s-clean\""), "{xml}");
    assert!(xml.contains("songCount=\"2\""), "{xml}");
    assert!(xml.contains("duration=\"450\""), "{xml}");
}

#[tokio::test]
async fn legacy_client_without_explicit_status_still_filtered() {
    let f = fixture(true).await;
    let legacy = f
        .get("kid", "getAlbum?id=al-shared&c=DSub&f=json")
        .await
        .envelope();
    assert_eq!(
        ids(&legacy["album"]["song"]),
        ["s-clean", "s-unrated"],
        "{legacy}"
    );
    let xml = f.get("kid", "getAlbum?id=al-shared&c=DSub").await.text();
    assert!(!xml.contains("s-explicit"), "{xml}");
}

#[tokio::test]
async fn unrated_kept_by_default() {
    let f = fixture(true).await;
    let kid = f.json("kid", "getAlbum?id=al-shared").await;
    assert!(ids(&kid["album"]["song"]).contains(&"s-unrated".to_string()));
    let strict = fixture_with(true, |family| family.kid_unrated_is_explicit = true).await;
    let kid = strict.json("kid", "getAlbum?id=al-shared").await;
    assert_eq!(ids(&kid["album"]["song"]), ["s-clean"], "{kid}");
}

#[tokio::test]
async fn song_count_follows() {
    let f = fixture(true).await;
    let kid = f.json("kid", "getAlbum?id=al-shared").await;
    assert_eq!(kid["album"]["songCount"], 2, "{kid}");
    assert_eq!(kid["album"]["duration"], 450, "{kid}");
    let id = f.playlist(
        false,
        vec![
            library("s-clean", SHARED_SCOPE, None),
            library("s-explicit", SHARED_SCOPE, None),
            outside(OUTSIDE_EXPLICIT),
            outside(OUTSIDE),
        ],
    );
    let list = f.json("kid", &format!("getPlaylist?id={id}")).await;
    assert_eq!(ids(&list["playlist"]["entry"]), ["s-clean", OUTSIDE], "{list}");
    assert_eq!(list["playlist"]["songCount"], 2);
    let adult = f.json("sam", &format!("getPlaylist?id={id}")).await;
    assert_eq!(adult["playlist"]["songCount"], 4);
}

#[tokio::test]
async fn kid_get_song_of_explicit_song_is_not_found() {
    let f = fixture(true).await;
    let refused = f.json("kid", "getSong?id=s-explicit").await;
    assert_eq!(refused["error"]["code"], 70, "{refused}");
    let xml = f.get("kid", "getSong?id=s-explicit").await.text();
    assert!(xml.contains("code=\"70\""), "{xml}");
    let fine = f.json("kid", "getSong?id=s-clean").await;
    assert_eq!(fine["song"]["id"], "s-clean", "{fine}");
    let adult = f.json("sam", "getSong?id=s-explicit").await;
    assert_eq!(adult["song"]["id"], "s-explicit", "{adult}");
}

#[tokio::test]
async fn adult_unchanged() {
    let f = fixture(true).await;
    for user in ["sam", "alex", "winters"] {
        let random = f.json(user, "getRandomSongs").await;
        assert!(
            ids(&random["randomSongs"]["song"]).contains(&"s-explicit".to_string()),
            "{user}"
        );
        let album = f.json(user, "getAlbum?id=al-shared").await;
        assert_eq!(album["album"]["songCount"], 3, "{user}");
    }
}

#[tokio::test]
async fn unrated_count_counts_shared_songs_only() {
    let f = fixture(true).await;
    assert_eq!(
        kid_filter::unrated_count(&f.state.library_index, &f.state.library_scope),
        1,
        "s-unrated; members' own songs are not counted"
    );
}

// ---- the owner's privacy ----

#[tokio::test]
async fn owner_folders_hide_personal() {
    let f = fixture(true).await;
    let owner = f.json("winters", "getMusicFolders").await;
    assert_eq!(ids(&owner["musicFolders"]["musicFolder"]), ["1"], "{owner}");
    let sees = fixture_with(true, |family| family.owner_sees_member_libraries = true).await;
    let owner = sees.json("winters", "getMusicFolders").await;
    assert_eq!(ids(&owner["musicFolders"]["musicFolder"]).len(), 4, "{owner}");
}

#[tokio::test]
async fn owner_artist_page_hides_member_albums() {
    let f = fixture(true).await;
    let artist = f.json("winters", "getArtist?id=ar-shared").await;
    assert_eq!(ids(&artist["artist"]["album"]), ["al-shared"], "{artist}");
    assert_eq!(artist["artist"]["albumCount"], 1, "{artist}");
    let artists = f.json("winters", "getArtists").await;
    let index = &artists["artists"]["index"];
    assert_eq!(index.as_array().map(Vec::len), Some(1), "{artists}");
    assert_eq!(ids(&index[0]["artist"]), ["ar-shared"]);
    let search = f.json("winters", "search3?query=").await;
    assert_eq!(
        ids(&search["searchResult3"]["song"]),
        ["s-clean", "s-explicit", "s-unrated"],
        "{search}"
    );
    assert_eq!(ids(&search["searchResult3"]["album"]), ["al-shared"]);
    assert_eq!(ids(&search["searchResult3"]["artist"]), ["ar-shared"]);
    // A member is never filtered this way: Navidrome shows them their own library already.
    let alex = f.json("alex", "getArtist?id=ar-shared").await;
    assert_eq!(ids(&alex["artist"]["album"]).len(), 2, "{alex}");
}

#[tokio::test]
async fn owner_playlists_hide_members() {
    let f = fixture(true).await;
    let id = f.playlist(false, Vec::new());
    let owner = f.json("winters", "getPlaylists").await;
    let shown = ids(&owner["playlists"]["playlist"]);
    assert!(shown.contains(&"p-own".to_string()), "{owner}");
    assert!(!shown.contains(&"p-alex".to_string()), "{owner}");
    assert!(shown.contains(&id), "family playlists stay: {owner}");
    let sees = fixture_with(true, |family| family.owner_sees_member_playlists = true).await;
    let owner = sees.json("winters", "getPlaylists").await;
    assert!(ids(&owner["playlists"]["playlist"]).contains(&"p-alex".to_string()));
}

// ---- members' own songs ----

#[tokio::test]
async fn personal_songs_are_marked_json_and_xml() {
    let f = fixture(true).await;
    let alex = f.json("alex", "getRandomSongs").await;
    let songs = alex["randomSongs"]["song"].as_array().expect("songs").clone();
    let mine = songs.iter().find(|s| s["id"] == "s-alex").expect("there");
    assert_eq!(mine["octoPersonal"], true, "{alex}");
    let xml = f.get("alex", "getRandomSongs").await.text();
    assert!(xml.contains("octoPersonal=\"true\""), "{xml}");
    let song = f.json("alex", "getSong?id=s-alex").await;
    assert_eq!(song["song"]["octoPersonal"], true, "{song}");
}

#[tokio::test]
async fn shared_songs_unmarked() {
    let f = fixture(true).await;
    let alex = f.json("alex", "getRandomSongs").await;
    for song in alex["randomSongs"]["song"].as_array().expect("songs") {
        if song["id"] != "s-alex" {
            assert!(song.get("octoPersonal").is_none(), "{song}");
        }
    }
    // Nobody else's answers carry marks, not even for alex's songs.
    for user in ["winters", "sam"] {
        let body = f.get(user, "getRandomSongs?f=json").await.text();
        assert!(!body.contains("octoPersonal"), "{user}: {body}");
    }
}

// ---- lyrics pins ----

#[tokio::test]
async fn lyrics_choice_needs_the_member_preset() {
    let f = fixture(true).await;
    for user in ["sam", "kid"] {
        let refused = f
            .get(user, "setLyricsChoice?id=s-clean&candidate=none")
            .await
            .envelope();
        assert_eq!(refused["error"]["code"], 50, "{user}: {refused}");
    }
    for user in ["alex", "winters"] {
        let answer = f
            .get(user, "setLyricsChoice?id=s-clean&candidate=none")
            .await
            .text();
        assert!(!answer.contains("\"code\":50"), "{user}: {answer}");
    }
}

// ---- Family off ----

#[tokio::test]
async fn family_off_changes_no_answer() {
    let f = fixture(false).await;
    for user in ["kid", "winters"] {
        let search = f.json(user, "search3?query=").await;
        assert_eq!(ids(&search["searchResult3"]["song"]).len(), 5, "{user}: {search}");
        let album = f.json(user, "getAlbum?id=al-shared").await;
        assert_eq!(album["album"]["songCount"], 3, "{user}");
        let artist = f.json(user, "getArtist?id=ar-shared").await;
        assert_eq!(ids(&artist["artist"]["album"]).len(), 2, "{user}");
        let song = f.json(user, "getSong?id=s-explicit").await;
        assert_eq!(song["song"]["id"], "s-explicit", "{user}");
    }
    // Relayed answers go out byte for byte.
    for endpoint in ["getMusicFolders", "getArtists", "getMusicFolders?f=json"] {
        let body = f.get("winters", endpoint).await.text();
        assert!(!body.contains("octoPersonal"));
        assert!(
            body.contains("Alex") || body.contains("ar-alex"),
            "{endpoint}: {body}"
        );
    }
    let random = f.get("alex", "getRandomSongs?f=json").await.text();
    assert!(!random.contains("octoPersonal"), "{random}");
}

// ---- Genres, playlist lengths and playlist additions ----

#[tokio::test]
async fn owner_genres_leave_out_members_own_songs() {
    let f = fixture(true).await;
    let owner = f.json("winters", "getGenres").await;
    assert_eq!(
        owner["genres"]["genre"],
        json!([{"value": "Rock", "songCount": 3, "albumCount": 1}]),
        "{owner}"
    );
    let xml = f.get("winters", "getGenres").await.text();
    assert!(
        xml.contains(r#"songCount="3""#) && xml.contains(r#"albumCount="1""#),
        "{xml}"
    );
    assert!(!xml.contains("Pop"), "{xml}");
    // Members' answers are Navidrome's own (their libraries are theirs).
    let alex = f.json("alex", "getGenres").await;
    assert_eq!(alex["genres"]["genre"][0]["songCount"], 5, "{alex}");
    assert_eq!(alex["genres"]["genre"][1]["value"], "Pop", "{alex}");
    // An owner who chose to see members' music sees every count.
    let sees = fixture_with(true, |family| family.owner_sees_member_libraries = true).await;
    let all = sees.json("winters", "getGenres").await;
    assert_eq!(all["genres"]["genre"][0]["songCount"], 5, "{all}");
}

#[tokio::test]
async fn family_playlist_rows_carry_their_length_and_kids_lose_explicit_outside_songs() {
    let f = fixture(true).await;
    let id = f.playlist(
        false,
        vec![
            library("s-clean", SHARED_SCOPE, None),
            library("s-explicit", SHARED_SCOPE, None),
            outside(OUTSIDE),
            outside(OUTSIDE_EXPLICIT),
        ],
    );
    let owner = f.json("winters", "getPlaylists").await;
    let row_of = |answer: &Value| row(answer, &id).cloned().expect("listed");
    let listed = row_of(&owner);
    assert_eq!(listed["songCount"], 4, "{listed}");
    // 200 and 150 seconds from the library index, 180 each from the catalog.
    assert_eq!(listed["duration"], 710, "{listed}");
    let kid = row_of(&f.json("kid", "getPlaylists").await);
    assert_eq!(kid["songCount"], 2, "{kid}");
    assert_eq!(kid["duration"], 380, "{kid}");
    // The playlist itself says the same as its row, for the kid and for everyone else.
    let kid_list = f.json("kid", &format!("getPlaylist?id={id}")).await;
    assert_eq!(kid_list["playlist"]["songCount"], 2, "{kid_list}");
    assert_eq!(kid_list["playlist"]["duration"], 380, "{kid_list}");
    let sam_list = f.json("sam", &format!("getPlaylist?id={id}")).await;
    assert_eq!(sam_list["playlist"]["duration"], 710, "{sam_list}");
}

#[tokio::test]
async fn an_outside_song_added_keeps_its_length_and_mark() {
    let f = fixture(true).await;
    let id = f.playlist(false, Vec::new());
    let added = f
        .json(
            "alex",
            &format!("updatePlaylist?playlistId={id}&songIdToAdd={OUTSIDE_EXPLICIT}"),
        )
        .await;
    assert_eq!(added["status"], "ok", "{added}");
    let entry = f.entries(&id).pop().expect("added");
    assert_eq!(entry.duration, Some(180));
    assert_eq!(entry.explicit_status.as_deref(), Some("explicit"));
}

#[tokio::test]
async fn a_held_addition_is_a_request_approving_adds_it_and_declining_drops_it() {
    let f = fixture(true).await;
    let id = f.playlist(true, Vec::new());
    let add = |song: &'static str| {
        let id = id.clone();
        let f = &f;
        async move {
            f.json(
                "sam",
                &format!("updatePlaylist?playlistId={id}&songIdToAdd={song}"),
            )
            .await
        }
    };
    assert_eq!(add(OUTSIDE).await["status"], "ok");
    let entries = f.entries(&id);
    assert_eq!(entries.len(), 1);
    assert!(entries[0].pending, "a listener's addition waits");
    let requests = &f.state.family_requests;
    let titled = crate::services::family::test_support::eventually(|| {
        requests
            .for_user("sam")
            .iter()
            .any(|r| r.title == "Outside" && r.artist == "Somebody")
    })
    .await;
    assert!(titled, "{:?}", requests.for_user("sam"));
    let asked = requests.for_user("sam").pop().expect("a request");
    assert_eq!(asked.kind, octo_core::family::request::RequestKind::PlaylistAdd);
    assert_eq!(asked.state, octo_core::family::request::RequestState::Pending);
    assert_eq!(asked.note, "For the family playlist Road trip");
    assert_eq!(
        requests.quota_used("sam", Utc::now()),
        0,
        "an addition takes no slot"
    );

    // Approving puts the song in the playlist for everyone.
    f.state
        .family_request_service
        .approve(&asked.id, "winters", None)
        .expect("approved");
    let added = crate::services::family::test_support::eventually(|| {
        f.entries(&id).first().is_some_and(|e| !e.pending)
            && requests
                .get(&asked.id)
                .is_some_and(|r| r.state == octo_core::family::request::RequestState::Done)
    })
    .await;
    assert!(added, "{:?} {:?}", f.entries(&id), requests.get(&asked.id));
    let alex = f.json("alex", &format!("getPlaylist?id={id}")).await;
    assert_eq!(ids(&alex["playlist"]["entry"]), [OUTSIDE], "{alex}");

    // Declining takes the song back out.
    assert_eq!(add(OUTSIDE_EXPLICIT).await["status"], "ok");
    assert_eq!(f.entries(&id).len(), 2);
    let second = crate::services::family::test_support::eventually(|| {
        requests
            .for_user("sam")
            .iter()
            .any(|r| r.state == octo_core::family::request::RequestState::Pending)
    })
    .await;
    assert!(second);
    let waiting = requests
        .for_user("sam")
        .into_iter()
        .find(|r| r.state == octo_core::family::request::RequestState::Pending)
        .expect("waiting");
    f.state
        .family_request_service
        .decline(&waiting.id, "winters", Some("Not this one"))
        .expect("declined");
    assert_eq!(
        f.entries(&id)
            .iter()
            .map(|e| e.outside_id.clone())
            .collect::<Vec<_>>(),
        [Some(OUTSIDE.to_string())]
    );
}

// ---- radio stations ----

/// A family member gets the songs of their own stations only: the owner's station, asked for by
/// its id, is not there for them.
#[tokio::test]
async fn a_member_gets_songs_of_their_own_stations_only() {
    use octo_core::last_fm::last_fm_radio_state_store::station_id;
    use octo_core::models::radio::{LastFmRadioStation, LastFmRadioStationKind, LastFmRadioTrack};

    let f = fixture(true).await;
    for owner in ["winters", "alex"] {
        f.state.last_fm_radio_state.replace_stations(
            owner,
            &[LastFmRadioStation {
                id: station_id(owner, "your-mix"),
                key: "your-mix".into(),
                name: "Your Radio".into(),
                owner: owner.into(),
                kind: LastFmRadioStationKind::YourMix,
                personalized: true,
                tracks: vec![LastFmRadioTrack {
                    artist: "ar-shared".into(),
                    title: "s-clean".into(),
                    duration: Some(200),
                    ..Default::default()
                }],
                ..Default::default()
            }],
        );
    }
    let ask = |user: &'static str, owner: &str| {
        let rest = format!("getStationSongs?id={}&count=1", station_id(owner, "your-mix"));
        let f = &f;
        async move { f.json(user, &rest).await }
    };

    let own = ask("alex", "alex").await;
    assert_eq!(own["status"], "ok", "{own}");
    assert_eq!(
        own["stationSongs"]["entry"].as_array().map(Vec::len),
        Some(1),
        "{own}"
    );
    let owners = ask("alex", "winters").await;
    assert_eq!(owners["status"], "failed", "{owners}");
    assert_eq!(owners["error"]["code"], 70, "{owners}");
    let theirs = ask("winters", "winters").await;
    assert_eq!(theirs["status"], "ok", "{theirs}");
    assert_eq!(ask("winters", "alex").await["error"]["code"], 70);
}
