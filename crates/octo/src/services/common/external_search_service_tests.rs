//! The external search. Last.fm is a wiremock server answering by the `method` query
//! parameter; the metadata service is a hand-written fake.

use std::sync::Arc;
use std::time::Duration;

use futures::FutureExt;
use octo_core::radio::radio_provider;
use octo_core::settings::{AppSettings, LastFmSettings, SubsonicSettings};
use tokio::sync::watch;
use wiremock::matchers::{any, path, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

use super::*;
use crate::services::common::test_fakes::FakeMetadata;
use crate::services::soulseek::ExternalIdRegistry;
use crate::services::you_tube::YouTubeResolver;
use crate::services::you_tube::you_tube_music_client::YouTubeMusicClient;

/// `ExternalSearchService::new` with YouTube sources pointed at an address that refuses at
/// once, so the tests of the catalog half stay about the catalogs.
fn no_you_tube_search(
    metadata: Arc<FakeMetadata>,
    last_fm: Option<Arc<LastFmService>>,
    settings: Option<Arc<SettingsStore>>,
) -> ExternalSearchService {
    let refused = format!("http://{}", crate::services::test_support::refused_address());
    ExternalSearchService::new(
        metadata,
        last_fm,
        settings,
        Arc::new(YouTubeMusicClient::with_shim_url(Some(&refused))),
        Arc::new(YouTubeResolver::with_base_url(Some(&refused))),
        Arc::new(ExternalIdRegistry::in_memory()),
    )
}

/// A Last.fm whose track.search answers `hits` rows ("Song 1".. by "Artist") and whose
/// artist.gettoptracks answers none.
async fn last_fm(hits: usize) -> (Arc<LastFmService>, MockServer) {
    let server = MockServer::builder().start().await;
    Mock::given(any())
        .respond_with(move |request: &Request| {
            let url = request.url.to_string();
            let tracks: Vec<String> = (1..=hits)
                .map(|i| format!(r#"{{"name":"Song {i}","artist":"Artist"}}"#))
                .collect();
            let body = if url.contains("method=track.search") {
                format!(
                    r#"{{"results":{{"trackmatches":{{"track":[{}]}}}}}}"#,
                    tracks.join(",")
                )
            } else {
                r#"{"toptracks":{"track":[]}}"#.to_string()
            };
            ResponseTemplate::new(200).set_body_string(body)
        })
        .mount(&server)
        .await;
    let settings = Arc::new(SettingsStore::for_tests(AppSettings {
        last_fm: LastFmSettings {
            api_key: "key".into(),
            ..Default::default()
        },
        ..Default::default()
    }));
    let service = Arc::new(LastFmService::with_base_url(
        settings,
        &format!("{}/", server.uri()),
    ));
    (service, server)
}

/// `OneHitLastFm`: every call answers one track.search row, Artist - Song.
async fn one_hit_last_fm() -> (Arc<LastFmService>, MockServer) {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(
            ResponseTemplate::new(200).set_body_string(
                r#"{"results":{"trackmatches":{"track":[{"name":"Song","artist":"Artist"}]}}}"#,
            ),
        )
        .mount(&server)
        .await;
    let settings = Arc::new(SettingsStore::for_tests(AppSettings {
        last_fm: LastFmSettings {
            api_key: "key".into(),
            ..Default::default()
        },
        ..Default::default()
    }));
    (
        Arc::new(LastFmService::with_base_url(
            settings,
            &format!("{}/", server.uri()),
        )),
        server,
    )
}

fn waiting(wait_for_search_durations: bool) -> Option<Arc<SettingsStore>> {
    Some(Arc::new(SettingsStore::for_tests(AppSettings {
        subsonic: SubsonicSettings {
            wait_for_search_durations,
            ..Default::default()
        },
        ..Default::default()
    })))
}

fn waiting_ms(search_duration_wait_ms: i32) -> Option<Arc<SettingsStore>> {
    Some(Arc::new(SettingsStore::for_tests(AppSettings {
        subsonic: SubsonicSettings {
            search_duration_wait_ms,
            ..Default::default()
        },
        ..Default::default()
    })))
}

/// How many track.search calls Last.fm has had.
async fn track_searches(server: &MockServer) -> usize {
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|r| r.url.to_string().contains("method=track.search"))
        .count()
}

fn one_song_metadata() -> Arc<FakeMetadata> {
    let metadata = Arc::new(FakeMetadata::default());
    metadata.hits.lock().insert(
        ("Artist".into(), "Song".into()),
        Song {
            artist: "Artist".into(),
            title: "Song".into(),
            ..Default::default()
        },
    );
    metadata
}

/// A gate the durations pass waits on, and the background flag it was called with.
struct DurationsGate {
    open: watch::Sender<bool>,
    started: watch::Receiver<Option<bool>>,
}

fn gate_durations(metadata: &FakeMetadata) -> DurationsGate {
    let (open, opened) = watch::channel(false);
    let (started_tx, started) = watch::channel(None);
    let started_tx = Arc::new(started_tx);
    *metadata.durations.lock() = Some(Arc::new(move |background| {
        started_tx.send_replace(Some(background));
        let mut opened = opened.clone();
        async move {
            let _ = opened.wait_for(|open| *open).await;
        }
        .boxed()
    }));
    DurationsGate { open, started }
}

#[tokio::test]
async fn pads_with_top_tracks_only_when_track_search_is_thin() {
    for (search_hits, expected_top_track_calls) in [(50, 0), (5, 1)] {
        let (last_fm, server) = last_fm(search_hits).await;
        let search = no_you_tube_search(Arc::new(FakeMetadata::default()), Some(last_fm), None);

        search.get("artist").await;

        let top_track_calls = server
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter(|r| r.url.to_string().contains("method=artist.gettoptracks"))
            .count();
        assert_eq!(top_track_calls, expected_top_track_calls, "{search_hits} hits");
    }
}

#[tokio::test]
async fn search_waits_for_you_tube_durations_only_when_configured() {
    for wait_for_durations in [true, false] {
        let metadata = one_song_metadata();
        let mut gate = gate_durations(&metadata);
        let (last_fm, _server) = one_hit_last_fm().await;
        let search = Arc::new(no_you_tube_search(
            metadata.clone(),
            Some(last_fm),
            waiting(wait_for_durations),
        ));

        let running = search.clone();
        let mut result = tokio::spawn(async move { running.get("song").await });
        let background = tokio::time::timeout(Duration::from_secs(5), gate.started.wait_for(Option::is_some))
            .await
            .expect("the durations pass started")
            .expect("open")
            .expect("a flag");

        // The durations pass is still running, so only a search that does not wait can be done.
        assert_eq!(background, !wait_for_durations);
        if wait_for_durations {
            assert!(!result.is_finished());
        } else {
            let songs = tokio::time::timeout(Duration::from_secs(5), &mut result)
                .await
                .expect("in time")
                .expect("joined");
            assert_eq!(songs.len(), 1);
        }
        gate.open.send_replace(true);
        if wait_for_durations {
            let songs = tokio::time::timeout(Duration::from_secs(5), result)
                .await
                .expect("in time")
                .expect("joined");
            assert_eq!(songs.len(), 1);
        }
    }
}

#[tokio::test]
async fn background_durations_run_before_the_video_prewarm() {
    let metadata = one_song_metadata();
    let mut gate = gate_durations(&metadata);
    let (prewarmed_tx, mut prewarmed) = watch::channel(false);
    let prewarmed_tx = Arc::new(prewarmed_tx);
    *metadata.prewarmed.lock() = Some(Arc::new(move || {
        prewarmed_tx.send_replace(true);
    }));
    let (last_fm, _server) = one_hit_last_fm().await;
    let search = no_you_tube_search(metadata.clone(), Some(last_fm), waiting(false));

    tokio::time::timeout(Duration::from_secs(5), search.get("song"))
        .await
        .expect("in time");
    tokio::time::timeout(Duration::from_secs(5), gate.started.wait_for(Option::is_some))
        .await
        .expect("the durations pass started")
        .expect("open");
    assert!(!*prewarmed.borrow());

    gate.open.send_replace(true);
    tokio::time::timeout(Duration::from_secs(5), prewarmed.wait_for(|done| *done))
        .await
        .expect("prewarmed")
        .expect("open");
}

/// track.search answers in fuzzy-relevance order; the rows are built most listened first, the
/// live take of a song is folded into the song, and the row in another script comes last.
#[tokio::test]
async fn search_rows_are_built_most_listened_first() {
    let rows = [
        ("Skyfall", "Adele", 1_430_000),
        ("Rolling in the Deep", "Adele", 2_680_000),
        ("Rolling in the Deep (Live)", "Adele", 9_000),
        ("Привет", "Адель", 5_000_000),
        ("Someone Like You", "Adele", 2_200_000),
    ];
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(move |request: &Request| {
            let tracks: Vec<String> = rows
                .iter()
                .map(|(name, artist, listeners)| {
                    format!(r#"{{"name":"{name}","artist":"{artist}","listeners":"{listeners}"}}"#)
                })
                .collect();
            let body = if request.url.to_string().contains("method=track.search") {
                format!(
                    r#"{{"results":{{"trackmatches":{{"track":[{}]}}}}}}"#,
                    tracks.join(",")
                )
            } else {
                r#"{"toptracks":{"track":[]}}"#.to_string()
            };
            ResponseTemplate::new(200).set_body_string(body)
        })
        .mount(&server)
        .await;
    let settings = Arc::new(SettingsStore::for_tests(AppSettings {
        last_fm: LastFmSettings {
            api_key: "key".into(),
            ..Default::default()
        },
        ..Default::default()
    }));
    let last_fm = Arc::new(LastFmService::with_base_url(
        settings,
        &format!("{}/", server.uri()),
    ));
    let metadata = Arc::new(FakeMetadata::default());
    for (title, artist, _) in rows {
        metadata.hits.lock().insert(
            (artist.into(), title.into()),
            Song {
                artist: artist.into(),
                title: title.into(),
                ..Default::default()
            },
        );
    }
    let search = no_you_tube_search(metadata, Some(last_fm), waiting(false));

    let songs = search.get("adele").await;

    let titles: Vec<&str> = songs.iter().map(|song| song.title.as_str()).collect();
    assert_eq!(
        titles,
        ["Rolling in the Deep", "Someone Like You", "Skyfall", "Привет"]
    );
}
// ---- empty inputs and limits -------------------------------------------------------------

/// A search waits for the lengths at most its budget, then answers with what it has.
#[tokio::test]
async fn a_search_answers_when_its_wait_for_lengths_runs_out() {
    let metadata = one_song_metadata();
    let mut gate = gate_durations(&metadata);
    let (last_fm, _server) = one_hit_last_fm().await;
    let search = no_you_tube_search(metadata.clone(), Some(last_fm), waiting_ms(200));

    let started = std::time::Instant::now();
    let songs = tokio::time::timeout(Duration::from_secs(5), search.get("song"))
        .await
        .expect("answered while the lengths are still being looked up");

    assert_eq!(songs.len(), 1);
    assert!(started.elapsed() >= Duration::from_millis(200));
    assert_eq!(*gate.started.borrow_and_update(), Some(false));
    gate.open.send_replace(true);
}

/// A wait of 0 is the background mode: the search does not wait at all.
#[tokio::test]
async fn a_wait_of_zero_never_waits() {
    let metadata = one_song_metadata();
    let mut gate = gate_durations(&metadata);
    let (last_fm, _server) = one_hit_last_fm().await;
    let search = no_you_tube_search(metadata.clone(), Some(last_fm), waiting_ms(0));

    let songs = tokio::time::timeout(Duration::from_secs(5), search.get("song"))
        .await
        .expect("answered");
    assert_eq!(songs.len(), 1);
    let background = tokio::time::timeout(Duration::from_secs(5), gate.started.wait_for(Option::is_some))
        .await
        .expect("the durations pass started")
        .expect("open")
        .expect("a flag");
    assert!(background);
    gate.open.send_replace(true);
}

/// The same words again, in any case and spacing, answer from the last build: Last.fm is not
/// asked again, the rows take the lengths found since, and the kept list itself is unchanged.
#[tokio::test]
async fn a_repeated_query_answers_from_the_last_build_with_lengths_found_since() {
    let metadata = one_song_metadata();
    let (last_fm, server) = one_hit_last_fm().await;
    let search = no_you_tube_search(metadata.clone(), Some(last_fm), waiting(false));

    let first = search.get("song").await;
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].duration, None);
    metadata.lengths_now.lock().insert("Song".into(), 245);

    let again = search.get("  SONG ").await;

    assert_eq!(track_searches(&server).await, 1);
    assert_eq!(again.len(), 1);
    assert_eq!(again[0].duration, Some(245));
    assert_eq!(first[0].duration, None);
    assert_eq!(search.get("song").await[0].duration, Some(245));
    // Another query is built.
    search.get("song two").await;
    assert_eq!(track_searches(&server).await, 2);
}

/// A build with no rows (Last.fm found nothing, or it failed) is not kept.
#[tokio::test]
async fn a_build_with_no_rows_is_built_again() {
    let (last_fm, server) = last_fm(0).await;
    let search = no_you_tube_search(Arc::new(FakeMetadata::default()), Some(last_fm), waiting(false));

    assert!(search.get("nothing").await.is_empty());
    assert!(search.get("nothing").await.is_empty());

    assert_eq!(track_searches(&server).await, 2);
}

#[tokio::test]
async fn album_searches_are_kept_by_query_and_limit() {
    let metadata = Arc::new(FakeMetadata::default());
    metadata.album_hits.lock().push(Album {
        title: "Album".into(),
        ..Default::default()
    });
    let search = no_you_tube_search(metadata.clone(), None, None);

    assert_eq!(search.get_albums("Album", 5).await.len(), 1);
    assert_eq!(search.get_albums(" album", 5).await.len(), 1);
    assert_eq!(*metadata.album_searches.lock(), 1);
    search.get_albums("album", 10).await;
    assert_eq!(*metadata.album_searches.lock(), 2);

    let empty = Arc::new(FakeMetadata::default());
    let search = no_you_tube_search(empty.clone(), None, None);
    search.get_albums("none", 5).await;
    search.get_albums("none", 5).await;
    assert_eq!(*empty.album_searches.lock(), 2);
}

#[test]
fn kept_results_expire_after_their_time_and_the_oldest_go_past_the_limit() {
    let kept: RecentResults<i32> = RecentResults::new(Duration::from_secs(600), 2);
    let start = Instant::now();
    kept.put(recent_key(" A "), Arc::new(vec![1]), start);
    assert_eq!(
        kept.get("a", start + Duration::from_secs(599)).as_deref(),
        Some(&vec![1])
    );
    assert!(kept.get("a", start + Duration::from_secs(600)).is_none());
    // Gone once expired.
    assert!(kept.get("a", start).is_none());

    kept.put("a".into(), Arc::new(vec![1]), start);
    kept.put("b".into(), Arc::new(vec![2]), start);
    kept.put("a".into(), Arc::new(vec![10]), start);
    kept.put("c".into(), Arc::new(vec![3]), start);
    // "b" was the oldest once "a" was kept again.
    assert!(kept.get("b", start).is_none());
    assert_eq!(kept.get("a", start).as_deref(), Some(&vec![10]));
    assert_eq!(kept.get("c", start).as_deref(), Some(&vec![3]));
    assert_eq!(RECENT_RESULTS_TTL, Duration::from_secs(600));
    assert_eq!(RECENT_RESULTS_MAX, 256);
}

/// No Last.fm key, or a blank query, is no discovery; a blank album query or limit is none.
#[tokio::test]
async fn nothing_to_search_with_is_no_results() {
    let metadata = one_song_metadata();
    let search = no_you_tube_search(metadata.clone(), None, None);
    assert!(search.get("song").await.is_empty());
    let (last_fm, _server) = one_hit_last_fm().await;
    let search = no_you_tube_search(metadata, Some(last_fm), None);
    assert!(search.get("   ").await.is_empty());
    assert!(search.get_albums("", 5).await.is_empty());
    assert!(search.get_albums("song", 0).await.is_empty());
}

/// A Last.fm whose track.search answers one row, Artist - Song, after `delay`.
async fn slow_last_fm(delay: Duration) -> (Arc<LastFmService>, MockServer) {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(
                    r#"{"results":{"trackmatches":{"track":[{"name":"Song","artist":"Artist"}]}}}"#,
                )
                .set_delay(delay),
        )
        .mount(&server)
        .await;
    let settings = Arc::new(SettingsStore::for_tests(AppSettings {
        last_fm: LastFmSettings {
            api_key: "key".into(),
            ..Default::default()
        },
        ..Default::default()
    }));
    (
        Arc::new(LastFmService::with_base_url(
            settings,
            &format!("{}/", server.uri()),
        )),
        server,
    )
}

