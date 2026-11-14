//! The live radio stations, hermetically: the YouTube streamUrl parser, the list rewrite,
//! and `/radio/live/{id}` itself (headers, passthrough, HEAD, 404, the shim's refusal, the
//! cap), over wiremock Navidrome and wiremock shim stands. Every fixture answers the same
//! station rows, so the process-wide station cache sharing entries between the tests of
//! this file is harmless; ids unique to a test keep the outcomes apart.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Method, StatusCode};
use http_body_util::BodyExt;
use tempfile::TempDir;
use tower::ServiceExt;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, Respond, ResponseTemplate};

use super::{you_tube_video_id, rewrite_json};
use crate::app::{AppState, TestServices};
use crate::controllers::subsonic::test_app::{Reply, send};
use crate::http::pipeline::{App, build_with};

const STATIONS_JSON: &str = r#"{"subsonic-response":{"status":"ok","version":"1.16.1","internetRadioStations":{"internetRadioStation":[
    {"id":"st-watch","name":"Lofi Watch","streamUrl":"https://www.youtube.com/watch?v=UYOb37KRFqk"},
    {"id":"st-be","name":"Lofi Be","streamUrl":"https://youtu.be/UYOb37KRFqk"},
    {"id":"st-live","name":"Lofi Live","streamUrl":"https://music.youtube.com/live/dQw4w9WgXcQ"},
    {"id":"st-plain","name":"Plain Radio","streamUrl":"https://plain.example/stream"},
    {"id":"st-short","name":"Shorts Only","streamUrl":"https://www.youtube.com/shorts/abcdefghijk"}
]}}}"#;

const STATIONS_XML: &str = r#"<subsonic-response xmlns="http://subsonic.org/restapi" status="ok" version="1.16.1"><internetRadioStations><internetRadioStation id="st-watch" name="Lofi Watch" streamUrl="https://www.youtube.com/watch?v=UYOb37KRFqk"/><internetRadioStation id="st-be" name="Lofi Be" streamUrl="https://youtu.be/UYOb37KRFqk"/><internetRadioStation id="st-live" name="Lofi Live" streamUrl="https://music.youtube.com/live/dQw4w9WgXcQ"/><internetRadioStation id="st-plain" name="Plain Radio" streamUrl="https://plain.example/stream"/><internetRadioStation id="st-short" name="Shorts Only" streamUrl="https://www.youtube.com/shorts/abcdefghijk"/></internetRadioStations></subsonic-response>"#;

const OK_JSON: &str = r#"{"subsonic-response":{"status":"ok","version":"1.16.1"}}"#;

fn query_value(request: &wiremock::Request, name: &str) -> Option<String> {
    request
        .url
        .query_pairs()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.into_owned())
}

/// The Navidrome stand-in: the station rows for `getInternetRadioStations` (JSON unless the
/// caller asked for XML), an empty ok envelope for everything the radio bootstrap relays.
struct NavidromeStations;

impl Respond for NavidromeStations {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        if request.url.path() != "/rest/getInternetRadioStations" {
            return ResponseTemplate::new(200).set_body_string(OK_JSON);
        }
        if query_value(request, "f").as_deref() == Some("xml") {
            return ResponseTemplate::new(200)
                .set_body_string(STATIONS_XML)
                .insert_header("content-type", "application/xml");
        }
        ResponseTemplate::new(200).set_body_string(STATIONS_JSON)
    }
}

/// The shim stand-in: `/live/stream` answering the same MP3 body, refusable and delayable.
/// The flags are shared, so the fixture's handle moves the mock the test holds.
#[derive(Clone, Default)]
struct ShimLive {
    refuse: std::sync::Arc<AtomicBool>,
    delay_ms: std::sync::Arc<AtomicU64>,
}

impl ShimLive {
    fn streaming() -> Self {
        ShimLive::default()
    }
}

