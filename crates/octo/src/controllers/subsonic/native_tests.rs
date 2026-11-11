//! The native song object's discovery badge: a row the search answered says which service it
//! came from, under the same name the Subsonic serializer gives it (`octoSuggestedBy`).

use octo_core::models::domain::Song;
use octo_core::settings::AppSettings;

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