fn last_fm_wait(ms: i32) -> Option<Arc<SettingsStore>> {
    Some(Arc::new(SettingsStore::for_tests(AppSettings {
        subsonic: SubsonicSettings {
            wait_for_search_durations: false,
            search_last_fm_wait_ms: ms,
            ..Default::default()
        },
        ..Default::default()
    })))
}

fn catalog_metadata() -> Arc<FakeMetadata> {
    let metadata = one_song_metadata();
    metadata.hits.lock().insert(
        ("Deezer Artist".into(), "Deezer Song".into()),
        Song {
            artist: "Deezer Artist".into(),
            title: "Deezer Song".into(),
            ..Default::default()
        },
    );
    *metadata.catalog_tracks.lock() = Some(vec![
        ("Deezer Artist".into(), "Deezer Song".into()),
        ("deezer artist".into(), "DEEZER SONG".into()),
    ]);
    metadata
}

/// Last.fm late: the rows are the catalog's own track search, deduped, without waiting for
/// Last.fm. Its answer is kept, and the next search of the words uses it without asking again.
#[tokio::test]
async fn a_late_last_fm_gives_way_to_the_catalog_track_search_and_is_kept_for_next_time() {
    let metadata = catalog_metadata();
    let (last_fm, server) = slow_last_fm(Duration::from_millis(800)).await;
    let search = no_you_tube_search(metadata, Some(last_fm), last_fm_wait(150));

    let started = std::time::Instant::now();
    let first = search.get("song").await;
    assert!(
        started.elapsed() < Duration::from_millis(700),
        "{:?}",
        started.elapsed()
    );
    let titles: Vec<&str> = first.iter().map(|song| song.title.as_str()).collect();
    assert_eq!(titles, ["Deezer Song"]);

    // Once Last.fm has answered, the kept build gives way and the next search uses its answer.
    tokio::time::sleep(Duration::from_millis(1000)).await;
    let next = search.get("song").await;
    let titles: Vec<&str> = next.iter().map(|song| song.title.as_str()).collect();
    assert_eq!(titles, ["Song"]);
    assert_eq!(track_searches(&server).await, 1);
}

