//! The external search: outside songs and albums for a query.

use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use octo_core::common::dotnet::{self, ordinal_ignore_case_key};
use octo_core::common::{SongIdentity, SupersedableBuildCoordinator, skipped_takes};
use octo_core::last_fm::SimilarTrack;
use octo_core::last_fm::last_fm_search_cleanup::LastFmSearchCleanup;
use octo_core::last_fm::last_fm_search_ranking::LastFmSearchRanking;
use octo_core::models::domain::{Album, Song};
use octo_core::settings::{SettingsStore, SubsonicSettings};
use parking_lot::Mutex;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use crate::services::i_music_metadata_service::IMusicMetadataService;
use crate::services::last_fm::LastFmService;
use crate::services::soulseek::ExternalIdRegistry;
use crate::services::soulseek::soulseek_metadata_service::{parse_query, register_you_tube_row};
use crate::services::you_tube::YouTubeResolver;
use crate::services::you_tube::you_tube_music_client::{YouTubeMusicClient, YtmRow, song_and_routing};

/// Rows built per query, regardless of how many the caller wants.
///
/// It has to be a constant rather than the caller's target, or single-flight is
/// unsound: a client asking for 8 rows could win the race and hand 8 rows to a caller
/// that asked for 150. 60 is the number because that is where enrichment stops
/// (BackgroundEnrichLimit), so rows past it would ship as bare placeholders carrying a
/// fallback duration, and because the Navidrome-native search path already caps here.
pub const BUILD_SIZE: i32 = 60;

// track.search returns at most 50 rows, so padding whenever the list was short of
// BuildSize made the second, sequential Last.fm call on every search.
const THIN_SEARCH_THRESHOLD: usize = 20;

/// Deadline for one build. Last.fm has no configured HTTP timeout of its own, so
/// without this a single hung call would pin the query for every joined caller.
const BUILD_TIMEOUT: Duration = Duration::from_secs(10);

/// The longest album ownership waits for a song build's row lookups before reading its own.
const ROWS_WAIT: Duration = Duration::from_secs(10);

/// Deadline for one album build. Deezer's own client timeout already bounds
/// the call inside it; this is the ceiling on the whole build.
const ALBUM_BUILD_TIMEOUT: Duration = Duration::from_secs(10);

/// Builds the external (discovery) half of a search, once per query.
///
/// Every caller for the same query joins one execution and receives the same list. That
/// is not an optimization, it is what keeps the search3 fix from re-creating issue #8.
/// Clients routinely fire several search calls for one typed query, and registry ids are
/// deterministic, so those calls resolve to the *same* SoulseekRouting objects and would
/// each run the enrichment pipeline over them concurrently: three writers to a shared
/// int? duration, and three times the Deezer fan-out against a budget that is already the
/// tightest thing in the system.
///
/// The returned list is FROZEN (an `Arc` nobody can write through). Callers slice it and
/// serialize it, concurrently, without copying. Both Subsonic serializers and the native one
/// only read, and the star/download paths rebuild a Song from the registry rather than from a
/// search result. Any future "top up the enrichment because this caller wanted more rows"
/// belongs inside the build, not after it.
pub struct ExternalSearchService {
    // Amperfy (and most Subsonic clients' type-ahead) fires one search3 call per
    // keystroke, uncancelled. Plain SingleFlight only collapses two callers asking for
    // the SAME query; it does nothing for the sequence "cage", "cage t", "cage the",
    // where each keystroke is a distinct key racing the same rate-limited lane as the
    // query the user actually meant. SupersedableBuildCoordinator adds prefix-based
    // cancellation on top of that collapsing.
    song_builds: SupersedableBuildCoordinator<(Arc<Vec<Song>>, SongSource)>,
    album_builds: SupersedableBuildCoordinator<Arc<Vec<Album>>>,
    metadata: Arc<dyn IMusicMetadataService>,
    last_fm: Option<Arc<LastFmService>>,
    /// Optional: how long a build waits for lengths, read at every build.
    settings: Option<Arc<SettingsStore>>,
    /// YouTube Music's search, through the shim's `/ytm` endpoints: the one client the radio
    /// shares (D14), so discovery and radio spend one shim.
    you_tube_music: Arc<YouTubeMusicClient>,
    /// The shim's yt-dlp client, for the multi-hit fallback (`/search/list`, D11).
    you_tube_resolver: Arc<YouTubeResolver>,
    /// The registry YouTube rows are minted into: their ids are `make_short_id` of the routing
    /// they arrive with, so the row plays, opens and stars like any outside song (D6/D13).
    id_registry: Arc<ExternalIdRegistry>,
    /// The last finished builds, by query.
    recent_songs: Arc<RecentResults<Song>>,
    recent_albums: RecentResults<Album>,
    /// Last.fm answers that came too late for their search, for the next one.
    last_fm_answers: Arc<RecentResults<SimilarTrack>>,
    row_signals: RowSignals,
}

impl ExternalSearchService {
    pub const BUILD_SIZE: i32 = BUILD_SIZE;

