//! The personal playlists' background pass: binds a position's library half to what the library
//! actually holds, both ways. A download that lands gives the position its `library_id`; a file
//! that leaves takes it away, and the position falls back to the outside stream (R-4) — one
//! [`ImportMatcher::match_tracks`] call answers both, because it sets the id or `None`.
//!
//! After a change the library's half of the playlist is mirrored to Navidrome as local ids only,
//! in the stored order, by the same diff the imports' playlist keeper uses (so an unchanged
//! playlist costs one read and no writes).

use std::sync::Arc;
use std::time::Duration;

use tracing::{info, warn};

use crate::services::imports::ImportMatcher;
use crate::services::imports::import_models::ImportTrack;
use crate::services::library::NavidromePlaylistApi;
use crate::services::subsonic::personal_playlist_store::PersonalPlaylist;
use crate::services::subsonic::personal_playlist_store::PersonalPlaylistStore;

/// How often the pass runs. A download usually lands minutes before anyone reopens the list, so
/// a minute keeps the mirror fresh without polling Navidrome hard.
const RECONCILE_EVERY: Duration = Duration::from_secs(60);

/// The reconciler behind the personal playlists. Adding a position never queues a download
/// (that is the heart's job); this only watches what the library already holds.
pub struct PersonalPlaylistReconciler {
    store: Arc<PersonalPlaylistStore>,
    matcher: Arc<ImportMatcher>,
    navidrome: Arc<NavidromePlaylistApi>,
}

impl PersonalPlaylistReconciler {
    pub fn new(
        store: Arc<PersonalPlaylistStore>,
        matcher: Arc<ImportMatcher>,
        navidrome: Arc<NavidromePlaylistApi>,
    ) -> Self {
        PersonalPlaylistReconciler {
            store,
            matcher,
            navidrome,
        }
    }

    /// One pass over every playlist. True when anything changed (for the tests).
    pub async fn reconcile_once(&self) -> bool {
        let mut changed = false;
        for playlist in self.store.snapshot() {
            changed |= self.reconcile_playlist(&playlist).await;
        }
        changed
    }

    /// One playlist: match its positions against the library, then mirror the local half when
    /// the matching moved anything. Idempotent: an unchanged playlist is neither written nor
    /// mirrored.
    async fn reconcile_playlist(&self, playlist: &PersonalPlaylist) -> bool {
        // The positions a match can decide: those whose snapshot names the song. A purely local
        // position (added by its library id, no outside half) keeps the id it was added with.
        let mut tracks: Vec<ImportTrack> = Vec::new();
        let mut places: Vec<&str> = Vec::new();
        for entry in &playlist.entries {
            let Some(snapshot) = &entry.song_snapshot else {
                continue;
            };
            tracks.push(ImportTrack {
                key: String::new(),
                title: snapshot.title.clone(),
                artist: snapshot.artist.clone(),
                album: (!snapshot.album.is_empty()).then(|| snapshot.album.clone()),
                seconds: snapshot.duration,
                isrc: snapshot.isrc.clone(),
                library_id: None,
            });
            places.push(&entry.occurrence_id);
        }
        if tracks.is_empty() {
            return false;
        }
        if !self.matcher.match_tracks(&mut tracks).await {
            // The library could not be read: the positions keep what they hold.
            return false;
        }
        let mut moved: Vec<(String, Option<String>)> = Vec::new();
        for (entry, track) in playlist.entries.iter().zip(&tracks) {
            if entry.library_id != track.library_id {
                moved.push((entry.occurrence_id.clone(), track.library_id.clone()));
            }
        }
        if moved.is_empty() {
            return false;
        }
        for (occurrence_id, library_id) in &moved {
            self.store
                .set_library_id(&playlist.id, occurrence_id, library_id.clone());
        }
        info!(
            "Playlist '{}' moved {} position(s) between the library and the stream",
            playlist.name,
            moved.len()
        );
        let Some(playlist) = self.store.find(&playlist.id) else {
            return true;
        };
        self.mirror(&playlist).await;
        true
    }

    /// Writes the playlist's local half to Navidrome: local ids only, in the stored order,
    /// changing only what differs — the songs up to the first difference stay, everything after
    /// it is replaced (the imports' keeper's diff, so a mirror that matches costs nothing).
    /// Navidrome never sees a registry id (it would refuse the whole call).
    async fn mirror(&self, playlist: &PersonalPlaylist) {
        const REMOVE_CHUNK: usize = 100;
        const ADD_CHUNK: usize = 500;

        let wanted: Vec<String> = playlist
            .entries
            .iter()
            .filter_map(|entry| entry.library_id.clone())
            .collect();
        let Ok(Some(rows)) = self.navidrome.read_all_tracks(&playlist.id).await else {
            warn!(
                "Playlist '{}' could not be read to mirror its local songs",
                playlist.name
            );
            return;
        };
        let current: Vec<(String, String)> = rows
            .into_iter()
            .map(|row| (row.position, row.media_file_id))
            .collect();
        let mut same = 0;
        while same < current.len() && same < wanted.len() && current[same].1 == wanted[same] {
            same += 1;
        }
        if same == current.len() && same == wanted.len() {
            return;
        }

        // From the end backwards: removing the last positions leaves the earlier ones in place.
        let stale: Vec<String> = current[same..]
            .iter()
            .map(|(position, _)| position.clone())
            .collect();
        let mut end = stale.len();
        while end > 0 {
            let start = end.saturating_sub(REMOVE_CHUNK);
            if !self
                .navidrome
                .remove_positions(&playlist.id, &stale[start..end])
                .await
                .unwrap_or(false)
            {
                warn!(
                    "Playlist '{}' could not drop its stale mirror rows",
                    playlist.name
                );
                return;
            }
            end = start;
        }
        for chunk in wanted[same..].chunks(ADD_CHUNK) {
            if !self
                .navidrome
                .add_tracks(&playlist.id, chunk)
                .await
                .unwrap_or(false)
            {
                warn!("Playlist '{}' could not add its fresh mirror rows", playlist.name);
                return;
            }
        }
    }

    /// The pass, as a worker.
    pub async fn run(self: Arc<Self>, token: tokio_util::sync::CancellationToken) -> anyhow::Result<()> {
        let mut every = tokio::time::interval(RECONCILE_EVERY);
        every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                () = token.cancelled() => return Ok(()),
                _ = every.tick() => {
                    let _ = self.reconcile_once().await;
                }
            }
        }
    }
}