/// Last.fm late and the catalog with nothing: Last.fm is waited for after all.
#[tokio::test]
async fn a_late_last_fm_is_waited_for_when_the_catalog_has_nothing() {
    let metadata = one_song_metadata();
    let (last_fm, _server) = slow_last_fm(Duration::from_millis(400)).await;
    let search = no_you_tube_search(metadata, Some(last_fm), last_fm_wait(100));

    let songs = search.get("song").await;

    assert_eq!(songs.len(), 1);
    assert_eq!(songs[0].title, "Song");
}

/// A wait of 0 waits for Last.fm however long it takes.
#[tokio::test]
async fn a_last_fm_wait_of_zero_never_uses_the_catalog_search() {
    let metadata = catalog_metadata();
    let (last_fm, _server) = slow_last_fm(Duration::from_millis(300)).await;
    let search = no_you_tube_search(metadata, Some(last_fm), last_fm_wait(0));

    let songs = search.get("song").await;

    assert_eq!(songs[0].title, "Song");
}

/// Album ownership is let go once the song build has looked its rows up, while the build
/// still waits for lengths; with the songs kept from before, at once.
#[tokio::test]
async fn row_lookups_are_done_before_the_wait_for_lengths_and_at_once_from_the_kept_build() {
    let metadata = one_song_metadata();
    let gate = gate_durations(&metadata);
    let (last_fm, _server) = one_hit_last_fm().await;
    let search = Arc::new(no_you_tube_search(metadata, Some(last_fm), waiting_ms(5000)));

    let looked_up = search.rows_looked_up("song");
    let running = search.clone();
    let build = tokio::spawn(async move { running.get("song").await });
    tokio::time::timeout(Duration::from_secs(3), looked_up)
        .await
        .expect("let go while the lengths are still awaited");
    assert!(!build.is_finished());
    gate.open.send_replace(true);
    build.await.expect("built");

    let again = search.rows_looked_up("song");
    search.get("song").await;
    tokio::time::timeout(Duration::from_millis(200), again)
        .await
        .expect("at once from the kept build");
}