    pub fn new(
        metadata: Arc<dyn IMusicMetadataService>,
        last_fm: Option<Arc<LastFmService>>,
        settings: Option<Arc<SettingsStore>>,
        you_tube_music: Arc<YouTubeMusicClient>,
        you_tube_resolver: Arc<YouTubeResolver>,
        id_registry: Arc<ExternalIdRegistry>,
    ) -> Self {
        ExternalSearchService {
            song_builds: SupersedableBuildCoordinator::new(),
            album_builds: SupersedableBuildCoordinator::new(),
            metadata,
            last_fm,
            settings,
            you_tube_music,
            you_tube_resolver,
            id_registry,
            recent_songs: Arc::new(RecentResults::new(RECENT_RESULTS_TTL, RECENT_RESULTS_MAX)),
            recent_albums: RecentResults::new(RECENT_RESULTS_TTL, RECENT_RESULTS_MAX),
            last_fm_answers: Arc::new(RecentResults::new(RECENT_RESULTS_TTL, RECENT_RESULTS_MAX)),
            row_signals: RowSignals::default(),
        }
    }

    /// Up to [`BUILD_SIZE`] enriched external songs for this query. Callers take
    /// the prefix they need; the list is shared and must not be mutated. While Skip covers,
    /// karaoke and instrumentals is on, cover, karaoke and instrumental takes are left out; the build itself
    /// keeps them, so switching the setting changes the next answer at once.
    pub async fn get(&self, query: &str) -> Arc<Vec<Song>> {
        let key = recent_key(query);
        let songs = self.songs(query, &key).await;
        // However the songs were had, nothing more of this query's rows will be looked up.
        self.row_signals.done(&key);
        if self.skips_covers_and_instrumentals()
            && songs.iter().any(|song| skipped_takes::hides_song(song, true))
        {
            return Arc::new(skipped_takes::listed_songs(songs.as_ref().clone(), true));
        }
        songs
    }

    /// Skip covers and instrumentals, as it is set now.
    fn skips_covers_and_instrumentals(&self) -> bool {
        self.settings
            .as_ref()
            .is_some_and(|settings| settings.current().soulseek.skip_covers_and_instrumentals)
    }

    /// Resolves once the song build for `query` has looked its rows up on Deezer (at once
    /// when there is nothing to wait for, and after [`ROWS_WAIT`] at the latest). Taken before
    /// [`Self::get`] is called for the query, so the build cannot finish unseen.
    pub fn rows_looked_up(&self, query: &str) -> impl Future<Output = ()> + Send + 'static {
        let mut done = self.row_signals.subscribe(&recent_key(query));
        async move {
            let _ = tokio::time::timeout(ROWS_WAIT, done.wait_for(|done| *done)).await;
        }
    }

    async fn songs(&self, query: &str, key: &str) -> Arc<Vec<Song>> {
        if dotnet::is_blank(query) {
            return Arc::new(Vec::new());
        }
        // Only the key, not the radio switch. Discovery in the search bar is a different
        // feature from radio, and gating it on EnableRadio made turning radio off empty
        // the search results too.
        //
        // The YouTube half needs no key (D4): without one the catalogs are simply absent and
        // the build answers from YouTube Music and the yt-dlp fallback alone; with one, the
        // catalogs keep their priority and YouTube supplements them.
        let last_fm = self.last_fm.clone().filter(|l| l.has_api_key());

        let key = key.to_string();
        if let Some(cached) = self.recent_songs.get(&key, Instant::now()) {
            // A copy: the cached list is shared and frozen like any build's. Rows whose video
            // was found after the build answered carry its length now.
            let mut songs = cached.as_ref().clone();
            self.metadata.refresh_outside_songs(&mut songs);
            debug!(
                "external search '{query}' answered from the last {} rows",
                songs.len()
            );
            return Arc::new(songs);
        }

        let metadata = self.metadata.clone();
        let subsonic = self
            .settings
            .as_ref()
            .map_or_else(SubsonicSettings::default, |s| s.current().subsonic.clone());
        let wait = subsonic.effective_search_duration_wait();
        let last_fm_wait = subsonic.effective_search_last_fm_wait();
        let query_text = query.to_string();
        let you_tube = YouTubeSources {
            music: Arc::clone(&self.you_tube_music),
            resolver: Arc::clone(&self.you_tube_resolver),
            registry: Arc::clone(&self.id_registry),
        };
        let shared = BuildShared {
            key: key.clone(),
            last_fm_answers: Arc::clone(&self.last_fm_answers),
            recent_songs: Arc::clone(&self.recent_songs),
        };
        let rows_done = RowsDone {
            signals: self.row_signals.clone(),
            key: key.clone(),
        };
        let (songs, source) = self
            .song_builds
            .run(
                query.trim(),
                move |token: CancellationToken| async move {
                    tokio::select! {
                        (built, source) = build(query_text, last_fm, metadata, wait, last_fm_wait, you_tube, shared, rows_done) => Ok((Arc::new(built), source)),
                        () = token.cancelled() => Err(anyhow::anyhow!("The operation was canceled.")),
                    }
                },
                // The waits for Last.fm and for lengths are on top of the deadline, so long
                // ones never time the whole build out.
                BUILD_TIMEOUT + wait + last_fm_wait,
                (Arc::new(Vec::new()), SongSource::Nothing),
                // Discovery is an addition to search, never a precondition for it. The
                // steps inside the build are individually best-effort, but the deadline is
                // not, and a timeout must not take local results down with it.
                |q: &str, e: &anyhow::Error| debug!("external search '{q}' failed: {e}"),
            )
            .await;
        // A failed, timed-out or superseded build answers an empty list, which is not kept. A
        // build from Deezer's rows gives way to Last.fm's answer once that is in.
        if !songs.is_empty() {
            self.recent_songs.put(key.clone(), songs.clone(), Instant::now());
            if source == SongSource::Deezer && self.last_fm_answers.get(&key, Instant::now()).is_some() {
                self.recent_songs.remove(&key);
            }
        }
        songs
    }

    /// Up to `limit` external albums for this query, via the same prefix-supersession as
    /// [`ExternalSearchService::get`]. Needs no Last.fm key: Deezer's album catalog is keyless.
    /// While Skip covers and instrumentals is on, an album named as covers, karaoke or instrumentals is left out.
    pub async fn get_albums(&self, query: &str, limit: i32) -> Arc<Vec<Album>> {
        let albums = self.albums(query, limit).await;
        if self.skips_covers_and_instrumentals()
            && albums.iter().any(|album| skipped_takes::hides_album(album, true))
        {
            return Arc::new(skipped_takes::listed_albums(albums.as_ref().clone(), true));
        }
        albums
    }

    async fn albums(&self, query: &str, limit: i32) -> Arc<Vec<Album>> {
        if dotnet::is_blank(query) || limit <= 0 {
            return Arc::new(Vec::new());
        }
        let key = format!("{}|{limit}", recent_key(query));
        if let Some(cached) = self.recent_albums.get(&key, Instant::now()) {
            return cached;
        }
        let metadata = self.metadata.clone();
        let query_text = query.to_string();
        let albums = self
            .album_builds
            .run(
                query,
                move |token: CancellationToken| async move {
                    tokio::select! {
                        albums = metadata.search_album_rows(&query_text, limit) => Ok(Arc::new(albums)),
                        () = token.cancelled() => Err(anyhow::anyhow!("The operation was canceled.")),
                    }
                },
                ALBUM_BUILD_TIMEOUT,
                Arc::new(Vec::new()),
                |q: &str, e: &anyhow::Error| debug!("external album search '{q}' failed: {e}"),
            )
            .await;
        if !albums.is_empty() {
            self.recent_albums.put(key, albums.clone(), Instant::now());
        }
        albums
    }
}

