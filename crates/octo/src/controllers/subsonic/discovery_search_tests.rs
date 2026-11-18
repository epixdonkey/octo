//! The discovery labels through the assembled pipeline: `rest/search3` and the native
//! `/api/song?title=...` search both answer with rows that say which service suggested them
//! (`octoSuggestedBy`), the video upload ahead of the song release of the same recording.

use axum::http::StatusCode;
use octo_core::settings::{AppSettings, SubsonicSettings};
use wiremock::matchers::{any, path, query_param};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

use super::route_tests::{Reply, app, get};
use crate::app::{AppState, TestServices};
use crate::http::pipeline::App;

/// Navidrome with an empty library: the local half of either search shape is empty, so the
/// page is discovery's alone. Deezer is pointed here too — it reads nothing it can parse,
/// so no test reaches the network.
#[derive(Clone, Default)]
struct EmptyLibrary;

impl Respond for EmptyLibrary {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let path = request.url.path();
        if path.starts_with("/rest/search") {
            return ResponseTemplate::new(200).set_body_string(
                r#"{"subsonic-response":{"status":"ok","version":"1.16.1","searchResult3":{}}}"#,
            );
        }
        if path == "/api/song" {
            return ResponseTemplate::new(200)
                .set_body_string("[]")
                .insert_header("X-Total-Count", "0");
        }
        ResponseTemplate::new(404)
    }
}

/// The shim's YouTube Music answer for "K3KHA - Lost in Sound": the videos filter has the
/// upload the query means, the songs catalog the release of the same recording (the measured
/// live shape) and an unrelated row each.
async fn you_tube_music(shim: &MockServer) {
    Mock::given(path("/ytm/search"))
        .and(query_param("filter", "songs"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"tracks":[
                {"videoId":"rwqD5PrajEA","title":"Lost in Sound","artists":["K3KHA"],"durationSeconds":619,"album":"Weightless Orbit"},
                {"videoId":"jenks111111","title":"Lost In The Sound","artists":["Jenks (UK)"],"durationSeconds":360}
            ]}"#,
        ))
        .mount(shim)
        .await;
    Mock::given(path("/ytm/search"))
        .and(query_param("filter", "videos"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"tracks":[
                {"videoId":"vBvDiLKTDWA","title":"Lost in Sound","artists":["K3KHA"],"durationSeconds":619},
                {"videoId":"R-JnkBpJuNs","title":"K3KHA – Deep Minimal Techno Set 2025","artists":["K3KHA"],"durationSeconds":3403}
            ]}"#,
        ))
        .mount(shim)
        .await;
}

/// The assembled app: Navidrome empty, the shim on a wiremock server, no Last.fm key (the
/// YouTube half answers on its own, D4).
async fn labeled_discovery() -> (MockServer, AppState, App) {
    let navidrome = MockServer::start().await;
    Mock::given(any())
        .respond_with(EmptyLibrary)
        .mount(&navidrome)
        .await;
    let shim = MockServer::start().await;
    you_tube_music(&shim).await;
    let state = AppState::for_tests_with(
        AppSettings {
            subsonic: SubsonicSettings {
                url: Some(navidrome.uri()),
                auto_detect_download_path: false,
                wait_for_search_durations: false,
                ..Default::default()
            },
            ..Default::default()
        },
        TestServices {
            deezer_base_url: Some(navidrome.uri()),
            raw_settings: vec![("YouTube:ShimUrl".into(), shim.uri())],
            ..Default::default()
        },
    );
    let app = app(state.clone());
    (shim, state, app)
}

/// The rows of a `search3` answer, with each row's `octoSuggestedBy`.
fn songs_of(reply: &Reply) -> Vec<serde_json::Value> {
    reply.json()["subsonic-response"]["searchResult3"]["song"]
        .as_array()
        .unwrap_or_else(|| panic!("the song rows: {}", reply.body))
        .clone()
}

/// `rest/search3` answers with the video upload and the song release of the query's own
/// recording as two rows, each labeled with its service, the video first (W3 through the
/// whole pipeline), and the video row is registered with the upload it names.
#[tokio::test]
async fn a_search3_answer_labels_its_discovery_rows_by_service() {
    let (_shim, state, app) = labeled_discovery().await;

    let reply = get(
        &app,
        "/rest/search3?query=K3KHA%20-%20Lost%20in%20Sound&songCount=20&songOffset=0\
         &albumCount=0&artistCount=0&u=tester&t=token&s=salt&v=1.16.1&c=test&f=json",
    )
    .await;

    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    let rows = songs_of(&reply);
    let label_of = |row: &serde_json::Value| row["octoSuggestedBy"].as_str().unwrap_or_default().to_string();
    let video = rows.iter().position(|r| label_of(r) == "YouTube Music (video)");
    let song = rows.iter().position(|r| label_of(r) == "YouTube Music (song)");
    let (video, song) = (video.expect("a video row"), song.expect("a song row"));
    assert!(video < song, "the upload the query means leads: {rows:?}");
    // The video row is the upload itself, registered with its video and its length.
    let video_id = rows[video]["id"].as_str().expect("an id");
    let routing = state
        .external_id_registry
        .lookup(video_id)
        .expect("registered")
        .snapshot();
    assert_eq!(routing.you_tube_id.as_deref(), Some("vBvDiLKTDWA"));
    assert_eq!(routing.duration, Some(619));
    // The origin rides the routing as its own label, so a playlist position added from the row
    // can keep its source after the answer is gone (the badge's durable bridge).
    assert_eq!(routing.suggested_by.as_deref(), Some("YouTube Music (video)"));
    assert_eq!(rows[video]["duration"], 619);
    // The song row is its own row beside it, the release's album on it.
    assert_eq!(rows[song]["album"], "Weightless Orbit");
}

/// The native `/api/song?title=...` search answers with the same labeled rows under the same
/// field name, the same order, from the same core (one build shared by both protocols).
#[tokio::test]
async fn a_native_song_search_labels_its_discovery_rows_by_service() {
    let (_shim, _state, app) = labeled_discovery().await;

    let reply = get(
        &app,
        "/api/song?title=K3KHA%20-%20Lost%20in%20Sound&_start=0&_end=50",
    )
    .await;

    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    let rows = reply.json().as_array().expect("a native array").clone();
    let label_of = |row: &serde_json::Value| row["octoSuggestedBy"].as_str().unwrap_or_default().to_string();
    let video = rows.iter().position(|r| label_of(r) == "YouTube Music (video)");
    let song = rows.iter().position(|r| label_of(r) == "YouTube Music (song)");
    let (video, song) = (video.expect("a video row"), song.expect("a song row"));
    assert!(video < song, "the upload the query means leads: {rows:?}");
    assert_eq!(rows[video]["duration"], 619);
    assert_eq!(rows[video]["suffix"], "m4a");
}
