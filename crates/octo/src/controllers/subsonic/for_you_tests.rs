//! Made for you lists through the app: getPlaylists marks each Made for you list in JSON and XML,
//! getPlaylist serves one, and its cover is an image. The stand-in Navidrome and Deezer are
//! `services::library::for_you_lists::tests`'. A list switched off in the dashboard is gone from
//! the very next getPlaylists.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::Method;
use octo_core::settings::{AppSettings, SettingsFileWriter, SettingsStore, SubsonicSettings};
use serde_json::Value;

use super::test_app::{app, get, send};
use crate::app::{AppState, TestServices};
use crate::http::pipeline::{App, app_routes, build_with};
use crate::http::static_files::StaticAssets;
use crate::services::library::for_you_lists::NewReleasesBuilder;
use crate::services::library::for_you_lists::tests::ForYouUpstream;
use crate::services::library::generated_playlist_service::{ForYouServices, GeneratedPlaylistService};
use crate::services::subsonic::SubsonicProxyService;

const AUTH: &str = "u=alice&t=token&s=salt&v=1.16.1&c=test";

#[tokio::test]
async fn get_playlists_marks_each_made_for_you_list_in_json_and_xml() {
    let upstream = ForYouUpstream::start().await;
    let state = AppState::for_tests_with(
        AppSettings {
            subsonic: SubsonicSettings {
                url: Some(upstream.navidrome_url()),
                auto_detect_download_path: false,
                ..Default::default()
            },
            ..Default::default()
        },
        TestServices {
            deezer_base_url: Some(upstream.deezer_url()),
            ..Default::default()
        },
    );
    let app = app(&state);

    let mut rows: Vec<Value> = Vec::new();
    for _ in 0..100 {
        let reply = get(&app, &format!("/rest/getPlaylists.view?{AUTH}&f=json")).await;
        let body = reply.json();
        rows = body["subsonic-response"]["playlists"]["playlist"]
            .as_array()
            .map(|rows| {
                rows.iter()
                    .filter(|row| row.get("octoList").is_some())
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        if rows.len() >= 3 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    assert_eq!(
        rows.iter()
            .map(|row| row["octoList"].as_str().unwrap_or_default())
            .collect::<Vec<_>>(),
        ["newReleases", "rediscover", "deepCuts"]
    );
    assert!(rows.iter().all(|row| row["readonly"] == true));
    let xml = get(&app, &format!("/rest/getPlaylists.view?{AUTH}")).await.text();
    assert!(xml.contains(r#"octoList="newReleases""#), "{xml}");

    let id = rows[0]["id"].as_str().expect("an id").to_string();
    let detail = get(&app, &format!("/rest/getPlaylist.view?{AUTH}&f=json&id={id}"))
        .await
        .json();
    let playlist = &detail["subsonic-response"]["playlist"];
    assert_eq!(playlist["octoList"], "newReleases");
    assert!(!playlist["entry"].as_array().expect("entries").is_empty());

    let cover = get(&app, &format!("/rest/getCoverArt.view?{AUTH}&f=json&id={id}")).await;
    assert!(cover.status.is_success(), "{}", cover.status);
    assert_eq!(cover.media_type().as_deref(), Some("image/jpeg"));
}

/// The Made for you lists in a getPlaylists answer, by kind.
async fn listed_kinds(app: &App) -> Vec<String> {
    let body = get(app, &format!("/rest/getPlaylists.view?{AUTH}&f=json"))
        .await
        .json();
    body["subsonic-response"]["playlists"]["playlist"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|row| row["octoList"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

async fn save(app: &App, method: Method, uri: &str, json: &str) {
    let reply = send(
        app,
        method,
        uri,
        &[("X-Octo-Admin", "1"), ("content-type", "application/json")],
        Body::from(json.to_string()),
    )
    .await;
    assert!(
        reply.status.is_success(),
        "{uri}: {} {}",
        reply.status,
        reply.text()
    );
}

/// The dashboard saves a list off and the app asks for its playlists at once. The settings file
/// is read as Octo reads /app/config/settings.json, but with no file watcher, so only the save
/// itself can make the running settings follow the file.
#[tokio::test]
async fn a_list_switched_off_in_the_dashboard_is_gone_from_the_very_next_get_playlists() {
    let upstream = ForYouUpstream::start().await;
    let directory = tempfile::TempDir::new().expect("a temp folder");
    let path = directory.path().join("settings.json");
    std::fs::write(&path, "{}").expect("the settings file");
    let env = [
        ("Subsonic__Url", upstream.navidrome_url()),
        ("Subsonic__AutoDetectDownloadPath", "false".to_string()),
        (
            "Library__DownloadPath",
            directory.path().to_string_lossy().into_owned(),
        ),
        ("Admin__SignIn", "off".to_string()),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_string(), value))
    .collect();
    let store = Arc::new(SettingsStore::from_env(env, Some(path.clone())));

    let mut state = AppState::for_tests_with(
        AppSettings {
            subsonic: SubsonicSettings {
                url: Some(upstream.navidrome_url()),
                auto_detect_download_path: false,
                ..Default::default()
            },
            ..Default::default()
        },
        TestServices {
            deezer_base_url: Some(upstream.deezer_url()),
            ..Default::default()
        },
    );
    {
        let inner = Arc::get_mut(&mut state.inner).expect("nothing else holds the test state yet");
        let lists = GeneratedPlaylistService::new(
            Some(directory.path().join("generated-playlists.json")),
            SubsonicProxyService::new(Arc::clone(&store)),
            Arc::clone(&store),
            Arc::new(crate::services::family::SavedStore::in_memory()),
        )
        .with_for_you(ForYouServices {
            builder: Arc::new(NewReleasesBuilder::new(
                upstream.deezer(),
                Arc::clone(&inner.external_id_registry),
            )),
            responses: Arc::clone(&inner.subsonic_response_builder),
        });
        inner.generated_playlists = Arc::new(lists);
        inner.settings = Arc::clone(&store);
        inner.settings_writer = Arc::new(SettingsFileWriter::new(&path));
    }
    let app = build_with(state.clone(), app_routes(&StaticAssets::default()));

    let mut kinds = Vec::new();
    for _ in 0..100 {
        kinds = listed_kinds(&app).await;
        if kinds.len() >= 3 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(kinds, ["newReleases", "rediscover", "deepCuts"]);

    // Each list, one save at a time, and the getPlaylists right after it.
    let mut left = vec!["newReleases", "rediscover", "deepCuts"];
    for (setting, kind) in [
        ("NewReleases", "newReleases"),
        ("Rediscover", "rediscover"),
        ("DeepCuts", "deepCuts"),
    ] {
        save(
            &app,
            Method::POST,
            "/api/admin/settings",
            &format!(r#"{{"GeneratedPlaylists":{{"{setting}":false}}}}"#),
        )
        .await;
        left.retain(|list| *list != kind);
        assert_eq!(listed_kinds(&app).await, left, "after {setting} was switched off");
    }

    // The settings that decide what goes in the lists follow a save as soon, and so does a
    // Raw config save.
    save(
        &app,
        Method::POST,
        "/api/admin/settings",
        r#"{"GeneratedPlaylists":{"NewReleaseWeeks":4,"NewReleaseArtists":20,"RediscoverMonths":12}}"#,
    )
    .await;
    let lists = state.settings.current().generated_playlists.clone();
    assert_eq!(
        (
            lists.new_release_weeks,
            lists.new_release_artists,
            lists.rediscover_months
        ),
        (4, 20, 12)
    );
    save(
        &app,
        Method::PUT,
        "/api/admin/raw-config",
        r#"{"GeneratedPlaylists":{"NewReleases":false,"Rediscover":true,"DeepCuts":false}}"#,
    )
    .await;
    let lists = state.settings.current().generated_playlists.clone();
    assert!(lists.rediscover && !lists.new_releases && !lists.deep_cuts);
}