/// How long a finished build answers the same query again.
pub const RECENT_RESULTS_TTL: Duration = Duration::from_secs(10 * 60);

/// How many queries' builds are kept; past it the oldest go first.
pub const RECENT_RESULTS_MAX: usize = 256;

/// The key a query's build is kept under: trimmed and lowercase, so "Adele " and "adele" share
/// one.
pub fn recent_key(query: &str) -> String {
    query.trim().to_lowercase()
}

/// The builds of the last queries, each kept for [`RECENT_RESULTS_TTL`], at most
/// [`RECENT_RESULTS_MAX`] of them. Typing the same words again, paging a result, or a client
/// asking twice then answers at once instead of asking Last.fm, Deezer and YouTube again.
pub struct RecentResults<T> {
    entries: Mutex<RecentEntries<T>>,
    ttl: Duration,
    max: usize,
}

struct RecentEntries<T> {
    by_key: HashMap<String, (Instant, Arc<Vec<T>>)>,
    /// Keys oldest first, each once.
    order: VecDeque<String>,
}

impl<T> RecentResults<T> {
    pub fn new(ttl: Duration, max: usize) -> Self {
        RecentResults {
            entries: Mutex::new(RecentEntries {
                by_key: HashMap::new(),
                order: VecDeque::new(),
            }),
            ttl,
            max,
        }
    }

    /// The build kept for `key`, unless it is older than the time to live at `now`.
    pub fn get(&self, key: &str, now: Instant) -> Option<Arc<Vec<T>>> {
        let mut entries = self.entries.lock();
        let (kept_at, value) = entries.by_key.get(key)?;
        if now.saturating_duration_since(*kept_at) < self.ttl {
            return Some(value.clone());
        }
        entries.by_key.remove(key);
        entries.order.retain(|k| k != key);
        None
    }

    /// Forgets what is kept for `key`.
    pub fn remove(&self, key: &str) {
        let mut entries = self.entries.lock();
        if entries.by_key.remove(key).is_some() {
            entries.order.retain(|k| k != key);
        }
    }

    /// Keeps `value` for `key` from `now`, replacing an older build of it; past the limit the
    /// oldest query goes.
    pub fn put(&self, key: String, value: Arc<Vec<T>>, now: Instant) {
        let mut entries = self.entries.lock();
        if entries.by_key.insert(key.clone(), (now, value)).is_some() {
            entries.order.retain(|k| *k != key);
        }
        entries.order.push_back(key);
        while entries.order.len() > self.max {
            if let Some(oldest) = entries.order.pop_front() {
                entries.by_key.remove(&oldest);
            }
        }
    }
}