// ---- The YouTube half: interleave, dedup, and no Last.fm key (D2/D3/D4) ---------------------

/// A row of a `/ytm/search` answer.
fn ytm_row(id: &str, title: &str, artist: &str, seconds: Option<i32>) -> YtmRow {
    YtmRow {
        video_id: id.into(),
        title: title.into(),
        artists: vec![artist.into()],
        album: None,
        duration_seconds: seconds,
        video_type: None,
    }
}

/// The query's own rows lead the answer, the video ahead of the song (W3): for K3KHA - Lost in
/// Sound the videos filter has the upload the query means while the songs catalog answers the
/// release of the same recording (and other artists' tracks of the same name) — a video and a
/// song are two different rows, so the video leads and the song follows it (PoC-1's measured
/// case, re-ruled by the Admiral's order).
#[test]
fn the_querys_own_rows_lead_and_the_video_precedes_the_song() {
    let songs = vec![
        ytm_row("jenks111111", "Lost In The Sound", "Jenks (UK)", Some(360)),
        ytm_row("keyeff22222", "Lost in the Sound", "Key Eff", Some(312)),
        ytm_row("rwqD5PrajEA", "Lost in Sound", "K3KHA", Some(619)),
    ];
    let videos = vec![
        ytm_row("vBvDiLKTDWA", "Lost in Sound", "K3KHA", Some(619)),
        ytm_row(
            "R-JnkBpJuNs",
            "K3KHA – Deep Minimal Techno Set 2025",
            "K3KHA",
            Some(3403),
        ),
    ];

    let (exact, rest) = split_ytm_rows("K3KHA - Lost in Sound", songs, videos);

    let exact_ids: Vec<&str> = exact.iter().map(|(row, _)| row.video_id.as_str()).collect();
    // W3: the upload the query means is the answer, ahead of the release of the same recording.
    assert_eq!(exact_ids, ["vBvDiLKTDWA", "rwqD5PrajEA"], "{exact_ids:?}");
    let rest_ids: Vec<&str> = rest.iter().map(|(row, _)| row.video_id.as_str()).collect();
    // The rest keep the interleaved order: songs[i] before videos[i], row by row.
    assert_eq!(
        rest_ids,
        ["jenks111111", "keyeff22222", "R-JnkBpJuNs"],
        "{rest_ids:?}"
    );
}

