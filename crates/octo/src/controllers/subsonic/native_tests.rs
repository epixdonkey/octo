//! The native song object's discovery badge: a row the search answered says which service it
//! came from, under the same name the Subsonic serializer gives it (`octoSuggestedBy`).

use octo_core::models::domain::Song;
use octo_core::settings::AppSettings;
use serde_json::json;

use super::*;
use crate::app::AppState;

/// A discovery row's service label rides the native object as `octoSuggestedBy` (the mirror
/// of the Subsonic serializer's field), and a row no service suggested carries no field at
/// all — an empty label least of all.
#[test]
fn a_native_song_object_carries_the_discovery_row_label() {
    let state = AppState::for_tests(AppSettings::default());

    let labeled = native_song_object(
        &state,
        &Song {
            id: "row-1".into(),
            title: "Lost in Sound".into(),
            artist: "K3KHA".into(),
            duration: Some(619),
            suggested_by: Some("YouTube Music (video)".into()),
            ..Default::default()
        },
    );
    assert_eq!(labeled["octoSuggestedBy"], "YouTube Music (video)");
    // The row's own shape is untouched beside it.
    assert_eq!(labeled["title"], "Lost in Sound");
    assert_eq!(labeled["duration"], 619);

    let plain = native_song_object(
        &state,
        &Song {
            id: "row-2".into(),
            title: "Song".into(),
            artist: "Artist".into(),
            ..Default::default()
        },
    );
    assert!(plain.get("octoSuggestedBy").is_none(), "{plain}");

    let empty = native_song_object(
        &state,
        &Song {
            id: "row-3".into(),
            title: "Song".into(),
            artist: "Artist".into(),
            suggested_by: Some(String::new()),
            ..Default::default()
        },
    );
    assert!(empty.get("octoSuggestedBy").is_none(), "{empty}");
}

/// The source badge (option A) rides the native object the same way it rides the Subsonic
/// row: each discovery origin badges as its own genre in `genre` and `genres`, the row's own
/// genre comes first with the source appended (never twice), and a library row or a radio
/// row gets none.
#[test]
fn a_native_song_object_carries_the_source_as_a_genre_badge() {
    let state = AppState::for_tests(AppSettings::default());

    for (label, badge) in [
        ("YouTube Music (song)", "YouTube Music"),
        ("YouTube Music (video)", "YouTube Video"),
        ("YouTube", "YouTube"),
    ] {
        let row = native_song_object(
            &state,
            &Song {
                id: "row-1".into(),
                title: "Lost in Sound".into(),
                artist: "K3KHA".into(),
                duration: Some(619),
                suggested_by: Some(label.into()),
                ..Default::default()
            },
        );
        assert_eq!(row["genre"], badge, "{label}: {row}");
        assert_eq!(row["genres"], json!([{ "name": badge }]), "{label}: {row}");
    }

    let techno = native_song_object(
        &state,
        &Song {
            id: "row-2".into(),
            title: "Lost in Sound".into(),
            artist: "K3KHA".into(),
            genre: Some("Techno".into()),
            suggested_by: Some("YouTube Music (song)".into()),
            ..Default::default()
        },
    );
    assert_eq!(techno["genre"], "Techno; YouTube Music");
    assert_eq!(
        techno["genres"],
        json!([{ "name": "Techno" }, { "name": "YouTube Music" }])
    );

    let library = native_song_object(
        &state,
        &Song {
            id: "row-3".into(),
            title: "Song".into(),
            artist: "Artist".into(),
            genre: Some("Techno".into()),
            is_local: true,
            suggested_by: Some("YouTube".into()),
            ..Default::default()
        },
    );
    assert_eq!(library["genre"], "Techno");
    assert!(library.get("genres").is_none(), "{library}");

    let radio = native_song_object(
        &state,
        &Song {
            id: "row-4".into(),
            title: "Song".into(),
            artist: "Artist".into(),
            suggested_by: Some("Last.fm".into()),
            ..Default::default()
        },
    );
    assert!(radio.get("genre").is_none(), "{radio}");
    assert!(radio.get("genres").is_none(), "{radio}");
}