/// Where a build's song rows came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SongSource {
    /// Nothing was built (a failed or superseded build).
    Nothing,
    /// Last.fm's track.search, answered in time.
    LastFm,
    /// Last.fm's answer to an earlier search of the same words, which came too late for it.
    LastFmKept,
    /// Deezer's own track search: Last.fm did not answer in time.
    Deezer,
    /// Last.fm, waited for after all: it did not answer in time and Deezer had nothing.
    LastFmLate,
}

impl SongSource {
    fn describe(self) -> &'static str {
        match self {
            SongSource::Nothing => "nothing",
            SongSource::LastFm => "Last.fm",
            SongSource::LastFmKept => "Last.fm (kept from an earlier search)",
            SongSource::Deezer => "Deezer's track search (Last.fm did not answer in time)",
            SongSource::LastFmLate => "Last.fm (late; Deezer's track search had nothing)",
        }
    }
}

/// What a build shares with the service: Last.fm answers kept for the next search, the
/// finished builds, and who waits for its row lookups.
struct BuildShared {
    key: String,
    last_fm_answers: Arc<RecentResults<SimilarTrack>>,
    recent_songs: Arc<RecentResults<Song>>,
}

/// Fans out to Last.fm, then fills in the metadata a client needs to render and play
/// the rows. Order:
///   1. track.search hits for the query as typed, most listened first
///   2. canonical artist's top tracks (in case (1) was thin: common for
///      single-word artist queries)
///
/// When Last.fm has not answered within `last_fm_wait`, the rows are Deezer's own track search
/// for the query instead, in Deezer's order, and Last.fm's answer is kept for the next search
/// of the same words.
///
/// Deduped by `SongIdentity::match_key`, so the same track cannot appear twice however its
/// artist and title are written.
#[allow(clippy::too_many_arguments)]
async fn build(
    query: String,
    last_fm: Option<Arc<LastFmService>>,
    metadata: Arc<dyn IMusicMetadataService>,
    duration_wait: Duration,
    last_fm_wait: Duration,
    you_tube: YouTubeSources,
    shared: BuildShared,
    rows_done: RowsDone,
) -> (Vec<Song>, SongSource) {
    let started = Instant::now();
    let (mut collected, source) = song_rows(&query, &last_fm, &metadata, last_fm_wait, &shared).await;
    let searched = started.elapsed();

    // The YouTube half (D2/D11/D3): rows registered with their video and its length at once,
    // deduped version-aware. A catalog row the SONGS catalog's row covers is left out rather
    // than minted: that row is the richer duplicate of the same recording (the video is pinned
    // and the length is the video's own) and the songs catalog folds its own duplicates
    // (Correction B). A video row or a YouTube hit covers nothing: a video and a song are two
    // different rows that never merge, and so are YouTube Music and YouTube — the catalog row
    // keeps its own place beside them.
    let you_tube_songs = you_tube_songs(&query, &collected, &you_tube).await;
    if !you_tube_songs.is_empty() {
        collected.retain(|(artist, title)| you_tube_songs.iter().all(|row| !row.covers(artist, title)));
    }

    let mut songs = Vec::with_capacity(collected.len() + you_tube_songs.len());
    for (artist, title) in &collected {
        if let Some(hit) = metadata
            .search_songs_by_artist_title(artist, title, 1, None)
            .await
            .into_iter()
            .next()
        {
            songs.push(hit);
        }
    }
    songs.extend(you_tube_songs.into_iter().map(|row| row.song));
    songs.truncate(BUILD_SIZE as usize);
    let placed = started.elapsed();
    info!("External search '{query}' -> {} placeholder songs", songs.len());

    // Album/art/year from Deezer (fast), then the ACCURATE duration for the top of the
    // list from the real YouTube video (so the scrub bar matches the audio and the
    // client advances correctly), waited for at most `duration_wait`. Rows not resolved by
    // then go out with Deezer's length and are resolved in the background. Bounded + cached.
    // The video lookups start now, so they run while Deezer answers; the pick still waits
    // for Deezer's length.
    metadata.prefetch_top_videos(&songs);
    metadata.enrich_search_rows(&mut songs).await;
    let enriched = started.elapsed();
    // Album ownership reads tracklists from the same Deezer budget; they start now.
    rows_done.mark();
    let before: Vec<Option<i32>> = songs.iter().map(|song| song.duration).collect();
    let durations = if duration_wait.is_zero() {
        None
    } else {
        Some(
            metadata
                .resolve_top_durations_within(&mut songs, duration_wait)
                .await,
        )
    };
    let lengthened = started.elapsed();
    info!(
        "External search '{query}' built in {} ms from {}: songs {} ms, rows {} ms, Deezer {} ms, YouTube lengths {} ms ({} rows changed{})",
        lengthened.as_millis(),
        source.describe(),
        searched.as_millis(),
        (placed - searched).as_millis(),
        (enriched - placed).as_millis(),
        (lengthened - enriched).as_millis(),
        songs
            .iter()
            .zip(&before)
            .filter(|(song, before)| song.duration != **before)
            .count(),
        match &durations {
            Some(Some(_)) => ", the rest in the background",
            Some(None) => "",
            None => ", all in the background",
        }
    );

    // Fire-and-forget: pre-resolve YouTube videoIds for the top hits so the first
    // /rest/stream click doesn't pay the cold yt-dlp double-call cost (ytsearch1: + -g,
    // 6-16s combined). Arpeggi cancels at ~10s and falls back to a local song; without
    // this, external playback is unreachable from that client. 12 is about what fits on
    // the first page of search results.
    //
    // It runs LAST on purpose. It used to run before enrichment, where it wrote a videoId chosen
    // with no duration hint while the top durations pass was choosing a different one using the
    // Deezer duration: so for the top rows the two raced and the loser could leave a song
    // advertising the length of a video that would not be the one played.
    //
    // The background work gets its own copy of the list: the songs this build returns stay
    // frozen, and in background mode the durations pass writes only the routing. The prewarm
    // waits for the rows the durations pass is still resolving.
    let background = songs.clone();
    let prewarm = metadata.clone();
    match durations {
        None => {
            tokio::spawn(resolve_durations_then_prewarm(prewarm, background));
        }
        Some(still_running) => {
            tokio::spawn(async move {
                if let Some(pass) = still_running {
                    let _ = pass.await;
                }
                prewarm.prewarm_you_tube_ids(&background, 12).await;
            });
        }
    }

    // Same reasoning, for cover art: a client renders the first screen of results a
    // moment after this returns, and without a prewarm each row's getCoverArt call
    // pays for the Deezer/iTunes/Last.fm chain itself, one row at a time. 24 covers
    // more than a screenful so scrolling a little still finds a warm cache.
    let covers = songs.clone();
    tokio::spawn(async move { metadata.prewarm_cover_art(&covers, 24).await });

    (songs, source)
}