/// One YouTube-origin row of the dedup: the song as registered, the video it was minted with,
/// and the service it came from.
fn you_tube_row(
    artist: &str,
    title: &str,
    duration: Option<i32>,
    isrc: Option<&str>,
    music_brainz: Option<&str>,
    you_tube_id: Option<&str>,
    origin: RowOrigin,
) -> YouTubeSong {
    let mut song = Song {
        artist: artist.into(),
        title: title.into(),
        duration,
        isrc: isrc.map(str::to_string),
        music_brainz_recording_id: music_brainz.map(str::to_string),
        ..Default::default()
    };
    song.suggested_by = Some(origin.label().to_string());
    YouTubeSong {
        song,
        you_tube_id: you_tube_id.map(str::to_string),
        origin,
    }
}

/// The version-aware dedup folds only what it may (Correction B, re-ruled by the Admiral's
/// order): the video and the song of the same recording stay two rows — one YouTube Music
/// upload and one release, the user's choice, not the algorithm's — while the very same video
/// seen from two services folds into one enriched row and the songs catalog folds its own
/// duplicates (a shared ISRC, the same key with a close length). The winner keeps its video,
/// its length and its service's label, taking only what it lacks. Six versions stay six rows.
#[test]
fn the_dedup_folds_only_what_it_may_and_keeps_versions_apart() {
    let rows = vec![
        // The videos filter's hit: the upload the query means (W3's row), without its
        // MusicBrainz recording yet.
        you_tube_row(
            "K3KHA",
            "Lost in Sound",
            Some(619),
            Some("USK3K2600001"),
            None,
            Some("vBvDiLKTDWA"),
            RowOrigin::Videos,
        ),
        // The songs catalog's release of the same recording: its own row. A video and a song
        // never merge, whatever the shared ISRC and the equal length say.
        you_tube_row(
            "K3KHA",
            "Lost in Sound",
            Some(619),
            Some("USK3K2600001"),
            None,
            Some("rwqD5PrajEA"),
            RowOrigin::Songs,
        ),
        // The same video from the fallback (D11): folds into the video row (the same upload)
        // and fills the recording it lacked.
        you_tube_row(
            "K3KHA",
            "K3KHA - Lost in Sound",
            Some(619),
            None,
            Some("mb-rec-k3kha-orig"),
            Some("vBvDiLKTDWA"),
            RowOrigin::YouTube,
        ),
        // The songs catalog's own duplicate by ISRC: folds into the song row (Correction B).
        you_tube_row(
            "K3KHA",
            "Lost in Sound",
            Some(619),
            Some("USK3K2600001"),
            None,
            Some("atvRelVid007"),
            RowOrigin::Songs,
        ),
        // The songs catalog's own duplicate by key and a close length: folds too, and the
        // song row keeps the length it arrived with.
        you_tube_row(
            "K3KHA",
            "Lost in Sound",
            Some(621),
            None,
            None,
            Some("atvRelVid008"),
            RowOrigin::Songs,
        ),
        // Versions: each keeps its own row.
        you_tube_row(
            "K3KHA",
            "Lost in Sound (Extended Mix)",
            Some(480),
            None,
            None,
            Some("extMixVid01"),
            RowOrigin::Songs,
        ),
        you_tube_row(
            "K3KHA",
            "Lost in Sound (Radio Edit)",
            Some(215),
            None,
            None,
            Some("radEditVid2"),
            RowOrigin::Songs,
        ),
        you_tube_row(
            "K3KHA",
            "Lost in Sound (Live)",
            Some(510),
            None,
            None,
            Some("liveVerVid3"),
            RowOrigin::Videos,
        ),
        you_tube_row(
            "K3KHA",
            "Lost in Sound (Club Remix)",
            Some(390),
            None,
            None,
            Some("remixVerVd4"),
            RowOrigin::Songs,
        ),
        you_tube_row(
            "K3KHA",
            "Deep Minimal Techno Set 2025",
            Some(3403),
            None,
            None,
            Some("R-JnkBpJuNs"),
            RowOrigin::Videos,
        ),
    ];

    let deduped = dedupe_you_tube_songs(rows);

    let ids: Vec<&str> = deduped
        .iter()
        .map(|r| r.you_tube_id.as_deref().unwrap_or(""))
        .collect();
    assert_eq!(
        deduped.len(),
        7,
        "{:?}",
        deduped.iter().map(|r| r.song.title.clone()).collect::<Vec<_>>()
    );
    // The video and the song of the same recording are two rows, the video first, each with
    // its service's label and its own video.
    assert_eq!(ids[0], "vBvDiLKTDWA", "{ids:?}");
    assert_eq!(
        deduped[0].song.suggested_by.as_deref(),
        Some("YouTube Music (video)")
    );
    assert_eq!(deduped[0].song.duration, Some(619));
    // The fallback's copy of the same video folded in and filled the recording it lacked.
    assert_eq!(
        deduped[0].song.music_brainz_recording_id.as_deref(),
        Some("mb-rec-k3kha-orig")
    );
    assert_eq!(ids[1], "rwqD5PrajEA", "{ids:?}");
    assert_eq!(
        deduped[1].song.suggested_by.as_deref(),
        Some("YouTube Music (song)")
    );
    assert_eq!(deduped[1].song.duration, Some(619));
    // The versions stay apart, each with its service's label.
    assert_eq!(
        ids[2..],
        [
            "extMixVid01",
            "radEditVid2",
            "liveVerVid3",
            "remixVerVd4",
            "R-JnkBpJuNs"
        ],
        "{ids:?}"
    );
    assert_eq!(
        deduped[2].song.suggested_by.as_deref(),
        Some("YouTube Music (song)")
    );
    assert_eq!(
        deduped[6].song.suggested_by.as_deref(),
        Some("YouTube Music (video)")
    );
}