impl Respond for ShimLive {
    fn respond(&self, _request: &wiremock::Request) -> ResponseTemplate {
        if self.refuse.load(Ordering::SeqCst) {
            return ResponseTemplate::new(503).set_body_string(r#"{"error":"busy"}"#);
        }
        let mut answer =
            ResponseTemplate::new(200).set_body_raw(b"live-mp3-bytes".to_vec(), "audio/mpeg");
        let delay = self.delay_ms.load(Ordering::SeqCst);
        if delay > 0 {
            answer = answer.set_delay(Duration::from_millis(delay));
        }
        answer
    }
}

struct LiveFixture {
    _dir: TempDir,
    _navidrome: MockServer,
    shim: MockServer,
    shim_live: ShimLive,
    app: App,
    state: AppState,
}

impl LiveFixture {
    async fn new() -> LiveFixture {
        let dir = TempDir::new().expect("temp dir");
        let navidrome = MockServer::builder().start().await;
        Mock::given(any())
            .respond_with(NavidromeStations)
            .mount(&navidrome)
            .await;
        let shim_live = ShimLive::streaming();
        let shim = MockServer::builder().start().await;
        Mock::given(any())
            .respond_with(shim_live.clone())
            .mount(&shim)
            .await;
        let settings = crate::controllers::subsonic::test_app::settings(&navidrome.uri());
        let state = AppState::for_tests_with(
            settings,
            TestServices {
                config_dir: Some(dir.path().join("config")),
                raw_settings: vec![("YouTube:ShimUrl".to_string(), shim.uri())],
                ..Default::default()
            },
        );
        let app = build_with(state.clone(), crate::controllers::subsonic::routes());
        LiveFixture {
            _dir: dir,
            _navidrome: navidrome,
            shim,
            shim_live,
            app,
            state,
        }
    }

    async fn get(&self, uri: &str) -> Reply {
        send(&self.app, Method::GET, uri, &[], Body::empty()).await
    }

    async fn head(&self, uri: &str) -> Reply {
        send(&self.app, Method::HEAD, uri, &[], Body::empty()).await
    }