/// The artist and title of each row, deduped, at most [`BUILD_SIZE`]: Last.fm's, or Deezer's
/// when Last.fm is late (see [`build`]). With no Last.fm key there are no catalog rows at all
/// (D4): the build's songs are the YouTube half's alone.
async fn song_rows(
    query: &str,
    last_fm: &Option<Arc<LastFmService>>,
    metadata: &Arc<dyn IMusicMetadataService>,
    last_fm_wait: Duration,
    shared: &BuildShared,
) -> (Vec<(String, String)>, SongSource) {
    let Some(last_fm) = last_fm.as_ref().filter(|l| l.has_api_key()) else {
        return (Vec::new(), SongSource::Nothing);
    };
    if let Some(kept) = shared.last_fm_answers.get(&shared.key, Instant::now()) {
        return (last_fm_rows(query, &kept, last_fm).await, SongSource::LastFmKept);
    }
    let asking = Arc::clone(last_fm);
    let asked = query.to_string();
    let mut answer = tokio::spawn(async move { asking.search_tracks(&asked, 50.min(BUILD_SIZE * 2)).await });
    let in_time = if last_fm_wait.is_zero() {
        Some((&mut answer).await)
    } else {
        tokio::time::timeout(last_fm_wait, &mut answer).await.ok()
    };
    if let Some(tracks) = in_time {
        return (
            last_fm_rows(query, &tracks.unwrap_or_default(), last_fm).await,
            SongSource::LastFm,
        );
    }

    match metadata.search_catalog_tracks(query, BUILD_SIZE).await {
        Some(rows) if !rows.is_empty() => {
            // Last.fm's answer, when it comes, is the next search's, and the build kept from
            // this one gives way to it.
            let key = shared.key.clone();
            let answers = Arc::clone(&shared.last_fm_answers);
            let recent = Arc::clone(&shared.recent_songs);
            tokio::spawn(async move {
                if let Ok(tracks) = answer.await
                    && !tracks.is_empty()
                {
                    answers.put(key.clone(), Arc::new(tracks), Instant::now());
                    recent.remove(&key);
                }
            });
            (deduped(rows.into_iter()), SongSource::Deezer)
        }
        _ => (
            last_fm_rows(query, &answer.await.unwrap_or_default(), last_fm).await,
            SongSource::LastFmLate,
        ),
    }
}

/// Rows from Last.fm's track.search answer: names put right, most listened first, padded with
/// the artist's top tracks when thin.
async fn last_fm_rows(
    query: &str,
    answer: &[SimilarTrack],
    last_fm: &LastFmService,
) -> Vec<(String, String)> {
    // Last.fm's search carries rows from mislabelled scrobbles; put their names right first,
    // or they reach the results, the player and each listener's Last.fm as they are. Then
    // most listened first: track.search orders by fuzzy relevance, which puts a side project's
    // song ahead of the hit everybody means. Repeated versions of a song are one row, and rows
    // in another script than the query go last (see LastFmSearchRanking).
    let tracks = LastFmSearchRanking::rank(query, LastFmSearchCleanup::clean(answer));
    let mut collected = deduped(tracks.iter().map(|t| (t.artist.clone(), t.title.clone())));

    if collected.len() < THIN_SEARCH_THRESHOLD {
        // Use the first track-search hit's artist as the canonical anchor
        // for top-tracks padding. Falls back to the raw query string when
        // track.search came back empty.
        let anchor = tracks.first().map_or(query, |t| t.artist.as_str());
        let top_tracks = last_fm.search_artist_top_tracks(anchor, BUILD_SIZE * 2).await;
        collected = deduped(
            collected
                .into_iter()
                .chain(top_tracks.iter().map(|t| (t.artist.clone(), t.title.clone()))),
        );
    }
    collected
}