/// The rows the videos filter answers, as the shim writes them (K3KHA - Lost in Sound).
const K3KHA_VIDEOS: &str = r#"{"tracks":[
    {"videoId":"vBvDiLKTDWA","title":"Lost in Sound","artists":["K3KHA"],"durationSeconds":619},
    {"videoId":"R-JnkBpJuNs","title":"K3KHA – Deep Minimal Techno Set 2025","artists":["K3KHA"],"durationSeconds":3403}
]}"#;

/// Without a Last.fm key the search still answers, from YouTube Music alone (D4): the video
/// hit the query means is registered with its video and its length under the id its routing
/// hashes to (W3's row, seed-stable D6/D13), and the songs catalog's release of the same
/// recording answers as its own row beside it — a video and a song never merge, and each row
/// carries its service's label.
#[tokio::test]
async fn a_search_without_a_last_fm_key_answers_video_and_song_as_two_rows() {
    let shim = MockServer::start().await;
    Mock::given(path("/ytm/search"))
        .and(query_param("filter", "songs"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"tracks":[
                {"videoId":"rwqD5PrajEA","title":"Lost in Sound","artists":["K3KHA"],"durationSeconds":619,"album":"Weightless Orbit","videoType":"MUSIC_VIDEO_TYPE_ATV"},
                {"videoId":"jenks111111","title":"Lost In The Sound","artists":["Jenks (UK)"],"durationSeconds":360},
                {"videoId":"keyeff22222","title":"Lost in the Sound","artists":["Key Eff"],"durationSeconds":312}
            ]}"#,
        ))
        .mount(&shim)
        .await;
    Mock::given(path("/ytm/search"))
        .and(query_param("filter", "videos"))
        .respond_with(ResponseTemplate::new(200).set_body_string(K3KHA_VIDEOS))
        .mount(&shim)
        .await;
    let search = ExternalSearchService::new(
        Arc::new(FakeMetadata::default()),
        None,
        waiting(false),
        Arc::new(YouTubeMusicClient::with_shim_url(Some(&shim.uri()))),
        Arc::new(YouTubeResolver::with_base_url(Some(&shim.uri()))),
        Arc::new(ExternalIdRegistry::in_memory()),
    );

    let songs = search.get("K3KHA - Lost in Sound").await;

    assert_eq!(
        songs.len(),
        5,
        "{:?}",
        songs.iter().map(|s| (&s.artist, &s.title)).collect::<Vec<_>>()
    );
    // W3: the video first, the song after it — two rows of one recording, the user's choice.
    assert_eq!(songs[0].title, "Lost in Sound");
    assert_eq!(songs[0].duration, Some(619));
    assert_eq!(songs[0].suggested_by.as_deref(), Some("YouTube Music (video)"));
    assert_eq!(songs[1].title, "Lost in Sound");
    assert_eq!(songs[1].album, "Weightless Orbit");
    assert_eq!(songs[1].duration, Some(619));
    assert_eq!(songs[1].suggested_by.as_deref(), Some("YouTube Music (song)"));
    // The unrelated rows keep the interleaved order behind the query's own.
    let rest: Vec<&str> = songs[2..].iter().map(|s| s.title.as_str()).collect();
    assert_eq!(
        rest,
        [
            "Lost In The Sound",
            "K3KHA – Deep Minimal Techno Set 2025",
            "Lost in the Sound"
        ]
    );
    // Both rows are registered with their own video and their own length, seed-stable.
    for (row, video) in [&songs[0], &songs[1]].iter().zip(["vBvDiLKTDWA", "rwqD5PrajEA"]) {
        let routing = search.id_registry.lookup(&row.id).expect("registered").snapshot();
        assert_eq!(routing.you_tube_id.as_deref(), Some(video));
        assert_eq!(routing.duration, Some(619));
        assert_eq!(ExternalIdRegistry::make_short_id(&routing), row.id);
    }
}

