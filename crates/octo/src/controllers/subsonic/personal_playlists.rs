//! The personal playlists overlay: mixed positions on a playlist's real Navidrome id, answered
//! by both protocols (Subsonic REST and Navidrome's native API).
//!
//! The overlay knows a playlist by its being in `personal-playlists.json`; every other playlist
//! is a plain passthrough, until a client puts an outside id on one, which Navidrome would
//! refuse — the overlay adopts the playlist first (the am-yy move). Nothing here queues a
//! download: adding a position is a purely streaming write (R-2), and the only explicit
//! downloads are the heart's own paths — the `[download]` comment tag, straight through the
//! heart's coordinator (D15). Navidrome is mirrored with local ids only, or it would refuse the
//! whole call (Fakt A1).

use std::collections::{HashMap, HashSet};

use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use indexmap::IndexMap;
use octo_core::json::dom::Node;
use octo_core::soulseek::SoulseekRouting;
use octo_subsonic::Parameters;
use octo_subsonic::SubsonicReply;
use octo_subsonic::subsonic_response_builder::{genres_with_source, source_genre_for};
use octo_subsonic::xml::XElement;
use serde_json::{Value, json};
use tracing::warn;

use super::action_helpers::{SubsonicCall, is_octo_playlist_id, is_successful_subsonic_response};
use super::native::NativeRequest;
use super::playlists::outside_song_json;
use crate::app::AppState;
use crate::services::subsonic::personal_playlist_store::{
    PersonalPlaylist, PersonalPlaylistEntry, ServeSource, seed_matching,
};

/// The comment tag that asks for the playlist's missing positions to be fetched (R-5 op. 4).
pub(crate) const DOWNLOAD_TAG: &str = "[download]";

/// The folder tag's marker, `#folder:Ścieżka` (D9's third carrier).
const FOLDER_TAG: &str = "#folder:";

/// The keys a relayed call keeps the caller's sign-in under (`u/p/t/s`, `playlists.rs`'s
/// `AUTH_KEYS` pattern).
const AUTH_KEYS: [&str; 8] = ["u", "p", "t", "s", "v", "c", "apiKey", "jwt"];

/// Whether `comment` asks for the playlist's missing positions to be fetched.
pub(crate) fn comment_asks_to_download(comment: &str) -> bool {
    comment.contains(DOWNLOAD_TAG)
}

/// The caller's sign-in as the relay carries it, with JSON asked for. A native-mode client signs
/// in with a header the REST relay does not forward, so a request that named nobody signs in as
/// Octo's own service account — the scan-auth pair, the charts' country walks' stand-in. That
/// service signature is the relay's own hand, not the caller's: every Navidrome read or write it
/// makes here (the mirror, the snapshot backfills, the adoptions) is attributed to the service
/// account, because the caller's own sign-in never reached the REST relay. The limitation is the
/// product's single-owner shape — one household, one library, one admin — so the service account
/// is the owner's own stand-in and a native request that named nobody still acts for the owner;
/// with several real accounts it would lend the admin's authority to an unnamed caller (O2's
/// sibling), and widening that is the bridge's call, not this code's guess.
fn auth_of(state: &AppState, parameters: &Parameters) -> Vec<(String, String)> {
    let own: Vec<(String, String)> = parameters
        .iter()
        .filter(|(key, _)| AUTH_KEYS.iter().any(|k| k.eq_ignore_ascii_case(key)))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    if !own.is_empty() {
        return own;
    }
    match state.navidrome_identity.get_scan_auth() {
        Some((user, token, salt)) => [
            ("u", user),
            ("t", token),
            ("s", salt),
            ("v", "1.16.1".to_string()),
            ("c", "octo".to_string()),
        ]
        .into_iter()
        .map(|(key, value): (&str, String)| (key.to_string(), value))
        .collect(),
        None => Vec::new(),
    }
}

/// A fresh position id: names one place in the list, so the same song twice keeps two rows and
/// drag-and-drop moves one of them.
fn new_occurrence_id() -> String {
    format!("occ-{}", uuid::Uuid::new_v4().simple())
}

/// The song ids a client hands over as the positions they become, in one pass: a library id
/// keeps its own half — its metadata arrives with the mirror's one bulk read after the mutation,
/// not as a round-trip per position — and an outside id (the registry's short id) keeps the
/// outside half: its video, the full snapshot, and the seed that minted the id, honestly kept
/// out when no candidate hashes to it.
pub(crate) async fn entries_for(state: &AppState, ids: &[String]) -> Vec<PersonalPlaylistEntry> {
    let mut entries = Vec::new();
    for id in ids {
        let id = id.trim();
        if id.is_empty() {
            continue;
        }
        let (external, _, _) = state.local_library.parse_song_id(id);
        if !external {
            entries.push(PersonalPlaylistEntry {
                occurrence_id: new_occurrence_id(),
                library_id: Some(id.to_string()),
                ..Default::default()
            });
            continue;
        }
        if let Some(entry) = outside_entry_for(state, id).await {
            entries.push(entry);
        }
    }
    entries
}

/// One outside id as a position: the registry's routing, the full snapshot (the catalog's answer
/// first, the routing's own words when it is gone) and the seed that minted the id. Nothing here
/// asks Navidrome — the registry and the store are the outside half's own keepers.
async fn outside_entry_for(state: &AppState, id: &str) -> Option<PersonalPlaylistEntry> {
    let (_, provider, external_id) = state.local_library.parse_song_id(id);
    let provider = provider.unwrap_or_default();
    let external_id = external_id.unwrap_or_default();
    // The registry is what knows the id: its lookup re-registers an evicted entry through the
    // position's own seed, once the store stands behind the recovery chain.
    let routing = state
        .external_id_registry
        .lookup(&external_id)
        .map(|shared| shared.snapshot());
    let song = state
        .metadata_service
        .get_song(&provider, &external_id)
        .await
        .or_else(|| routing.as_ref().map(routing_song));
    let artist = song.as_ref().map(|s| s.artist.clone()).unwrap_or_default();
    let title = song.as_ref().map(|s| s.title.clone()).unwrap_or_default();
    let duration = song.as_ref().and_then(|s| s.duration);
    let you_tube_id = routing.as_ref().and_then(|r| r.you_tube_id.clone());
    // The discovery service the row came by, kept on the position because the song's own
    // `suggested_by` is never stored: it is what the position's rows badge their genre by,
    // and it survives a registry eviction with the position itself.
    let source = routing.as_ref().and_then(|r| r.suggested_by.clone());
    let seed = seed_matching(&external_id, you_tube_id.as_deref(), &artist, &title, duration);
    if seed.is_none() {
        warn!(
            "Playlist position {id} keeps no seed: no minted routing hashes to it, so it cannot \
             be recovered after a restart"
        );
    }
    Some(PersonalPlaylistEntry {
        occurrence_id: new_occurrence_id(),
        library_id: None,
        outside_id: Some(external_id),
        you_tube_id,
        source,
        song_snapshot: song,
        seed,
    })
}