/// Artist and title pairs, each song once by `SongIdentity::match_key`, at most [`BUILD_SIZE`].
fn deduped(rows: impl Iterator<Item = (String, String)>) -> Vec<(String, String)> {
    // HashSet<string>(OrdinalIgnoreCase) over the match keys.
    let mut seen = HashSet::new();
    let mut collected = Vec::new();
    for (artist, title) in rows {
        if collected.len() >= BUILD_SIZE as usize {
            break;
        }
        let key = SongIdentity::match_key(&artist, &title);
        if seen.insert(ordinal_ignore_case_key(&key)) {
            collected.push((artist, title));
        }
    }
    collected
}

// ---- The YouTube half (D2/D3/D11) ----------------------------------------------------------

/// The YouTube half's dependencies, into the build.
#[derive(Clone)]
struct YouTubeSources {
    music: Arc<YouTubeMusicClient>,
    resolver: Arc<YouTubeResolver>,
    registry: Arc<ExternalIdRegistry>,
}

/// Which of YouTube's services a row came from. A video and a song are two different things
/// that never merge into one row, and YouTube Music and plain YouTube are two different
/// services: the same song sounds different in each, and the choice belongs to the user, not
/// to the algorithm. So the merge rules and the answer's order both read this: only the very
/// same upload (the same `videoId`) seen from two services is one row, and only the songs
/// catalog folds its own duplicates; YouTube Music answers before plain YouTube, which is the
/// fallback for what the catalogs do not have (mixes, sets).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowOrigin {
    /// YouTube Music's `songs` catalog: a release.
    Songs,
    /// YouTube Music's `videos` catalog: an upload.
    Videos,
    /// Plain YouTube's own search, through the shim's yt-dlp fallback (D11).
    YouTube,
}

impl RowOrigin {
    /// What the clients show on the row (`octoSuggestedBy`, the badge the radio answers made
    /// a convention): the user sees what a row is — a YouTube Music song, a YouTube Music
    /// video, or a YouTube hit — and chooses between the services.
    fn label(self) -> &'static str {
        match self {
            RowOrigin::Songs => "YouTube Music (song)",
            RowOrigin::Videos => "YouTube Music (video)",
            RowOrigin::YouTube => "YouTube",
        }
    }
}

/// A YouTube-origin row: the registered song, the video it was minted with, and the service
/// it came from. The dedup's stages read the video and the origin; the song itself has no
/// field for either (the source label rides `suggested_by`, which nothing stores).
struct YouTubeSong {
    song: Song,
    you_tube_id: Option<String>,
    origin: RowOrigin,
}

/// The YouTube half of a search: YouTube Music's `songs` and `videos` filters asked at once,
/// the rows that match the query first with the videos filter's rows ahead of the songs
/// catalog's (the upload the query means is the answer, ahead of the release of the same
/// recording — W3), and, when neither the catalogs nor YouTube Music answered with a row that
/// matches the query, the shim's multi-hit yt-dlp search as the fallback (D11) behind the
/// matched rows and ahead of the unrelated ones. Every row is registered with its video and
/// its length at once (seed-stable, D6/D13), carries its service's label, and the
/// version-aware dedup folds only what it may: the same upload, and the songs catalog's own
/// duplicates. A shim that cannot answer — 502 `ytm_failed`, 503 busy, unreachable — leaves no
/// rows: the search still answers from its other sources (degradation, not panic).
async fn you_tube_songs(
    query: &str,
    catalog: &[(String, String)],
    sources: &YouTubeSources,
) -> Vec<YouTubeSong> {
    // Both filters at once (D2): the songs catalog answers releases, the videos one uploads,
    // and the song asked for can sit in either (measured: K3KHA - Lost in Sound is only in
    // videos). A filter the shim could not answer is no rows from it, never a failure.
    let (songs, videos) = tokio::join!(
        sources.music.search(query, "songs"),
        sources.music.search(query, "videos"),
    );
    let (exact, rest) = split_ytm_rows(
        query,
        songs.unwrap_or_default().unwrap_or_default(),
        videos.unwrap_or_default().unwrap_or_default(),
    );
    // The fallback only runs when nothing matched: it is the last source, and asking it while
    // a source already answered with the query's own song only adds duplicates to fold.
    let matched = !exact.is_empty()
        || catalog
            .iter()
            .any(|(artist, title)| pair_matches_query(query, artist, title));
    let fallback = fallback_rows(query, matched, sources).await;
    let rows = exact.into_iter().chain(fallback).chain(rest);
    let registered = rows
        .map(|(row, origin)| {
            let (song, routing) = song_and_routing(&row);
            let you_tube_id = routing.you_tube_id.clone();
            let mut song = register_you_tube_row(&sources.registry, (song, routing));
            song.suggested_by = Some(origin.label().to_string());
            YouTubeSong {
                song,
                you_tube_id,
                origin,
            }
        })
        .collect();
    dedupe_you_tube_songs(registered)
}