/// A catalog pair covered ONLY by a video row keeps its place in the answer (the Admiral's
/// order, review-2's elephant 6): the videos filter has the upload of the query's own
/// recording while the songs catalog answers nothing of it, and the catalog's own row of that
/// recording stays beside the video row — a video row (or a plain YouTube hit) covers no
/// catalog twin, so nothing is removed. The fallback does not run (a source matched), so no
/// row of it appears either.
#[tokio::test]
async fn a_catalog_pair_covered_only_by_a_video_row_keeps_its_place_in_the_answer() {
    let (last_fm, _server) = last_fm_with_catalog_pair().await;
    let shim = MockServer::start().await;
    Mock::given(path("/ytm/search"))
        .and(query_param("filter", "songs"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"tracks":[
                {"videoId":"jenks111111","title":"Lost In The Sound","artists":["Jenks (UK)"],"durationSeconds":360}
            ]}"#,
        ))
        .mount(&shim)
        .await;
    Mock::given(path("/ytm/search"))
        .and(query_param("filter", "videos"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"tracks":[
                {"videoId":"vBvDiLKTDWA","title":"Lost in Sound","artists":["K3KHA"],"durationSeconds":619},
                {"videoId":"R-JnkBpJuNs","title":"K3KHA – Deep Minimal Techno Set 2025","artists":["K3KHA"],"durationSeconds":3403}
            ]}"#,
        ))
        .mount(&shim)
        .await;
    // The fallback is answered for (its hit would be a third row of the same recording) so
    // the test says it is never ASKED to speak while a source has matched.
    Mock::given(path("/search/list"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"hits":[{"video_id":"fbVid0000001","title":"K3KHA Lost in Sound","duration":619,"channel":"K3KHA"}]}"#,
        ))
        .mount(&shim)
        .await;
    let metadata = Arc::new(FakeMetadata::default());
    metadata.hits.lock().insert(
        ("K3KHA".into(), "Lost in Sound".into()),
        Song {
            artist: "K3KHA".into(),
            title: "Lost in Sound".into(),
            album: String::new(),
            is_local: false,
            ..Default::default()
        },
    );
    let search = ExternalSearchService::new(
        metadata,
        Some(last_fm),
        waiting(false),
        Arc::new(YouTubeMusicClient::with_shim_url(Some(&shim.uri()))),
        Arc::new(YouTubeResolver::with_base_url(Some(&shim.uri()))),
        Arc::new(ExternalIdRegistry::in_memory()),
    );

    let songs = search.get("K3KHA - Lost in Sound").await;

    // The catalog pair's own row and the video row, side by side: the answer keeps both, and
    // the unrelated rows behind them.
    assert_eq!(
        songs.len(),
        4,
        "{:?}",
        songs
            .iter()
            .map(|s| (&s.title, &s.suggested_by))
            .collect::<Vec<_>>()
    );
    let lost: Vec<&Song> = songs.iter().filter(|s| s.title == "Lost in Sound").collect();
    assert_eq!(
        lost.len(),
        2,
        "{:?}",
        songs.iter().map(|s| &s.title).collect::<Vec<_>>()
    );
    // The catalog row: the pair's own row, minted a placeholder with no length of its own —
    // and no service label (the label is the YouTube half's alone).
    let catalog = lost
        .iter()
        .find(|s| s.suggested_by.is_none())
        .expect("the catalog row carries no service label");
    assert_eq!(catalog.duration, None);
    // The video row beside it: its own upload and length, the videos filter's label.
    let video = lost
        .iter()
        .find(|s| s.suggested_by.as_deref() == Some("YouTube Music (video)"))
        .expect("the video row is labeled");
    assert_eq!(video.duration, Some(619));
    let video_routing = search
        .id_registry
        .lookup(&video.id)
        .expect("registered")
        .snapshot();
    assert_eq!(video_routing.you_tube_id.as_deref(), Some("vBvDiLKTDWA"));
    assert_eq!(video_routing.duration, Some(619));
    // The fallback never ran: its hit is nowhere in the answer.
    assert!(
        songs.iter().all(|s| s.suggested_by.as_deref() != Some("YouTube")),
        "the fallback answered nothing while a source had matched: {:?}",
        songs.iter().map(|s| &s.suggested_by).collect::<Vec<_>>()
    );
}

/// Last.fm answering one track.search row — the catalog pair of the query's own recording —
/// and no top tracks.
async fn last_fm_with_catalog_pair() -> (Arc<LastFmService>, MockServer) {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(move |request: &Request| {
            let body = if request.url.to_string().contains("method=track.search") {
                r#"{"results":{"trackmatches":{"track":[{"name":"Lost in Sound","artist":"K3KHA"}]}}}"#
                    .to_string()
            } else {
                r#"{"toptracks":{"track":[]}}"#.to_string()
            };
            ResponseTemplate::new(200).set_body_string(body)
        })
        .mount(&server)
        .await;
    let settings = Arc::new(SettingsStore::for_tests(AppSettings {
        last_fm: LastFmSettings {
            api_key: "key".into(),
            ..Default::default()
        },
        ..Default::default()
    }));
    (
        Arc::new(LastFmService::with_base_url(
            settings,
            &format!("{}/", server.uri()),
        )),
        server,
    )
}