/// The thin song a routing names, for a position whose catalog answer is gone.
fn routing_song(routing: &SoulseekRouting) -> octo_core::models::domain::Song {
    octo_core::models::domain::Song {
        id: String::new(),
        title: routing.title.clone().unwrap_or_default(),
        artist: routing.artist.clone().unwrap_or_default(),
        album: routing.album.clone().unwrap_or_default(),
        duration: routing.duration,
        isrc: routing.isrc.clone(),
        is_local: false,
        ..Default::default()
    }
}

/// The playlist's local half as its own mirror in Navidrome holds it — one relay, every local
/// row at once, however long the list. The mirror is the overlay's own copy of the local half,
/// and Navidrome drops a row when its file goes, so the rows are both the metadata and the
/// availability of the moment (PREFER_LOCAL, R-4). `None` when Navidrome could not be asked.
async fn mirror_song_rows(
    proxy: &crate::services::subsonic::SubsonicProxyService,
    auth: &[(String, String)],
    playlist_id: &str,
) -> Option<HashMap<String, Value>> {
    if auth.is_empty() {
        return None;
    }
    let mut pairs: Vec<(String, String)> = auth.to_vec();
    pairs.push(("id".into(), playlist_id.to_string()));
    pairs.push(("f".into(), "json".into()));
    let relay = proxy
        .relay_safe("rest/getPlaylist", pairs.iter().map(|(k, v)| (k, v)))
        .await?;
    let answer: Value = serde_json::from_slice(&relay.body).ok()?;
    let rows = answer.pointer("/subsonic-response/playlist/entry")?.as_array()?;
    Some(
        rows.iter()
            .filter_map(|row| {
                row.get("id")
                    .and_then(Value::as_str)
                    .map(|id| (id.to_string(), row.clone()))
            })
            .collect(),
    )
}

/// A library song's own fields, as the snapshot the native rows are built from later. One read
/// of the mirror answers every local row at once, so this is a mapping, not a round-trip.
fn song_from_row(row: &Value) -> octo_core::models::domain::Song {
    octo_core::models::domain::Song {
        id: row
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        title: row
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        artist: row
            .get("artist")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        album: row
            .get("album")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        duration: row
            .get("duration")
            .and_then(Value::as_i64)
            .and_then(|d| i32::try_from(d).ok()),
        is_local: true,
        ..Default::default()
    }
}

/// The positions' playback sources for one answer: the store's one resolution mechanism
/// (`resolve_playback_sources`, PREFER_LOCAL) over the mirror's fresh availability — one relay
/// for the whole list, not one per position. When Navidrome cannot be asked, the store's own
/// bindings stand in, so the answer keeps every position that still has a source. A read that
/// does reach the mirror heals with it: the rows are in hand anyway, so a bare local position's
/// snapshot fills right here (O5) — no second relay, and signed in as the caller whenever the
/// caller named anyone, which every REST request does.
async fn served_sources(
    state: &AppState,
    proxy: &crate::services::subsonic::SubsonicProxyService,
    parameters: &Parameters,
    playlist: &PersonalPlaylist,
) -> (Vec<ServeSource>, Option<HashMap<String, Value>>) {
    let auth = auth_of(state, parameters);
    match mirror_song_rows(proxy, &auth, &playlist.id).await {
        Some(rows) => {
            if has_bare_local_entries(playlist) {
                fill_local_snapshots(state, &playlist.id, &rows);
            }
            let available: HashSet<String> = rows.keys().cloned().collect();
            (playlist.resolve_playback_sources(&available), Some(rows))
        }
        None => {
            let held: HashSet<String> = playlist
                .entries
                .iter()
                .filter_map(|entry| entry.library_id.clone())
                .collect();
            (playlist.resolve_playback_sources(&held), None)
        }
    }
}

/// The entries with the source each plays by, in the list's own order: the store's resolution
/// aligned back onto its entries. A position the resolution leaves out — no file and no stream —
/// is not listed; a song twice keeps both of its own places.
fn zipped<'a>(
    playlist: &'a PersonalPlaylist,
    sources: &'a [ServeSource],
) -> Vec<(&'a PersonalPlaylistEntry, &'a ServeSource)> {
    let mut at = 0;
    playlist
        .entries
        .iter()
        .filter_map(move |entry| {
            let source = sources.get(at)?;
            let same = match source {
                ServeSource::Library { id, .. } => entry.library_id.as_deref() == Some(id.as_str()),
                ServeSource::Outside { outside_id, .. } => {
                    entry.outside_id.as_deref() == Some(outside_id.as_str())
                }
            };
            if same {
                at += 1;
                Some((entry, source))
            } else {
                None
            }
        })
        .collect()
}

/// Fills the snapshots of the local positions the mirror just wrote: one bulk read, so a
/// mutation costs it once however many positions it added — and an adopted position (which
/// arrives with no snapshot at all) fills in here too. The fill itself is the store's one
/// atomic edit, so a parallel mutation of the same playlist cannot be lost to it (O3). Reads
/// heal the same way through `served_sources`, from the mirror rows the answer read anyway
/// (O5). The playlist as the store now holds it; `None` when there was nothing to fill or the
/// mirror could not be read — a native caller that named nobody still needs the service
/// account's sign-in (scan-auth) for any Navidrome read at all, the one dependency left.
async fn backfill_local_snapshots(
    state: &AppState,
    proxy: &crate::services::subsonic::SubsonicProxyService,
    parameters: &Parameters,
    playlist: &PersonalPlaylist,
) -> Option<PersonalPlaylist> {
    // Nothing to fill costs no read at all: a removal, a reorder or a metadata edit does not
    // ask Navidrome again.
    if !has_bare_local_entries(playlist) {
        return None;
    }
    let auth = auth_of(state, parameters);
    let rows = mirror_song_rows(proxy, &auth, &playlist.id).await?;
    fill_local_snapshots(state, &playlist.id, &rows)
}

/// Whether a local position still has no snapshot: the cheap gate that keeps an ordinary
/// mutation or answer from paying a mirror read or a store write for nothing (O5).
fn has_bare_local_entries(playlist: &PersonalPlaylist) -> bool {
    playlist
        .entries
        .iter()
        .any(|entry| entry.song_snapshot.is_none() && entry.library_id.is_some())
}

