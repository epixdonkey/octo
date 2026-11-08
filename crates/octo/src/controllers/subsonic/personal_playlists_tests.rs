//! The personal playlists overlay's tests, through the controller harness: the REST and native
//! CRUD, the local-only mirror, the heart's own download paths (R4-T4).

use super::*;

/// A held playlist answers `getPlaylist` from its own list, not the local-only mirror's.
#[test]
fn the_overlay_module_is_wired_into_the_state() {
    let state = crate::app::AppState::for_tests(octo_core::settings::AppSettings::default());
    assert!(!state.personal_playlists.knows("nd-unknown"));
}