/// The fallback's rows (D11): plain YouTube's own hits for a query neither the catalogs nor
/// YouTube Music answered with its own song — mixes and sets the catalogs do not have. Empty
/// when `matched`, so a source that already answered is never asked to duplicate itself. The
/// hits sit behind the matched rows (there are none when this runs) and ahead of the
/// unrelated ones: they are the query's own content, on plain YouTube, behind YouTube Music.
async fn fallback_rows(query: &str, matched: bool, sources: &YouTubeSources) -> Vec<(YtmRow, RowOrigin)> {
    if matched {
        return Vec::new();
    }
    sources
        .resolver
        .search_many(query, YouTubeResolver::SEARCH_MANY_LIMIT)
        .await
        .into_iter()
        .filter_map(|hit| fallback_row(query, &hit))
        .map(|row| (row, RowOrigin::YouTube))
        .collect()
}

/// Splits the two catalogs' answers into the rows that are the query's own song and the rest
/// (D2, PoC-1's `interleave_ytm_rows`). Among the query's own rows the videos filter's come
/// first, the songs catalog's after them: a video and a song of the same recording are two
/// different rows, and the upload the query means is the answer ahead of the release of it
/// (W3: `vBvDiLKTDWA` first). The rest keep the index-by-index interleave: songs[i] before
/// videos[i], row by row.
fn split_ytm_rows(
    query: &str,
    songs: Vec<YtmRow>,
    videos: Vec<YtmRow>,
) -> (Vec<(YtmRow, RowOrigin)>, Vec<(YtmRow, RowOrigin)>) {
    let mut exact_videos = Vec::new();
    let mut exact_songs = Vec::new();
    let mut rest = Vec::new();
    for i in 0..songs.len().max(videos.len()) {
        for (rows, origin) in [(&songs, RowOrigin::Songs), (&videos, RowOrigin::Videos)] {
            let Some(row) = rows.get(i) else {
                continue;
            };
            if !row_matches_query(query, row) {
                rest.push((row.clone(), origin));
            } else if origin == RowOrigin::Videos {
                exact_videos.push((row.clone(), origin));
            } else {
                exact_songs.push((row.clone(), origin));
            }
        }
    }
    (exact_videos.into_iter().chain(exact_songs).collect(), rest)
}

/// Whether a row is the query's own song: the query's key names one of the row's artists and
/// the row's title (`matches_query` from PoC-1).
fn row_matches_query(query: &str, row: &YtmRow) -> bool {
    let query_key = SongIdentity::key(query);
    let artist_hit = row.artists.iter().any(|artist| {
        let key = SongIdentity::key(artist);
        !key.is_empty() && query_key.contains(&key)
    });
    artist_hit && query_key.contains(&SongIdentity::key(&row.title))
}

/// Whether a catalog row is the query's own song, on the same rule as [`row_matches_query`].
fn pair_matches_query(query: &str, artist: &str, title: &str) -> bool {
    let query_key = SongIdentity::key(query);
    let artist_key = SongIdentity::key(artist);
    !artist_key.is_empty() && query_key.contains(&artist_key) && query_key.contains(&SongIdentity::key(title))
}

/// A fallback hit as the same row shape YouTube Music's rows have, so registration, the dedup
/// and the query match are one code path for both sources. The artist is the query's leading
/// words (the split `search_songs` makes of a free-text query); a hit without a title names
/// nothing to show and is skipped, as a YouTube Music row without one is.
fn fallback_row(query: &str, hit: &crate::services::you_tube::YouTubeHit) -> Option<YtmRow> {
    let title = hit.title.as_deref()?.trim().to_string();
    if title.is_empty() {
        return None;
    }
    let (artist, _) = parse_query(query);
    Some(YtmRow {
        video_id: hit.video_id.clone(),
        title,
        artists: vec![artist],
        album: None,
        duration_seconds: hit.duration,
        video_type: None,
    })
}

/// How far apart two rows' lengths may be and still be one recording in the songs catalog's
/// own dedup.
const SAME_RECORDING_WITHIN_SECONDS: i32 = 8;

impl YouTubeSong {
    /// Whether this row is the songs catalog's own row of the catalog row `artist - title`:
    /// a song row covers its catalog twin (the richer duplicate of the same recording — the
    /// video is pinned and the length is the video's own), while a video row or a plain
    /// YouTube hit covers nothing: a video and a song never merge, and neither do YouTube
    /// Music and YouTube, so the catalog row keeps its own place beside them. Versions keep
    /// their own rows (`match_key` is versioned).
    fn covers(&self, artist: &str, title: &str) -> bool {
        self.origin == RowOrigin::Songs
            && SongIdentity::match_key(&self.song.artist, &self.song.title)
                .eq_ignore_ascii_case(&SongIdentity::match_key(artist, title))
    }
}