    /// The `(name, value)` pairs of every query the shim stand-in received.
    async fn shim_queries(&self) -> Vec<Vec<(String, String)>> {
        self.shim
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .map(|request| {
                request
                    .url
                    .query_pairs()
                    .map(|(key, value)| (key.into_owned(), value.into_owned()))
                    .collect()
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------------------
// The parser
// ---------------------------------------------------------------------------------------

#[test]
fn the_parser_accepts_the_forms_a_station_can_be_saved_with() {
    for (url, id) in [
        ("https://www.youtube.com/watch?v=UYOb37KRFqk", "UYOb37KRFqk"),
        ("http://youtube.com/watch?v=dQw4w9WgXcQ&t=30", "dQw4w9WgXcQ"),
        ("https://m.youtube.com/watch?v=dQw4w9WgXcQ&list=xyz", "dQw4w9WgXcQ"),
        ("https://music.youtube.com/watch?v=dQw4w9WgXcQ", "dQw4w9WgXcQ"),
        ("youtube.com/watch?v=UYOb37KRFqk", "UYOb37KRFqk"),
        ("https://youtu.be/UYOb37KRFqk", "UYOb37KRFqk"),
        ("https://youtu.be/UYOb37KRFqk?t=42", "UYOb37KRFqk"),
        ("https://www.youtube.com/live/dQw4w9WgXcQ", "dQw4w9WgXcQ"),
        ("https://www.youtube.com/embed/UYOb37KRFqk", "UYOb37KRFqk"),
        ("https://www.youtube.com/watch/?v=UYOb37KRFqk", "UYOb37KRFqk"),
    ] {
        assert_eq!(you_tube_video_id(url).as_deref(), Some(id), "{url}");
    }
}

#[test]
fn the_parser_rejects_what_is_not_a_live_channel_url() {
    for url in [
        "https://plain.example/stream",
        // A short is a finite video, not a channel: deliberately out.
        "https://www.youtube.com/shorts/abcdefghijk",
        "https://www.youtube.com/watch?list=xyz",
        "https://www.youtube.com/watch?v=",
        "https://www.youtube.com/watch?v=tooshort",
        "https://www.youtube.com/watch?v=waytoolongforanid",
        "https://evilyoutube.com/watch?v=UYOb37KRFqk",
        "https://youtube.com/video/UYOb37KRFqk",
        "https://youtu.be/UYOb37KRFqk/extra",
        "https://youtu.be",
        // An Octo station's own URL is never mistaken for a YouTube one.
        "http://localhost/radio/stream/0123456789abcdef0123456789abcdef0123456789abcdef",
        "",
    ] {
        assert_eq!(you_tube_video_id(url), None, "{url}");
    }
}

#[test]
fn the_rewrite_leaves_every_row_but_the_you_tube_ones_alone() {
    let live_url = |id: &str| format!("http://localhost/radio/live/{id}");
    let body = concat!(
        r#"{"subsonic-response":{"status":"ok","internetRadioStations":{"internetRadioStation":["#,
        r#"{"id":"or123","name":"Your Radio","streamUrl":"http://localhost/radio/stream/0123456789abcdef0123456789abcdef0123456789abcdef"},"#,
        r#"{"id":"yt1","name":"Lofi","streamUrl":"https://youtu.be/UYOb37KRFqk"},"#,
        r#"{"id":"plain","name":"Plain","streamUrl":"https://plain.example/live"}]}}}"#
    );
    let rewritten = String::from_utf8(rewrite_json(body.as_bytes(), &live_url).expect("rewritten"))
        .expect("utf-8");
    let rows: serde_json::Value = serde_json::from_str(&rewritten).expect("json");
    let row = |id: &str| {
        rows["subsonic-response"]["internetRadioStations"]["internetRadioStation"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["id"] == id)
            .unwrap_or_else(|| panic!("row {id}: {rewritten}"))
            .clone()
    };
    assert_eq!(
        row("or123")["streamUrl"],
        "http://localhost/radio/stream/0123456789abcdef0123456789abcdef0123456789abcdef"
    );
    assert_eq!(row("yt1")["streamUrl"], "http://localhost/radio/live/yt1");
    assert_eq!(row("plain")["streamUrl"], "https://plain.example/live");
    assert_eq!(row("yt1")["name"], "Lofi");
}

// ---------------------------------------------------------------------------------------
// The list rewrite
// ---------------------------------------------------------------------------------------

#[tokio::test]
async fn the_station_list_rewrites_you_tube_rows_and_leaves_the_rest_alone() {
    for format in ["json", "xml"] {
        let fixture = LiveFixture::new().await;
        let reply = send(
            &fixture.app,
            Method::GET,
            &format!("/rest/getInternetRadioStations?u=alice&t=token&s=salt&f={format}"),
            &[("Host", "localhost")],
            Body::empty(),
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK, "{format}: {}", reply.text());
        let body = reply.text();
        for id in ["st-watch", "st-be", "st-live"] {
            let expected = format!("http://localhost/radio/live/{id}");
            assert!(
                body.contains(&expected),
                "{format}: {id} not rewritten: {body}"
            );
        }
        assert!(
            body.contains("https://plain.example/stream"),
            "{format}: the plain row changed: {body}"
        );
        assert!(
            body.contains("https://www.youtube.com/shorts/abcdefghijk"),
            "{format}: the shorts row changed: {body}"
        );
        assert!(body.contains("Lofi Watch"), "{format}: a name was lost: {body}");
        assert!(!body.contains("youtube.com/watch"), "{format}: {body}");
    }
}

// ---------------------------------------------------------------------------------------
// The endpoint
// ---------------------------------------------------------------------------------------

#[tokio::test]
async fn the_live_station_streams_the_shims_bytes_in_the_radio_shape() {
    let fixture = LiveFixture::new().await;
    let reply = fixture
        .get("/radio/live/st-watch?u=alice&t=token&s=salt")
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(reply.body, b"live-mp3-bytes".as_slice());
    assert_eq!(reply.header("content-type"), Some("audio/mpeg"));
    assert_eq!(reply.header("cache-control"), Some("no-store, no-transform"));
    assert_eq!(reply.header("accept-ranges"), Some("none"));
    assert_eq!(reply.header("icy-name"), Some("Lofi Watch"));
    assert_eq!(reply.header("icy-br"), Some("192"));
    assert!(reply.headers.get("content-length").is_none(), "{:?}", reply.headers);
    // The shim was asked for this station's video, at the radio's bitrate, under its name.
    assert_eq!(
        fixture.shim_queries().await,
        [[
            ("id".to_string(), "UYOb37KRFqk".to_string()),
            ("abr".to_string(), "192".to_string()),
            ("name".to_string(), "Lofi Watch".to_string()),
        ]],
    );
}

#[tokio::test]
async fn a_head_answers_the_headers_alone_without_touching_the_shim() {
    let fixture = LiveFixture::new().await;
    let reply = fixture
        .head("/radio/live/st-watch?u=alice&t=token&s=salt")
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.header("content-type"), Some("audio/mpeg"));
    assert_eq!(reply.header("icy-name"), Some("Lofi Watch"));
    assert_eq!(reply.header("icy-br"), Some("192"));
    assert_eq!(reply.body, b"".as_slice());
    assert_eq!(fixture.shim_queries().await, Vec::<Vec<(String, String)>>::new());
}

#[tokio::test]
async fn an_unknown_station_is_an_empty_404() {
    let fixture = LiveFixture::new().await;
    let reply = fixture.get("/radio/live/st-nope?u=alice&t=token&s=salt").await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert_eq!(reply.body, b"".as_slice());
    assert_eq!(fixture.shim_queries().await, Vec::<Vec<(String, String)>>::new());
}

#[tokio::test]
async fn a_refusing_shim_is_a_502_with_a_short_body() {
    let fixture = LiveFixture::new().await;
    fixture.shim_live.refuse.store(true, Ordering::SeqCst);
    let reply = fixture
        .get("/radio/live/st-watch?u=alice&t=token&s=salt")
        .await;
    assert_eq!(reply.status, StatusCode::BAD_GATEWAY);
    assert_eq!(reply.text(), "The live radio source could not be opened");
}

#[tokio::test]
async fn the_fifth_parallel_live_stream_is_refused_and_the_cap_frees_again() {
    let fixture = LiveFixture::new().await;
    // Four streams hold their slots while the shim stands silent; the fifth must not queue.
    fixture.shim_live.delay_ms.store(3_000, Ordering::SeqCst);
    let mut streams = Vec::new();
    for _ in 0..5 {
        let app = fixture.app.clone();
        streams.push(tokio::spawn(async move {
            let response = app
                .clone()
                .oneshot(
                    axum::http::Request::builder()
                        .method(Method::GET)
                        .uri("/radio/live/st-watch?u=alice&t=token&s=salt")
                        .body(Body::empty())
                        .expect("request"),
                )
                .await
                .expect("infallible");
            let status = response.status();
            let body = response.into_body().collect().await.expect("body");
            (status, body.to_bytes().len())
        }));
    }
    let mut statuses = Vec::new();
    for stream in streams {
        statuses.push(stream.await.expect("a stream").0);
    }
    statuses.sort();
    assert_eq!(
        statuses,
        [
            StatusCode::OK,
            StatusCode::OK,
            StatusCode::OK,
            StatusCode::OK,
            StatusCode::SERVICE_UNAVAILABLE
        ],
        "{statuses:?}"
    );
    // Once the listeners are gone the slots are theirs again.
    fixture.shim_live.delay_ms.store(0, Ordering::SeqCst);
    let again = fixture
        .get("/radio/live/st-watch?u=alice&t=token&s=salt")
        .await;
    assert_eq!(again.status, StatusCode::OK, "{}", again.text());
    assert_eq!(again.body, b"live-mp3-bytes".as_slice());
}