/// The one atomic fill itself (O3: under the store's lock, so no parallel edit is lost to it):
/// every local position the rows name gets its row as the snapshot, whatever else changed in
/// between. The playlist as the store now holds it; `None` when the store does not hold the id.
fn fill_local_snapshots(
    state: &AppState,
    playlist_id: &str,
    rows: &HashMap<String, Value>,
) -> Option<PersonalPlaylist> {
    state
        .personal_playlists
        .edit(playlist_id, |list| {
            for entry in &mut list.entries {
                if entry.song_snapshot.is_some() {
                    continue;
                }
                let Some(row) = entry.library_id.as_deref().and_then(|id| rows.get(id)) else {
                    continue;
                };
                entry.song_snapshot = Some(song_from_row(row));
            }
        })
        .map(|(filled, ())| filled)
}

/// The empty playlist in Navidrome first (am-yy's move: no song ids on the create), and its
/// real id back. `None` when Navidrome would not make it.
async fn create_empty_playlist(
    proxy: &crate::services::subsonic::SubsonicProxyService,
    auth: &[(String, String)],
    name: Option<&str>,
) -> Option<String> {
    let mut create = auth.to_vec();
    if let Some(name) = name {
        create.push(("name".into(), name.to_string()));
    }
    create.push(("f".into(), "json".into()));
    let created = proxy
        .relay_safe("rest/createPlaylist", create.iter().map(|(k, v)| (k, v)))
        .await;
    created
        .and_then(|relay| serde_json::from_slice::<Value>(&relay.body).ok())
        .and_then(|answer| {
            answer
                .pointer("/subsonic-response/playlist/id")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .filter(|id| !id.is_empty())
}

/// The song ids a client's `songId`-shaped parameter carried, in order, blanks left out.
fn song_ids(call: &SubsonicCall, key: &str) -> Vec<String> {
    call.incoming
        .parameter_values(key)
        .iter()
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .collect()
}

/// Takes a folder out of a name a client sent (`Folder / Podfolder / Nazwa`) and a comment's
/// `#folder:` tag: the overlay stores the base words and the folder field, and puts the carriers
/// back on every answer (D9, both directions).
fn take_folder(
    name: Option<&str>,
    comment: Option<&str>,
) -> (Option<String>, Option<String>, Option<String>) {
    // The tag's value is the rest of the comment: the overlay writes the tag last, and a folder
    // may hold spaces ("Techno/Peak Time") — the first word alone would not carry it (D9).
    let folder_from_tag = comment
        .and_then(|comment| comment.split(FOLDER_TAG).nth(1))
        .map(|rest| rest.trim().to_string())
        .filter(|folder| !folder.is_empty());
    let mut base_name = name.map(str::to_string);
    let folder_from_name = name.and_then(|name| {
        let parts: Vec<&str> = name.split(" / ").collect();
        if parts.len() < 2 {
            return None;
        }
        let folder = parts[..parts.len() - 1].join("/");
        base_name = Some(parts[parts.len() - 1].trim().to_string());
        (!folder.trim().is_empty()).then_some(folder)
    });
    let folder = folder_from_tag.or(folder_from_name);
    let base_comment = comment.map(|comment| {
        comment
            .split(FOLDER_TAG)
            .next()
            .unwrap_or("")
            .replace(DOWNLOAD_TAG, "")
            .trim()
            .to_string()
    });
    (base_name, base_comment, folder)
}

/// Applies what a mutation said about the playlist itself: name, comment (with both tags read
/// off) and publicity, folder included.
fn apply_metadata(
    playlist: &mut PersonalPlaylist,
    name: Option<&str>,
    comment: Option<&str>,
    public: Option<bool>,
) {
    let (base_name, base_comment, folder) = take_folder(name, comment);
    let folder = folder.or_else(|| playlist.folder.clone());
    if let Some(name) = base_name.filter(|name| !name.trim().is_empty()) {
        playlist.name = name;
    }
    if let Some(comment) = base_comment {
        playlist.comment = comment;
    }
    if let Some(public) = public {
        playlist.public = public;
    }
    playlist.set_folder(folder.as_deref());
    playlist.changed = chrono::Utc::now();
}

/// Mirrors the playlist to Navidrome: local ids only, in the stored order, by `createPlaylist`
/// with the id (Subsonic's replace-lists call) and then `updatePlaylist` for the metadata — the
/// am-yy keeper's pair. Best effort: a refused mirror is logged, never fatal.
async fn mirror_local_ids(
    state: &AppState,
    proxy: &crate::services::subsonic::SubsonicProxyService,
    parameters: &Parameters,
    playlist: &PersonalPlaylist,
) {
    let auth = auth_of(state, parameters);
    if auth.is_empty() {
        warn!(
            "Playlist '{}' could not be mirrored: nobody to sign in as",
            playlist.name
        );
        return;
    }
    let mut replace = auth.clone();
    replace.push(("playlistId".into(), playlist.id.clone()));
    replace.push(("name".into(), playlist.display_name()));
    replace.push(("f".into(), "json".into()));
    for id in playlist
        .entries
        .iter()
        .filter_map(|entry| entry.library_id.clone())
    {
        replace.push(("songId".into(), id));
    }
    let replaced = proxy
        .relay_safe("rest/createPlaylist", replace.iter().map(|(k, v)| (k, v)))
        .await;
    if !replaced.is_some_and(|relay| is_successful_subsonic_response(&relay.body, "json")) {
        warn!("Playlist '{}' could not be mirrored to Navidrome", playlist.name);
        return;
    }
    let mut metadata = auth;
    metadata.push(("playlistId".into(), playlist.id.clone()));
    metadata.push(("name".into(), playlist.display_name()));
    metadata.push(("comment".into(), playlist.display_comment()));
    metadata.push(("public".into(), playlist.public.to_string()));
    metadata.push(("f".into(), "json".into()));
    let _ = proxy
        .relay_safe("rest/updatePlaylist", metadata.iter().map(|(k, v)| (k, v)))
        .await;
}

/// Adopts a playlist Navidrome holds and the overlay does not: its rows become local positions,
/// so a mixed mutation on a plain playlist keeps everything it already had. `None` when
/// Navidrome cannot be asked.
async fn adopt(state: &AppState, id: &str) -> Option<PersonalPlaylist> {
    let rows = state.navidrome_playlist_api.list_playlists().await;
    let row = rows.iter().find(|row| row.id == id)?;
    let mut entries = Vec::new();
    for track in state.navidrome_playlist_api.list_tracks(id).await {
        entries.push(PersonalPlaylistEntry {
            occurrence_id: new_occurrence_id(),
            library_id: Some(track.media_file_id),
            ..Default::default()
        });
    }
    let (base_name, _base_comment, folder) = take_folder(Some(&row.name), None);
    Some(PersonalPlaylist {
        id: id.to_string(),
        owner: row.owner.clone(),
        name: base_name.unwrap_or_else(|| row.name.clone()),
        folder,
        entries,
        ..Default::default()
    })
}

/// The playlist rows of a `getPlaylists` answer with the counts the overlay holds — not the
/// local-only mirror's. `None` when the body is not one this reads, or nothing was corrected.
pub(crate) fn correct_playlist_rows(state: &AppState, body: &[u8], format: &str) -> Option<Vec<u8>> {
    let store = &state.personal_playlists;
    let mut corrected = false;
    let totals = |id: &str| -> Option<(usize, i64)> {
        let playlist = store.find(id)?;
        let seconds: i64 = playlist
            .entries
            .iter()
            .map(|entry| i64::from(entry.song_snapshot.as_ref().and_then(|s| s.duration).unwrap_or(0)))
            .sum();
        Some((playlist.entries.len(), seconds))
    };
    if format.eq_ignore_ascii_case("json") {
        let mut root = Node::parse(std::str::from_utf8(body).ok()?).ok()?;
        let response = root
            .as_object_mut()?
            .get_mut("subsonic-response")?
            .as_object_mut()?;
        let mut rows: Vec<&mut IndexMap<String, Node>> = Vec::new();
        for (key, value) in response.iter_mut() {
            match (key.as_str(), value) {
                ("playlists", Node::Object(holder)) => match holder.get_mut("playlist") {
                    Some(Node::Array(list)) => rows.extend(list.iter_mut().filter_map(Node::as_object_mut)),
                    Some(Node::Object(one)) => rows.push(one),
                    _ => {}
                },
                ("playlist", Node::Object(one)) => rows.push(one),
                _ => {}
            }
        }
        for row in rows {
            let Some(id) = row.get("id").and_then(Node::as_str) else {
                continue;
            };
            if let Some((count, seconds)) = totals(id) {
                row.insert("songCount".into(), Node::Number(count.to_string()));
                row.insert("duration".into(), Node::Number(seconds.to_string()));
                corrected = true;
            }
        }
        return corrected.then(|| root.to_json_string(false).into_bytes());
    }
    if !format.eq_ignore_ascii_case("xml") {
        return None;
    }
    let mut root = XElement::parse(&String::from_utf8_lossy(body)).ok()?;
    let mut mark = |row: &mut XElement| {
        let Some(id) = row.attribute("id") else {
            return;
        };
        if let Some((count, seconds)) = totals(id) {
            row.set_attr("songCount", count.to_string());
            row.set_attr("duration", seconds.to_string());
            corrected = true;
        }
    };
    for element in root.elements_mut() {
        if element.name == "playlists" {
            for row in element.elements_mut().filter(|row| row.name == "playlist") {
                mark(row);
            }
        } else if element.name == "playlist" {
            mark(element);
        }
    }
    corrected.then(|| root.to_xml_string().into_bytes())
}

/// The native list's playlist rows (`GET /api/playlist`, a bare JSON array) with the counts the
/// overlay holds — not the local-only mirror's; the native twin of [`correct_playlist_rows`],
/// which walks the Subsonic envelope instead. A row the overlay does not hold keeps Navidrome's
/// own counts (a plain playlist's passthrough).
pub(crate) fn correct_native_playlist_rows(state: &AppState, rows: &mut [Node]) {
    for row in rows {
        let Some(object) = row.as_object_mut() else {
            continue;
        };
        let Some(id) = object.get("id").and_then(Node::as_str) else {
            continue;
        };
        let Some(playlist) = state.personal_playlists.find(id) else {
            continue;
        };
        object.insert(
            "songCount".into(),
            Node::Number(playlist.entries.len().to_string()),
        );
        object.insert("duration".into(), Node::Number(seconds_of(&playlist).to_string()));
    }
}

/// How long a playlist's positions last, as its counts say them: the snapshot each position
/// kept.
fn seconds_of(playlist: &PersonalPlaylist) -> i64 {
    playlist
        .entries
        .iter()
        .map(|entry| i64::from(entry.song_snapshot.as_ref().and_then(|s| s.duration).unwrap_or(0)))
        .sum()
}

/// `getPlaylist` for a playlist the overlay holds: every position in its stored order, each with
/// the source it plays by (`PREFER_LOCAL`) — one read of the playlist's own mirror decides every
/// position at once through the store's `resolve_playback_sources`, so the answer costs one
/// upstream call however long the list is. The library's own live row when the file is there,
/// the outside version the position kept when it is not. Positions outside are registered for
/// the scrobble-driven prewarm. `None` when the overlay does not hold the id.
pub(crate) async fn rest_get_playlist(state: &AppState, call: &SubsonicCall, id: &str) -> Option<Response> {
    let playlist = state.personal_playlists.find(id)?;
    let (sources, mirror) = served_sources(state, &call.proxy, &call.parameters, &playlist).await;
    let mut entries: Vec<Node> = Vec::new();
    let mut outside_ids: Vec<String> = Vec::new();
    // The sources aligned back onto their entries, so each outside row is answered with its own
    // position's kept source — the label its genre badge reads (option A).
    for (entry, source) in zipped(&playlist, &sources) {
        match source {
            ServeSource::Library { id, outside } => {
                if let Some(row) = mirror.as_ref().and_then(|rows| rows.get(id)) {
                    entries.push(Node::from_value(row));
                    continue;
                }
                // Navidrome would not tell (or the row went between the read and here): the
                // stream takes over what the position kept.
                if let Some(outside) = outside
                    && let Some(song) = outside_song_json(state, outside, entry.source.as_deref()).await
                {
                    entries.push(Node::from_value(&song));
                    outside_ids.push(outside.clone());
                }
            }
            ServeSource::Outside { outside_id, .. } => {
                if let Some(song) = outside_song_json(state, outside_id, entry.source.as_deref()).await {
                    entries.push(Node::from_value(&song));
                    outside_ids.push(outside_id.clone());
                }
            }
        }
    }
    state.radio_queues.register(outside_ids.iter().cloned());
    let fields = playlist_fields(&playlist, entries.len(), seconds_of(&playlist));
    Some(
        state
            .subsonic_response_builder
            .create_entries_playlist_response(&call.format, fields, &entries)
            .into_response(),
    )
}

/// One playlist's fields for a REST answer.
fn playlist_fields(
    playlist: &PersonalPlaylist,
    count: usize,
    seconds: i64,
) -> serde_json::Map<String, Value> {
    let mut fields = serde_json::Map::new();
    fields.insert("id".into(), playlist.id.clone().into());
    fields.insert("name".into(), playlist.display_name().into());
    fields.insert("comment".into(), playlist.display_comment().into());
    fields.insert("owner".into(), playlist.owner.clone().into());
    fields.insert("public".into(), playlist.public.into());
    fields.insert("songCount".into(), count.into());
    fields.insert("duration".into(), seconds.into());
    fields.insert(
        "changed".into(),
        playlist
            .changed
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
            .into(),
    );
    fields
}

/// The REST mutations (`createPlaylist`, `updatePlaylist`, `deletePlaylist`) for a playlist the
/// overlay holds — and for a plain one the moment a client puts an outside id on it (the overlay
/// adopts the playlist first). `None` when the call is none of the overlay's business: a plain
/// passthrough.
pub(crate) async fn rest_mutate(
    state: &AppState,
    call: &SubsonicCall,
    endpoint: &str,
    failure: &str,
) -> Option<Response> {
    let playlist_id = call
        .param("playlistId")
        .or_else(|| call.param("id"))
        .unwrap_or("");
    let adding: Vec<String> = song_ids(call, "songId")
        .into_iter()
        .chain(song_ids(call, "songIdToAdd"))
        .collect();
    let mixed = || adding.iter().any(|id| state.local_library.parse_song_id(id).0);
    if endpoint.eq_ignore_ascii_case("createPlaylist") {
        return rest_create(state, call, playlist_id, &adding, failure).await;
    }
    let known = state.personal_playlists.knows(playlist_id);
    if !known && !mixed() {
        return None;
    }
    if !known {
        // Two parallel mixed edits can both adopt the playlist; the first adoption stands and
        // every edit lands on it through the store's own atomic edit (O3).
        let adopted = adopt(state, playlist_id).await?;
        state.personal_playlists.insert_if_absent(adopted);
    }

    if endpoint.eq_ignore_ascii_case("deletePlaylist") {
        let playlist = state.personal_playlists.find(playlist_id)?;
        let mut auth = auth_of(state, &call.parameters);
        auth.push(("id".into(), playlist.id.clone()));
        auth.push(("f".into(), "json".into()));
        let deleted = call
            .proxy
            .relay_safe("rest/deletePlaylist", auth.iter().map(|(k, v)| (k, v)))
            .await;
        let gone = deleted.is_some_and(|relay| {
            is_successful_subsonic_response(&relay.body, "json") || missing_subsonic_resource(&relay.body)
        });
        if !gone {
            return Some(error_response(state, call, 0, failure));
        }
        state.personal_playlists.remove(&playlist.id);
        return Some(ok_response(state, call, "deletePlaylist"));
    }
    if !endpoint.eq_ignore_ascii_case("updatePlaylist") {
        return None;
    }

    // updatePlaylist: metadata (with both tags), additions, removals. The additions are
    // prepared first (they may ask the metadata service), so the change itself is the store's
    // one atomic read-modify-write and a parallel request's change cannot be lost (O3).
    let name = call.param("name").map(str::to_string);
    let comment = call.param("comment").map(str::to_string);
    let public = call
        .param("public")
        .map(|value| value.trim().eq_ignore_ascii_case("true"));
    let asks_to_download = comment.as_deref().is_some_and(comment_asks_to_download);
    let fresh = entries_for(state, &adding).await;
    let (playlist, ()) = state
        .personal_playlists
        .edit(playlist_id, |list| {
            apply_metadata(list, name.as_deref(), comment.as_deref(), public);
            let removals: Vec<usize> = call
                .incoming
                .parameter_values("songIndexToRemove")
                .iter()
                .filter_map(|index| index.trim().parse::<usize>().ok())
                .collect();
            for index in removals.iter().rev() {
                if index < &list.entries.len() {
                    list.entries.remove(*index);
                }
            }
            list.entries.extend(fresh);
            list.changed = chrono::Utc::now();
        })
        // A playlist the store stopped holding between the gate and here is a plain one again:
        // the caller's passthrough.
        ?;
    let owner = playlist.owner.clone();
    mirror_local_ids(state, &call.proxy, &call.parameters, &playlist).await;
    let playlist = backfill_local_snapshots(state, &call.proxy, &call.parameters, &playlist)
        .await
        .unwrap_or(playlist);
    if asks_to_download {
        let requested_by = super::caller_checks::requester_for(state, Some(owner.as_str()));
        super::playlists::queue_missing(state, &playlist, requested_by.as_deref());
    }
    Some(ok_response(state, call, "updatePlaylist"))
}

/// `createPlaylist`: without `playlistId` a mixed list makes the empty playlist in Navidrome
/// first (its `songId` taken out, am-yy's move), takes the real id back and fills the overlay;
/// with `playlistId` (Subsonic's replace-the-list call) a held playlist gets a whole new list.
/// A purely local create is none of the overlay's business.
async fn rest_create(
    state: &AppState,
    call: &SubsonicCall,
    playlist_id: &str,
    adding: &[String],
    failure: &str,
) -> Option<Response> {
    let mixed = adding.iter().any(|id| state.local_library.parse_song_id(id).0);
    // Prepared before any store write, so the change itself is one atomic read-modify-write
    // (O3).
    let fresh = entries_for(state, adding).await;
    let playlist = if playlist_id.is_empty() {
        if !mixed {
            return None;
        }
        let auth = auth_of(state, &call.parameters);
        let id = create_empty_playlist(&call.proxy, &auth, call.param("name")).await;
        let Some(id) = id else {
            return Some(error_response(state, call, 0, failure));
        };
        let owner =
            super::caller_checks::listener_name_of(state, &call.parameters, &call.proxy, &call.parts.headers)
                .await;
        let mut playlist = PersonalPlaylist {
            id,
            owner,
            ..Default::default()
        };
        apply_metadata(
            &mut playlist,
            call.param("name"),
            call.param("comment"),
            call.param("public")
                .map(|value| value.eq_ignore_ascii_case("true")),
        );
        playlist.entries.extend(fresh);
        playlist.changed = chrono::Utc::now();
        // A brand-new id cannot race anyone: a plain whole write.
        state.personal_playlists.upsert(playlist.clone());
        playlist
    } else {
        let known = state.personal_playlists.knows(playlist_id);
        if !known && !mixed {
            return None;
        }
        if !known {
            // Insert-if-absent, so a parallel adoption's playlist stands and this replace
            // lands on it (O3).
            let adopted = adopt(state, playlist_id).await?;
            state.personal_playlists.insert_if_absent(adopted);
        }
        let (playlist, ()) = state
            .personal_playlists
            .edit(playlist_id, |list| {
                apply_metadata(
                    list,
                    call.param("name"),
                    call.param("comment"),
                    call.param("public")
                        .map(|value| value.eq_ignore_ascii_case("true")),
                );
                list.entries.clear();
                list.entries.extend(fresh);
                list.changed = chrono::Utc::now();
            })
            // As above: a playlist the store stopped holding is a plain one again.
            ?;
        playlist
    };
    mirror_local_ids(state, &call.proxy, &call.parameters, &playlist).await;
    // The answer carries no rows, so the filled playlist itself is not needed here.
    let _ = backfill_local_snapshots(state, &call.proxy, &call.parameters, &playlist).await;
    Some(ok_response(state, call, "createPlaylist"))
}

/// Whether Navidrome answered that it simply does not have the thing (error 70): deleting
/// something someone else already took is a success.
fn missing_subsonic_resource(body: &[u8]) -> bool {
    serde_json::from_slice::<Value>(body).ok().and_then(|answer| {
        answer
            .pointer("/subsonic-response/error/code")
            .and_then(Value::as_i64)
    }) == Some(70)
}

fn ok_response(state: &AppState, call: &SubsonicCall, name: &str) -> Response {
    state
        .subsonic_response_builder
        .create_response(&call.format, name)
        .into_response()
}

fn error_response(state: &AppState, call: &SubsonicCall, code: i32, message: &str) -> Response {
    state
        .subsonic_response_builder
        .create_error(&call.format, code, message)
        .into_response()
}

/// The native playlist answers in Navidrome mode: the collection's create (`POST
/// /api/playlist`) and the detail, its tracks, the moves and the additions of
/// `/api/playlist/{id}`. `Ok(None)` when the call is none of the overlay's business; the list
/// itself (`GET /api/playlist`) stays with the native radio handler, which corrects the held
/// playlists' counts in its rows.
pub(crate) async fn answer_native(
    state: &AppState,
    request: &NativeRequest<'_>,
) -> Result<Option<Response>, crate::http::error::AppError> {
    const COLLECTION: &str = "api/playlist";
    if request.endpoint.eq_ignore_ascii_case(COLLECTION) {
        if *request.method == axum::http::Method::POST {
            return native_create(state, request).await;
        }
        return Ok(None);
    }
    const PREFIX: &str = "api/playlist/";
    if !octo_core::common::dotnet::starts_with_ignore_case(request.endpoint, PREFIX)
        || request.endpoint.len() == PREFIX.len()
    {
        return Ok(None);
    }
    let tail = request.endpoint[PREFIX.len()..].trim_matches('/');
    let parts: Vec<&str> = tail.split('/').filter(|part| !part.is_empty()).collect();
    let Some(id) = parts.first().copied() else {
        return Ok(None);
    };
    if is_octo_playlist_id(id, state.settings.current().family.enabled) {
        return Ok(None);
    }
    let known = state.personal_playlists.knows(id);
    let body = native_body(request);
    let ids = native_ids(&body);
    let track_ids = native_track_ids(&body);
    let is_get = request.method == axum::http::Method::GET;
    if !known && is_get {
        return Ok(None);
    }
    // The gate is the create's own, symmetric: the overlay takes a plain playlist over only
    // when the body really carries a registry id — the one thing Navidrome would refuse whole
    // (Fakt A1). A purely local body, a `tracks` row list or not, is an ordinary edit:
    // Navidrome's own to do, a faithful passthrough — so the 403 of an unreachable adoption
    // never stands between a client and an edit Navidrome could have made.
    let mut carried = ids.iter().chain(track_ids.iter());
    let carries_a_registry_id = carried.any(|id| state.local_library.parse_song_id(id).0);
    if !known && !carries_a_registry_id {
        return Ok(None);
    }
    let playlist = if known {
        match state.personal_playlists.find(id) {
            Some(playlist) => playlist,
            // Held a moment ago and gone now: nothing to answer from.
            None => return Ok(Some(native_error(StatusCode::NOT_FOUND, "Playlist not found"))),
        }
    } else {
        // The overlay does not hold it, but the mutation is mixed: adopt first, so nothing the
        // playlist already had is lost. Insert-if-absent, so a parallel adoption's playlist
        // stands and every edit below lands on it (O3).
        let Some(adopted) = adopt(state, id).await else {
            return Ok(Some(native_error(
                StatusCode::FORBIDDEN,
                "Playlist is unavailable",
            )));
        };
        state.personal_playlists.insert_if_absent(adopted);
        match state.personal_playlists.find(id) {
            Some(standing) => standing,
            None => return Ok(Some(native_error(StatusCode::NOT_FOUND, "Playlist not found"))),
        }
    };
    // Every mutation below changes the playlist through the store's one atomic
    // read-modify-write, so parallel requests cannot lose each other's change (O3); the
    // standing copy above serves the reads.

    // The tracks: the list, the additions, the removals, the drag-and-drop move.
    if parts.len() >= 2 && parts[1].eq_ignore_ascii_case("tracks") {
        if is_get {
            return Ok(Some(
                native_tracks(state, request.proxy, request.parameters, &playlist).await,
            ));
        }
        if request.method == axum::http::Method::POST {
            let fresh = entries_for(state, &ids).await;
            let added = fresh.len();
            let Some((playlist, ())) = state.personal_playlists.edit(id, |list| {
                list.entries.extend(fresh);
                list.changed = chrono::Utc::now();
            }) else {
                return Ok(Some(native_error(StatusCode::NOT_FOUND, "Playlist not found")));
            };
            mirror_local_ids(state, request.proxy, request.parameters, &playlist).await;
            // The answer names no positions, so the filled playlist itself is not needed here.
            let _ = backfill_local_snapshots(state, request.proxy, request.parameters, &playlist).await;
            return Ok(Some(native_ok(json!({ "id": id, "added": added }))));
        }
        if request.method == axum::http::Method::DELETE {
            let mut remove: Vec<String> = request
                .parameters
                .get("id")
                .map(|value| value.split(',').map(str::to_string).collect())
                .unwrap_or_default();
            if let Some(position) = parts.get(2).and_then(|pos| pos.parse::<usize>().ok()) {
                remove = vec![position.to_string()];
            }
            let Some((playlist, ())) = state.personal_playlists.edit(id, |list| {
                let mut positions: Vec<usize> = remove
                    .into_iter()
                    .filter_map(|value| value.trim().parse::<usize>().ok())
                    .filter(|at| *at >= 1 && *at <= list.entries.len())
                    .map(|at| at - 1)
                    .collect();
                positions.sort_unstable();
                positions.dedup();
                for at in positions.iter().rev() {
                    list.entries.remove(*at);
                }
                list.changed = chrono::Utc::now();
            }) else {
                return Ok(Some(native_error(StatusCode::NOT_FOUND, "Playlist not found")));
            };
            mirror_local_ids(state, request.proxy, request.parameters, &playlist).await;
            return Ok(Some(native_ok(json!({ "id": id }))));
        }
        if request.method == axum::http::Method::PUT && parts.len() >= 3 {
            // Drag-and-drop: move position `from` before position `insert_before`, both
            // 1-based. The bounds are checked against the LIVE list inside the edit, so a
            // parallel change cannot make the move land wrong.
            let Some(from) = parts.get(2).and_then(|pos| pos.parse::<usize>().ok()) else {
                return Ok(Some(native_error(
                    StatusCode::BAD_REQUEST,
                    "Invalid playlist position",
                )));
            };
            let to = body
                .as_ref()
                .and_then(|body| body.get("insert_before"))
                .and_then(|value| {
                    value
                        .as_str()
                        .map(str::to_string)
                        .or_else(|| value.as_i64().map(|n| n.to_string()))
                })
                .and_then(|value| value.trim().parse::<usize>().ok());
            let Some(to) = to else {
                return Ok(Some(native_error(
                    StatusCode::BAD_REQUEST,
                    "Invalid playlist position",
                )));
            };
            let Some((playlist, moved)) = state.personal_playlists.edit(id, |list| {
                if from == 0 || from > list.entries.len() || to == 0 || to > list.entries.len() + 1 {
                    return false;
                }
                let moved = list.entries.remove(from - 1);
                let at = if to > from { to - 2 } else { to - 1 }.min(list.entries.len());
                list.entries.insert(at, moved);
                list.changed = chrono::Utc::now();
                true
            }) else {
                return Ok(Some(native_error(StatusCode::NOT_FOUND, "Playlist not found")));
            };
            if !moved {
                return Ok(Some(native_error(
                    StatusCode::BAD_REQUEST,
                    "Invalid playlist position",
                )));
            }
            mirror_local_ids(state, request.proxy, request.parameters, &playlist).await;
            return Ok(Some(native_ok(json!({ "id": parts[2] }))));
        }
        return Ok(Some(native_error(
            StatusCode::BAD_REQUEST,
            "Unsupported playlist mutation",
        )));
    }

    // The playlist itself: the detail, the metadata edit, or the delete.
    match request.method.clone() {
        axum::http::Method::GET => Ok(Some(native_detail(&playlist))),
        axum::http::Method::DELETE => {
            let raw = request
                .proxy
                .relay_raw(&format!("api/playlist/{id}"), request.parameters)
                .await?;
            if !(200..300).contains(&raw.status) && raw.status != 404 {
                return Ok(Some(native_status(raw.status, &raw.body)));
            }
            state.personal_playlists.remove(id);
            Ok(Some(native_ok(json!({ "id": id }))))
        }
        axum::http::Method::PUT => {
            let name = body
                .as_ref()
                .and_then(|body| body.get("name"))
                .and_then(Value::as_str)
                .map(str::to_string);
            let comment = body
                .as_ref()
                .and_then(|body| body.get("comment"))
                .and_then(Value::as_str)
                .map(str::to_string);
            let public = body
                .as_ref()
                .and_then(|body| body.get("public"))
                .and_then(Value::as_bool);
            let asks_to_download = comment.as_deref().is_some_and(comment_asks_to_download);
            // A `tracks` row list is a whole-list replacement — even an empty one clears the
            // playlist — so the key's presence decides the replacement; the adoption gate above
            // already settled whether the overlay handles this playlist at all. The new rows
            // are prepared first, so the replacement is one atomic edit (O3).
            let replaces = body
                .as_ref()
                .and_then(|body| body.get("tracks"))
                .and_then(Value::as_array)
                .is_some();
            let fresh = if replaces {
                Some(entries_for(state, &track_ids).await)
            } else {
                None
            };
            let Some((playlist, ())) = state.personal_playlists.edit(id, |list| {
                apply_metadata(list, name.as_deref(), comment.as_deref(), public);
                if let Some(fresh) = fresh {
                    list.entries.clear();
                    list.entries.extend(fresh);
                }
                list.changed = chrono::Utc::now();
            }) else {
                return Ok(Some(native_error(StatusCode::NOT_FOUND, "Playlist not found")));
            };
            let owner = playlist.owner.clone();
            mirror_local_ids(state, request.proxy, request.parameters, &playlist).await;
            let playlist = backfill_local_snapshots(state, request.proxy, request.parameters, &playlist)
                .await
                .unwrap_or(playlist);
            if asks_to_download {
                let requested_by = super::caller_checks::requester_for(state, Some(owner.as_str()));
                super::playlists::queue_missing(state, &playlist, requested_by.as_deref());
            }
            Ok(Some(native_detail(&playlist)))
        }
        _ => Ok(Some(native_error(
            StatusCode::BAD_REQUEST,
            "Unsupported playlist mutation",
        ))),
    }
}

/// `POST /api/playlist`: the native create. A body whose `tracks` rows carry an outside id is
/// the overlay's — Navidrome would refuse the registry's id whole (Fakt A1) — so the empty
/// playlist is made in Navidrome first (no song id on that create, am-yy's move), the real id
/// comes back, and the overlay keeps every position with the local half mirrored alone. A body
/// without `tracks`, or a purely local one, is none of the overlay's business (`None`: the
/// faithful relay takes the create to Navidrome as it came).
async fn native_create(
    state: &AppState,
    request: &NativeRequest<'_>,
) -> Result<Option<Response>, crate::http::error::AppError> {
    let body = native_body(request);
    let tracks = native_track_ids(&body);
    if tracks.is_empty() || !tracks.iter().any(|id| state.local_library.parse_song_id(id).0) {
        return Ok(None);
    }
    let name = body
        .as_ref()
        .and_then(|body| body.get("name"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let comment = body
        .as_ref()
        .and_then(|body| body.get("comment"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let public = body
        .as_ref()
        .and_then(|body| body.get("public"))
        .and_then(Value::as_bool);
    let auth = auth_of(state, request.parameters);
    let Some(id) = create_empty_playlist(request.proxy, &auth, name.as_deref()).await else {
        return Ok(Some(native_error(
            StatusCode::BAD_GATEWAY,
            "Navidrome made no playlist",
        )));
    };
    let owner =
        super::caller_checks::listener_name_of(state, request.parameters, request.proxy, request.headers)
            .await;
    let mut playlist = PersonalPlaylist {
        id,
        owner,
        ..Default::default()
    };
    apply_metadata(&mut playlist, name.as_deref(), comment.as_deref(), public);
    playlist.entries.extend(entries_for(state, &tracks).await);
    playlist.changed = chrono::Utc::now();
    // A brand-new id cannot race anyone: a plain whole write.
    state.personal_playlists.upsert(playlist.clone());
    mirror_local_ids(state, request.proxy, request.parameters, &playlist).await;
    let playlist = backfill_local_snapshots(state, request.proxy, request.parameters, &playlist)
        .await
        .unwrap_or(playlist);
    Ok(Some(native_detail(&playlist)))
}

/// The request's JSON body, when it carries one (POST/PUT/PATCH, `IncomingRequest::raw_body`).
pub(crate) fn native_body(request: &NativeRequest<'_>) -> Option<Value> {
    request
        .proxy
        .incoming()
        .and_then(|incoming| incoming.raw_body())
        .and_then(|body| serde_json::from_slice(body).ok())
}

/// The song ids a native body carried (`ids`).
pub(crate) fn native_ids(body: &Option<Value>) -> Vec<String> {
    body.as_ref()
        .and_then(|body| body.get("ids"))
        .and_then(Value::as_array)
        .map(|ids| {
            ids.iter()
                .filter_map(|id| id.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// The song ids a native body's `tracks` rows carried — `mediaFileId` (what a native client
/// sends on a create or a list replacement) or `id` — blanks left out.
pub(crate) fn native_track_ids(body: &Option<Value>) -> Vec<String> {
    body.as_ref()
        .and_then(|body| body.get("tracks"))
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .map(|row| {
                    row.get("mediaFileId")
                        .or_else(|| row.get("id"))
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string()
                })
                .filter(|id| !id.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// The playlist's native detail row (a `GET /api/playlist/{id}` answer).
fn native_detail(playlist: &PersonalPlaylist) -> Response {
    native_ok(json!({
        "id": playlist.id,
        "name": playlist.display_name(),
        "comment": playlist.display_comment(),
        "ownerName": playlist.owner,
        "public": playlist.public,
        "songCount": playlist.entries.len(),
        "duration": seconds_of(playlist),
        "updatedAt": playlist.changed.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        "path": "",
        "smartPlaylist": false,
        "readonly": false,
    }))
}

/// The discovery label a position's rows badge by: the position's own record first — kept at
/// add time, because `Song.suggested_by` is never stored and survives an eviction with the
/// position — else the registry routing's own label, which an older position (added before it
/// kept one) still badges by while the registry knows the row's origin.
fn position_source(state: &AppState, entry: &PersonalPlaylistEntry) -> Option<String> {
    if let Some(source) = entry.source.as_deref().filter(|by| !by.is_empty()) {
        return Some(source.to_string());
    }
    let id = entry.outside_id.as_deref()?;
    state
        .external_id_registry
        .lookup(id)
        .map(|shared| shared.snapshot())
        .and_then(|routing| routing.suggested_by)
}

/// The playlist's native tracks: one row per position, `id` 1..N (what Feishin moves and removes
/// by), `mediaFileId` the source the position plays by right now — the same one read of the
/// playlist's mirror and the same store resolution the REST answer goes by, so a file that went
/// takes its position to the stream at once, without waiting for the reconciler's minute. The
/// row's words come from the mirror's live row first (an adopted position has no snapshot), else
/// the position's snapshot. Outside positions are registered for the prewarm.
async fn native_tracks(
    state: &AppState,
    proxy: &crate::services::subsonic::SubsonicProxyService,
    parameters: &Parameters,
    playlist: &PersonalPlaylist,
) -> Response {
    let (sources, mirror) = served_sources(state, proxy, parameters, playlist).await;
    let mut rows: Vec<Value> = Vec::new();
    let mut outside_ids: Vec<String> = Vec::new();
    for (at, (entry, source)) in zipped(playlist, &sources).into_iter().enumerate() {
        let media = match source {
            ServeSource::Library { id, .. } => id.clone(),
            ServeSource::Outside { outside_id, .. } => {
                outside_ids.push(outside_id.clone());
                outside_id.clone()
            }
        };
        let live = match source {
            ServeSource::Library { id, .. } => mirror.as_ref().and_then(|rows| rows.get(id)),
            ServeSource::Outside { .. } => None,
        };
        let mut row = match live {
            Some(row) => json!({
                "title": row.get("title").and_then(Value::as_str).unwrap_or_default(),
                "artist": row.get("artist").and_then(Value::as_str).unwrap_or_default(),
                "album": row.get("album").and_then(Value::as_str).unwrap_or_default(),
                "duration": row.get("duration").and_then(Value::as_i64).unwrap_or(0),
            }),
            None => match &entry.song_snapshot {
                Some(snapshot) => json!({
                    "title": snapshot.title,
                    "artist": snapshot.artist,
                    "album": snapshot.album,
                    "duration": snapshot.duration.unwrap_or(0),
                }),
                None => json!({}),
            },
        };
        let fields = row.as_object_mut().expect("an object");
        // The source badge (option A), the native rows' own: an outside position's row carries
        // its source as a genre — the same mapping and the same merge the single-song object
        // applies (`native_song_object`) and the REST serializer does (`convert_song_fields`),
        // the snapshot's own genres first and the source appended after them, never twice. A
        // library position's row keeps the slim shape it always had.
        if matches!(source, ServeSource::Outside { .. })
            && let Some(source) = position_source(state, entry)
                .as_deref()
                .and_then(source_genre_for)
        {
            let own = entry
                .song_snapshot
                .as_ref()
                .and_then(|song| song.genre.as_deref());
            let (genre, genres) = genres_with_source(own, source);
            fields.insert("genre".into(), genre.into());
            fields.insert("genres".into(), Value::Array(genres));
        }
        fields.insert("id".into(), (at + 1).into());
        fields.insert("mediaFileId".into(), media.into());
        fields.insert("playlistId".into(), playlist.id.clone().into());
        rows.push(row);
    }
    state.radio_queues.register(outside_ids.iter().cloned());
    let body = serde_json::to_vec(&rows).unwrap_or_default();
    let mut response = SubsonicReply::file(body, "application/json").into_response();
    response.headers_mut().insert(
        HeaderName::from_static("x-total-count"),
        HeaderValue::from(rows.len()),
    );
    response
}

fn native_ok(body: Value) -> Response {
    let body = serde_json::to_vec(&body).unwrap_or_default();
    SubsonicReply::file(body, "application/json").into_response()
}

fn native_error(status: StatusCode, message: &str) -> Response {
    let body = serde_json::to_vec(&json!({ "error": message })).unwrap_or_default();
    let mut response = SubsonicReply::file(body, "application/json").into_response();
    *response.status_mut() = status;
    response
}

fn native_status(status: u16, body: &[u8]) -> Response {
    let mut response = SubsonicReply::file(body.to_vec(), "application/json").into_response();
    *response.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
    response
}

#[cfg(test)]
#[path = "personal_playlists_tests.rs"]
mod tests;