/// Whether two YouTube rows are one recording: the very same upload (the same `videoId`),
/// whatever service each was seen from; or — inside the songs catalog alone — a shared ISRC,
/// the same MusicBrainz recording, or the same versioned `match_key` with lengths within
/// [`SAME_RECORDING_WITHIN_SECONDS`] when both are known (Correction B). Everything else
/// stays apart: a YouTube Music video and a YouTube Music song are two different rows, and so
/// are YouTube Music and plain YouTube, whatever the same recording's length in each says.
/// Versions stay apart in every catalog: an Extended Mix, a Live take or a radio edit is its
/// own row.
fn same_recording(kept: &YouTubeSong, item: &YouTubeSong) -> bool {
    // The same upload seen from two services (the videos filter and yt-dlp's fallback, say)
    // is one row: literally the same video.
    if let (Some(a), Some(b)) = (kept.you_tube_id.as_deref(), item.you_tube_id.as_deref())
        && !a.is_empty()
        && a == b
    {
        return true;
    }
    // Only the songs catalog folds its own duplicates; every other pair is two rows.
    if kept.origin != RowOrigin::Songs || item.origin != RowOrigin::Songs {
        return false;
    }
    // A shared ISRC, or the same MusicBrainz recording.
    if SongIdentity::shares_isrc(kept.song.isrc.as_deref(), item.song.isrc.as_deref()) {
        return true;
    }
    if let (Some(a), Some(b)) = (
        kept.song.music_brainz_recording_id.as_deref(),
        item.song.music_brainz_recording_id.as_deref(),
    ) && !a.is_empty()
        && a.eq_ignore_ascii_case(b)
    {
        return true;
    }
    // The same versioned key, with lengths close enough when both are known.
    let kept_key = SongIdentity::match_key(&kept.song.artist, &kept.song.title);
    let item_key = SongIdentity::match_key(&item.song.artist, &item.song.title);
    if !kept_key.eq_ignore_ascii_case(&item_key) {
        return false;
    }
    match (kept.song.duration, item.song.duration) {
        (Some(a), Some(b)) => (a - b).abs() <= SAME_RECORDING_WITHIN_SECONDS,
        _ => true,
    }
}

/// Folds the duplicate rows (`dedupe_multi_source_songs` from PoC-1): the first of a set of
/// duplicates wins, keeps its video, its length and its service's label (a video a row was
/// minted with is never replaced, D13), and takes what it lacks — the ISRC, the MusicBrainz
/// recording, a blank album — from the rows folded into it.
fn dedupe_you_tube_songs(rows: Vec<YouTubeSong>) -> Vec<YouTubeSong> {
    let mut kept: Vec<YouTubeSong> = Vec::new();
    for row in rows {
        match kept.iter_mut().find(|existing| same_recording(existing, &row)) {
            Some(existing) => {
                if existing.you_tube_id.is_none() {
                    existing.you_tube_id = row.you_tube_id;
                }
                if existing.song.duration.is_none() {
                    existing.song.duration = row.song.duration;
                }
                if existing.song.album.is_empty() {
                    existing.song.album = row.song.album.clone();
                }
                if existing.song.isrc.is_none() {
                    existing.song.isrc = row.song.isrc.clone();
                }
                if existing.song.music_brainz_recording_id.is_none() {
                    existing.song.music_brainz_recording_id = row.song.music_brainz_recording_id.clone();
                }
            }
            None => kept.push(row),
        }
    }
    kept
}

/// Who waits for each query's row lookups: album ownership reads its tracklists from the same
/// Deezer budget, so it starts once the song build's rows are looked up.
#[derive(Clone, Default)]
pub struct RowSignals(Arc<Mutex<HashMap<String, watch::Sender<bool>>>>);

impl RowSignals {
    fn subscribe(&self, key: &str) -> watch::Receiver<bool> {
        self.0
            .lock()
            .entry(key.to_string())
            .or_insert_with(|| watch::channel(false).0)
            .subscribe()
    }

    fn done(&self, key: &str) {
        if let Some(sender) = self.0.lock().remove(key) {
            sender.send_replace(true);
        }
    }
}

/// Says a build's rows are looked up, when told or, at the latest, when the build ends.
struct RowsDone {
    signals: RowSignals,
    key: String,
}

impl RowsDone {
    fn mark(&self) {
        self.signals.done(&self.key);
    }
}

impl Drop for RowsDone {
    fn drop(&mut self) {
        self.mark();
    }
}

/// The durations pass search did not wait for, then the videoId prewarm, in that order
/// for the reason given in `build`. It runs in background mode, so the songs this
/// build returned stay frozen.
async fn resolve_durations_then_prewarm(metadata: Arc<dyn IMusicMetadataService>, mut songs: Vec<Song>) {
    metadata.resolve_top_durations(&mut songs, true).await;
    metadata.prewarm_you_tube_ids(&songs, 12).await;
}

#[cfg(test)]
#[path = "external_search_service_tests.rs"]
mod tests;