/// A shim that cannot answer leaves the catalogs' answer standing (the degradation edge): a
/// busy YouTube Music (503 `{"error":"busy"}`) and a failed fallback (502) are no rows from
/// either, never a failure — the answer is the catalog's alone, with no service label on it
/// and no video minted.
#[tokio::test]
async fn a_shim_that_cannot_answer_leaves_the_catalogs_answer_standing() {
    let (last_fm, _server) = one_hit_last_fm().await;
    let shim = MockServer::start().await;
    Mock::given(path("/ytm/search"))
        .respond_with(ResponseTemplate::new(503).set_body_string(r#"{"error":"busy"}"#))
        .mount(&shim)
        .await;
    Mock::given(path("/search/list"))
        .respond_with(ResponseTemplate::new(502).set_body_string(r#"{"error":"search_failed"}"#))
        .mount(&shim)
        .await;
    let search = ExternalSearchService::new(
        one_song_metadata(),
        Some(last_fm),
        waiting(false),
        Arc::new(YouTubeMusicClient::with_shim_url(Some(&shim.uri()))),
        Arc::new(YouTubeResolver::with_base_url(Some(&shim.uri()))),
        Arc::new(ExternalIdRegistry::in_memory()),
    );

    let songs = search.get("song").await;

    // The catalog's row answers; the YouTube half contributed nothing to label.
    assert_eq!(
        songs.len(),
        1,
        "{:?}",
        songs.iter().map(|s| &s.title).collect::<Vec<_>>()
    );
    assert_eq!(songs[0].title, "Song");
    assert_eq!(songs[0].suggested_by, None);
    assert!(
        songs.iter().all(|s| s.suggested_by.is_none()),
        "no row carries a service label: {:?}",
        songs.iter().map(|s| &s.suggested_by).collect::<Vec<_>>()
    );
}

/// The rows a search folds keep what they lack from the row folded in: a songs-catalog
/// duplicate that arrives with the album fills the winner's blank one, and nothing the winner
/// already had is replaced ("wzbogaca, nic nie znika").
#[test]
fn the_dedup_fills_a_blank_album_from_the_row_it_folds() {
    let blank = you_tube_row(
        "K3KHA",
        "Lost in Sound",
        Some(619),
        Some("USK3K2600001"),
        None,
        Some("vidNoAlbum1"),
        RowOrigin::Songs,
    );
    let with_album = YouTubeSong {
        song: Song {
            artist: "K3KHA".into(),
            title: "Lost in Sound".into(),
            album: "Weightless Orbit".into(),
            duration: Some(621),
            ..Default::default()
        },
        you_tube_id: Some("atvRelVid009".into()),
        origin: RowOrigin::Songs,
    };

    let deduped = dedupe_you_tube_songs(vec![blank, with_album]);

    assert_eq!(deduped.len(), 1);
    assert_eq!(deduped[0].song.album, "Weightless Orbit");
    // What the winner already had stays its own.
    assert_eq!(deduped[0].you_tube_id.as_deref(), Some("vidNoAlbum1"));
    assert_eq!(deduped[0].song.duration, Some(619));
}

/// The discovery labels never read as a radio provider's name: the radio paths that read a
/// row's `suggested_by` back (`from_display_name`) must not mistake a YouTube Music song,
/// a YouTube Music video or a plain YouTube hit for the radio's "YouTube Music" source.
#[test]
fn the_discovery_labels_are_not_radio_provider_names() {
    for label in [
        RowOrigin::Songs.label(),
        RowOrigin::Videos.label(),
        RowOrigin::YouTube.label(),
    ] {
        assert!(
            radio_provider::from_display_name(Some(label)).is_none(),
            "'{label}' must not name a radio provider"
        );
    }
    // And the radio's own YouTube Music name is none of the discovery labels, so the two
    // vocabularies stay apart in both directions.
    let radio_names = [
        "Last.fm",
        "YouTube Music",
        "ListenBrainz",
        "Sounds alike",
        "Your library",
    ];
    for label in [
        RowOrigin::Songs.label(),
        RowOrigin::Videos.label(),
        RowOrigin::YouTube.label(),
    ] {
        assert!(
            !radio_names.contains(&label),
            "'{label}' collides with a radio name"
        );
    }
}

/// A query neither the catalogs nor YouTube Music answers with its own row falls back to
/// plain YouTube (D11): the fallback's hit — a set the catalogs do not have — answers with
/// YouTube's label, behind the matched rows (there are none, or it would not run) and ahead
/// of the unrelated ones.
#[tokio::test]
async fn a_query_the_catalogs_dont_have_falls_back_to_you_tube_behind_the_matched_rows() {
    let shim = MockServer::start().await;
    Mock::given(path("/ytm/search"))
        .and(query_param("filter", "songs"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"tracks":[
                {"videoId":"jenks111111","title":"Other Song","artists":["Jenks (UK)"],"durationSeconds":360}
            ]}"#,
        ))
        .mount(&shim)
        .await;
    Mock::given(path("/ytm/search"))
        .and(query_param("filter", "videos"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"tracks":[
                {"videoId":"unrelVid0001","title":"Another Upload","artists":["Someone"],"durationSeconds":120}
            ]}"#,
        ))
        .mount(&shim)
        .await;
    Mock::given(path("/search/list"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"hits":[{"video_id":"setVid12345","title":"Deep Minimal Set","duration":3403,"channel":"K3KHA"}]}"#,
        ))
        .mount(&shim)
        .await;
    let search = ExternalSearchService::new(
        Arc::new(FakeMetadata::default()),
        None,
        waiting(false),
        Arc::new(YouTubeMusicClient::with_shim_url(Some(&shim.uri()))),
        Arc::new(YouTubeResolver::with_base_url(Some(&shim.uri()))),
        Arc::new(ExternalIdRegistry::in_memory()),
    );

    let songs = search.get("K3KHA Deep Minimal Set").await;

    let titles: Vec<&str> = songs.iter().map(|s| s.title.as_str()).collect();
    assert_eq!(
        titles,
        ["Deep Minimal Set", "Other Song", "Another Upload"],
        "{titles:?}"
    );
    assert_eq!(songs[0].suggested_by.as_deref(), Some("YouTube"));
    let routing = search
        .id_registry
        .lookup(&songs[0].id)
        .expect("registered")
        .snapshot();
    assert_eq!(routing.you_tube_id.as_deref(), Some("setVid12345"));
    assert_eq!(routing.duration, Some(3403));
    // The unrelated YouTube Music rows keep their own labels behind the fallback's hit.
    assert_eq!(songs[1].suggested_by.as_deref(), Some("YouTube Music (song)"));
    assert_eq!(songs[2].suggested_by.as_deref(), Some("YouTube Music (video)"));
}
