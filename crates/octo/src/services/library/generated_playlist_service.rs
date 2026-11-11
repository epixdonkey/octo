//! The generated playlists and their state file, `generated-playlists.json`.
//!
//! Genre, decade, year, country and artist mixes built from the listener's own library and
//! served like radio stations: per listener, read-only, under ids that start "og" so they
//! can never be mistaken for a Navidrome playlist. Nothing is written to Navidrome.
//!
//! Year mixes are counted from the same walk of the library as Made for you. Artist mixes are
//! the listener's most played artists, each drawn once a period from the artist's own songs
//! and the library's songs by the artists Last.fm calls similar, and kept on disk as Made for
//! you is.
//!
//! Which mixes exist is decided from counts refreshed every RefreshHours, with hysteresis so a
//! genre sitting at the threshold does not appear and vanish on alternate days. What is in a mix
//! is a seeded draw per listener and period, so it holds still for the period and every client
//! sees the same tracks, then changes. The rules are
//! `octo_core::library::generated_playlist_service`.
//!
//! The Made for you lists (New Releases, Rediscover, Deep Cuts) live here too, on by default and
//! switched apart from the mixes: built in the same refresh, from one walk of the listener's
//! library as they see it, and kept on disk, so opening one never waits on a build. The picking
//! is `octo_core::library::for_you_lists`.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Datelike, SecondsFormat, TimeDelta, Utc};
use indexmap::IndexMap;
use octo_core::common::{SingleFlight, dotnet};
use octo_core::json::dom::Node;
use octo_core::last_fm::last_fm_radio_recommendation_service::ARTIST_RADIO_NEIGHBOURS;
use octo_core::library::for_you_lists;
use octo_core::library::generated_playlist_service::{
    ARTIST_MIX_SLACK, ARTIST_PREFIX, ArtistMix, BLEND_CANDIDATES, COUNTRY_PREFIX, FIRST_DECADE,
    MAX_GENRE_PAGES, POOL_PAGE, StateDocument, UserMixes, YEAR_PREFIX, active_mixes, apply_hysteresis,
    artist_mix_key, artist_mixes_on, blend, count_years, country_mixes_on, describe, dotnet_ticks,
    for_you_on, is_stale, kept_artist_mixes, kinds_of, mixes_on, parse_genres, period_index, playlist_id,
    seed, select, str_of, user_key, year_mixes_on,
};
use octo_core::models::domain::Song;
use octo_core::models::radio::{LastFmRadioStation, LastFmRadioStationKind};
use octo_core::settings::{ExplicitFilter, GeneratedPlaylistSettings, SettingsStore};
use octo_subsonic::subsonic_response_builder::SubsonicResponseBuilder;
use parking_lot::Mutex;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

pub use octo_core::library::generated_playlist_service::GeneratedPlaylist;

use super::artist_country::CountryLibrary;
use super::for_you_lists::{CatalogSeed, Entry, NewReleasesBuilder, mint};
use crate::services::family::{FamilyStore, SavedStore, kid_filter};
use crate::services::framework::{DotnetDictionary, MemoryCache};
use crate::services::last_fm::{LastFmService, OperationCanceled};
use crate::services::state_file;
use crate::services::subsonic::SubsonicProxyService;

const FIRST_LIST_WAIT: Duration = Duration::from_millis(1500);
const REFRESH_DEADLINE: Duration = Duration::from_secs(120);
// New Releases asks the catalog for up to NewReleaseArtists artists on its background lane.
const FOR_YOU_REFRESH_DEADLINE: Duration = Duration::from_secs(6 * 60);
/// A library bigger than this is walked this far: 40,000 songs.
pub const MAX_WALK_PAGES: i32 = 80;
const FOR_YOU_PREFIX: &str = "for:";
// Where a stored outside song of New Releases keeps what it is made from; never sent out.
const SEED_FIELD: &str = "octoCatalog";
const RETRY_AFTER_FAILURE: TimeDelta = TimeDelta::minutes(5);
/// How long a listener's hearts and ratings, read once, serve every list they open.
const MARKS_FOR: Duration = Duration::from_secs(60);
/// The most ratings set through Octo remembered per listener.
const RATED_KEPT: usize = 1000;

/// A library song's marks as its listener left them: when it was hearted, and its rating.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Marks {
    pub starred: Option<String>,
    pub rating: Option<i64>,
}

/// A listener's hearted songs, by id.
pub type MarksById = HashMap<String, Marks>;

/// The listeners' mixes and when each was last counted, under `_lock`.
#[derive(Default)]
struct Mixes {
    users: IndexMap<String, UserMixes>,
    last_attempt: HashMap<String, DateTime<Utc>>,
}

pub struct GeneratedPlaylistService {
    state_path: Option<PathBuf>,
    /// A background `SubsonicProxyService`, with no request behind it.
    proxy: SubsonicProxyService,
    /// The generated playlist and genre settings, read at use.
    settings: Arc<SettingsStore>,
    refreshes: SingleFlight<String, bool>,
    /// The draws, and the Discovery blend's candidates, until their period ends.
    drawn: MemoryCache<Arc<Vec<Node>>>,
    /// Each listener's hearts, read from Navidrome at most once a minute.
    marks: MemoryCache<Arc<MarksById>>,
    /// The same read's songs, as Navidrome described them, for Liked Songs.
    starred_songs: MemoryCache<Arc<Vec<Node>>>,
    /// Ratings set through Octo, per listener, with when: Navidrome lists ratings only on
    /// hearted songs, so these keep the rest of a list current for as long as a read of the
    /// hearts is (a rating changed in another app since is not seen here).
    rated: Mutex<HashMap<String, IndexMap<String, (i64, Instant)>>>,
    state: Mutex<Mixes>,
    /// What New Releases needs, used when it builds the list: None leaves New Releases as it
    /// was.
    for_you: Option<ForYouServices>,
    /// The library walk and artist countries the country mixes share. None: no country mixes.
    countries: Option<Arc<CountryLibrary>>,
    /// Each listener's library song ids from their last walk: their country mixes count only
    /// their own songs.
    listener_songs: Mutex<HashMap<String, Arc<HashSet<String>>>>,
    /// The family, whose kid profiles get clean lists.
    family: Option<Arc<FamilyStore>>,
    /// The saved outside songs (`family-saved.json`), which join Liked Songs beside Navidrome's
    /// hearts.
    saved: Arc<SavedStore>,
    /// Last.fm, for the artists like each artist mix's own. None: artist mixes are the artist's
    /// own songs.
    last_fm: Option<Arc<LastFmService>>,
}

/// The services New Releases is made with: the catalog walk, and the outside song's JSON.
pub struct ForYouServices {
    pub builder: Arc<NewReleasesBuilder>,
    pub responses: Arc<SubsonicResponseBuilder>,
}

impl GeneratedPlaylistService {
    /// Loads the counts from `state_path` (None or blank keeps them in memory only). The saved
    /// store is the one behind `family-saved.json`, whose songs join Liked Songs.
    pub fn new(
        state_path: Option<PathBuf>,
        proxy: SubsonicProxyService,
        settings: Arc<SettingsStore>,
        saved: Arc<SavedStore>,
    ) -> Self {
        let state_path = state_path.filter(|path| !dotnet::is_blank(&path.to_string_lossy()));
        let service = GeneratedPlaylistService {
            state_path,
            proxy,
            settings,
            refreshes: SingleFlight::new(),
            drawn: MemoryCache::new(256),
            marks: MemoryCache::new(256),
            starred_songs: MemoryCache::new(256),
            rated: Mutex::new(HashMap::new()),
            state: Mutex::new(Mixes::default()),
            for_you: None,
            countries: None,
            listener_songs: Mutex::new(HashMap::new()),
            family: None,
            saved,
            last_fm: None,
        };
        service.load();
        service
    }

    /// The service with Last.fm, which names the artists like each artist mix's own.
    pub fn with_last_fm(mut self, last_fm: Arc<LastFmService>) -> Self {
        self.last_fm = Some(last_fm);
        self
    }

    /// The service with what New Releases needs.
    pub fn with_for_you(mut self, for_you: ForYouServices) -> Self {
        self.for_you = Some(for_you);
        self
    }

    /// The service with what the country mixes need.
    pub fn with_countries(mut self, countries: Arc<CountryLibrary>) -> Self {
        self.countries = Some(countries);
        self
    }

    /// Starts a pass over the library's artist countries in the background when the country
    /// mixes are on and one is due.
    fn start_country_pass(self: &Arc<Self>, settings: &GeneratedPlaylistSettings) {
        if !country_mixes_on(settings)
            || !self
                .countries
                .as_ref()
                .is_some_and(|countries| countries.pass_due())
        {
            return;
        }
        let service = Arc::clone(self);
        tokio::spawn(async move { service.country_pass().await });
    }

    /// One pass over the library's artist countries (see [`CountryLibrary::run_pass`]); when it
    /// learned something, every listener's country mixes are counted again from it.
    pub async fn country_pass(self: &Arc<Self>) {
        let Some(countries) = &self.countries else {
            return;
        };
        let settings = self.settings.current().generated_playlists.clone();
        let budget = u32::try_from(settings.effective_country_lookups_per_refresh()).unwrap_or(0);
        if countries.run_pass(budget).await {
            self.recount_countries();
        }
    }

    /// Every listener's country mixes counted again from the last walk and what is known of
    /// its artists, over the songs of their own library, with the same thresholds as before;
    /// nothing else of their mixes changes. A listener whose library has not been walked since
    /// Octo started keeps their counts until their next refresh.
    fn recount_countries(&self) {
        let settings = self.settings.current().generated_playlists.clone();
        let Some(countries) = self.countries.as_ref() else {
            return;
        };
        let Some(walked) = countries.last_walk() else {
            return;
        };
        if !country_mixes_on(&settings) {
            return;
        }
        let listeners = self.listener_songs.lock().clone();
        let codes = settings.country_codes();
        let mut state = self.state.lock();
        for (user, mixes) in state.users.iter_mut().filter(|(user, mixes)| {
            mixes.kinds
                == kinds_of(
                    &settings,
                    self.explicit_filter_now(user),
                    self.skips_covers_and_instrumentals(),
                )
        }) {
            let Some(ids) = listeners.get(user) else {
                continue;
            };
            let counts = walked.counts_among(countries.lookup(), &codes, ids);
            mixes.counts.retain(|key, _| !key.starts_with(COUNTRY_PREFIX));
            mixes.counts.extend(counts.clone());
            let shown = apply_hysteresis(
                &counts,
                &mixes.active,
                settings.effective_create_at(),
                settings.effective_remove_below(),
                settings.effective_max_country_playlists(),
            );
            mixes.active.retain(|key| !key.starts_with(COUNTRY_PREFIX));
            mixes.active.extend(shown);
        }
        self.save_locked(&state);
    }

    /// The listener's own songs whose artists are from `code`: which songs those are comes from
    /// the shared walk, and each song is as the listener's own walk describes it, with their
    /// plays and ratings.
    async fn country_pool(
        &self,
        username: &str,
        code: &str,
        auth: &IndexMap<String, String>,
        cancellation_token: &CancellationToken,
    ) -> anyhow::Result<Vec<Node>> {
        let Some(countries) = &self.countries else {
            return Ok(Vec::new());
        };
        let walked = match countries.last_walk() {
            Some(walked) => walked,
            None => match countries.walk().await {
                Some(walked) => walked,
                None => return Ok(Vec::new()),
            },
        };
        let from = walked.ids_from(countries.lookup(), code);
        if from.is_empty() {
            return Ok(Vec::new());
        }
        let Some(mine) = self.walk_library(auth, cancellation_token).await? else {
            return Ok(Vec::new());
        };
        self.remember_songs(username, &mine);
        Ok(mine
            .into_iter()
            .filter(|song| str_of(song, "id").is_some_and(|id| from.contains(id)))
            .collect())
    }

    /// The ids of the listener's library as their walk read it, for counting their country
    /// mixes.
    fn remember_songs(&self, username: &str, songs: &[Node]) -> Arc<HashSet<String>> {
        let ids: Arc<HashSet<String>> = Arc::new(
            songs
                .iter()
                .filter_map(|song| str_of(song, "id").map(str::to_string))
                .collect(),
        );
        self.listener_songs
            .lock()
            .insert(user_key(username), Arc::clone(&ids));
        ids
    }

    /// The service with the family, so a kid profile's lists are clean.
    pub fn with_family(mut self, family: Arc<FamilyStore>) -> Self {
        self.family = Some(family);
        self
    }

    /// The explicit filter for one listener's lists, which New Releases' outside songs obey as
    /// search and radio's do: the server's, or clean only for a kid profile. It is part of what
    /// decides the lists, so changing it makes them anew.
    fn explicit_filter_now(&self, username: &str) -> ExplicitFilter {
        kid_filter::explicit_filter_for(&self.settings.current(), self.family.as_deref(), Some(username))
    }

    /// Skip covers and instrumentals, which New Releases' outside songs obey as search and radio's do. Also
    /// part of what decides the lists.
    fn skips_covers_and_instrumentals(&self) -> bool {
        self.settings.current().soulseek.skip_covers_and_instrumentals
    }

    /// The listener's mixes, refreshing the counts behind them when they are stale. The first
    /// list ever waits a moment for them, so mixes show on a listener's first look rather than
    /// their second; later refreshes run behind the answer.
    pub async fn list(
        self: &Arc<Self>,
        username: &str,
        auth: &IndexMap<String, String>,
    ) -> Vec<GeneratedPlaylist> {
        let settings = self.settings.current().generated_playlists.clone();
        if dotnet::is_blank(username) || !(mixes_on(&settings) || settings.any_for_you()) {
            return Vec::new();
        }
        self.start_country_pass(&settings);

        let user = user_key(username);
        let now = Utc::now();
        let explicit_filter = self.explicit_filter_now(username);
        let skip_covers_and_instrumentals = self.skips_covers_and_instrumentals();
        let (known, due) = {
            let mut state = self.state.lock();
            let current = state.users.get(&user);
            let known = current.is_some();
            let due = current.is_none_or(|current| {
                is_stale(
                    current,
                    &settings,
                    explicit_filter,
                    skip_covers_and_instrumentals,
                    now,
                )
            }) && state
                .last_attempt
                .get(&user)
                .is_none_or(|attempted| now - *attempted >= RETRY_AFTER_FAILURE);
            if due {
                state.last_attempt.insert(user.clone(), now);
            }
            (known, due)
        };

        if due {
            let service = Arc::clone(self);
            let name = username.to_string();
            let copy = auth.clone();
            let refresh = self.refreshes.run(
                user,
                move |token| async move { service.refresh(&name, &copy, &token).await },
                if settings.new_releases {
                    FOR_YOU_REFRESH_DEADLINE
                } else {
                    REFRESH_DEADLINE
                },
            );
            let name = username.to_string();
            let logged = tokio::spawn(async move {
                if let Err(error) = refresh.await {
                    debug!("Mix refresh failed for {name}: {error:#}");
                }
            });
            if !known {
                let _ = tokio::time::timeout(FIRST_LIST_WAIT, logged).await;
            }
        }
        self.current(username, &settings, Utc::now())
    }

    /// The listener's mixes as of now, without refreshing anything or asking Navidrome.
    pub fn current_lists(&self, username: &str) -> Vec<GeneratedPlaylist> {
        let settings = self.settings.current().generated_playlists.clone();
        if dotnet::is_blank(username) {
            return Vec::new();
        }
        self.current(username, &settings, Utc::now())
    }

    /// A mix by id, for this listener, as of now. None for anyone else's id.
    pub fn find(&self, username: &str, id: &str) -> Option<GeneratedPlaylist> {
        let settings = self.settings.current().generated_playlists.clone();
        if id.is_empty() || !id.starts_with("og") {
            return None;
        }
        self.current(username, &settings, Utc::now())
            .into_iter()
            .find(|mix| mix.id == id)
    }

    fn current(
        &self,
        username: &str,
        settings: &GeneratedPlaylistSettings,
        now_utc: DateTime<Utc>,
    ) -> Vec<GeneratedPlaylist> {
        let user = user_key(username);
        let hours = settings.effective_refresh_hours();
        let period = period_index(now_utc, hours);
        let start = DateTime::UNIX_EPOCH + TimeDelta::hours(period * i64::from(hours));
        let end = start + TimeDelta::hours(i64::from(hours));

        let state = self.state.lock();
        let Some(mixes) = state.users.get(&user) else {
            return Vec::new();
        };
        // Made for you first, newest to oldest, and only a list with something in it.
        let for_you = for_you_lists::KINDS
            .iter()
            .filter(|kind| {
                for_you_on(settings, kind) && mixes.for_you.get(**kind).is_some_and(|songs| !songs.is_empty())
            })
            .map(|kind| {
                let key = format!("{FOR_YOU_PREFIX}{kind}");
                GeneratedPlaylist {
                    id: playlist_id(username, &key),
                    key,
                    kind: kind.to_string(),
                    label: for_you_lists::name(kind).to_string(),
                    name: for_you_lists::name(kind).to_string(),
                    owner: username.trim().to_string(),
                    pool_size: mixes.for_you.get(*kind).map_or(0, Vec::len) as i32,
                    period_start_utc: start,
                    period_end_utc: end,
                }
            });
        let mixes_shown = mixes_on(settings);
        let list: Vec<GeneratedPlaylist> = for_you.collect();
        // Artist mixes last, in rank order, each named after its artist by the name format.
        let artists = mixes
            .artist_mixes
            .iter()
            .filter(|_| mixes_shown && settings.artists)
            .filter(|(_, mix)| !mix.songs.is_empty())
            .map(|(key, mix)| GeneratedPlaylist {
                id: playlist_id(username, key),
                key: key.clone(),
                kind: "artist".to_string(),
                label: mix.name.clone(),
                name: settings.name(&mix.name),
                owner: username.trim().to_string(),
                pool_size: i32::try_from(mix.songs.len()).unwrap_or(i32::MAX),
                period_start_utc: start,
                period_end_utc: end,
            });
        list.into_iter()
            .chain(
                mixes
                    .active
                    .iter()
                    .filter(|_| mixes_shown)
                    .filter(|key| {
                        (settings.genres && key.starts_with("genre:"))
                            || (settings.decades && key.starts_with("decade:"))
                            || (settings.country_mixes && key.starts_with(COUNTRY_PREFIX))
                            || (settings.years && key.starts_with(YEAR_PREFIX))
                    })
                    .map(|key| {
                        let (kind, label) = describe(key);
                        GeneratedPlaylist {
                            id: playlist_id(username, key),
                            key: key.clone(),
                            name: settings.name(&label),
                            kind,
                            label,
                            owner: username.trim().to_string(),
                            pool_size: mixes.counts.get(key).copied().unwrap_or(0),
                            period_start_utc: start,
                            period_end_utc: end,
                        }
                    }),
            )
            .chain(artists)
            .collect()
    }

    /// The album covers New Releases' own cover is colored from, up to four.
    pub fn for_you_covers(&self, username: &str) -> Vec<String> {
        self.state
            .lock()
            .users
            .get(&user_key(username))
            .map(|state| state.for_you_covers.iter().take(4).cloned().collect())
            .unwrap_or_default()
    }

    /// Count what the listener's library holds per genre and decade, and decide which mixes they
    /// have. Only a complete count replaces the last one: a Navidrome that stops answering half
    /// way must not make every mix vanish.
    async fn refresh(
        &self,
        username: &str,
        auth: &IndexMap<String, String>,
        cancellation_token: &CancellationToken,
    ) -> anyhow::Result<bool> {
        let settings = self.settings.current().generated_playlists.clone();
        let mut counts: IndexMap<String, i32> = IndexMap::new();

        if mixes_on(&settings) && settings.genres {
            let Some(genres) = self.count_genres(auth).await else {
                return Ok(false);
            };
            counts.extend(genres);
        }
        if mixes_on(&settings) && settings.decades {
            let last = Utc::now().year() / 10 * 10;
            let mut decade = FIRST_DECADE;
            while decade <= last {
                if cancellation_token.is_cancelled() {
                    return Err(OperationCanceled.into());
                }
                let extra = [
                    ("size", POOL_PAGE.to_string()),
                    ("fromYear", decade.to_string()),
                    ("toYear", (decade + 9).to_string()),
                ];
                let Some(songs) = self
                    .fetch_songs(auth, "rest/getRandomSongs", "randomSongs", &extra)
                    .await
                else {
                    return Ok(false);
                };
                counts.insert(format!("decade:{decade}"), songs.len() as i32);
                decade += 10;
            }
        }
        // One walk of the listener's own library, for Made for you and their country, year and
        // artist mixes.
        let countries_on = country_mixes_on(&settings) && self.countries.is_some();
        let years_on = year_mixes_on(&settings);
        let artists_on = artist_mixes_on(&settings);
        let library = if settings.any_for_you() || countries_on || years_on || artists_on {
            self.walk_library(auth, cancellation_token).await?
        } else {
            None
        };
        // Years from the same walk. A walk that failed keeps the year counts they had.
        if years_on {
            match library.as_deref() {
                Some(songs) => counts.extend(count_years(songs)),
                None => counts.extend(self.counts_before(username, YEAR_PREFIX)),
            }
        }
        // Countries from the walk every listener shares, as far as their artists are known by
        // now, counted over the listener's own songs: a pass that learns more counts them again.
        // A walk that failed keeps the country counts they had.
        if countries_on {
            let mine = library
                .as_deref()
                .map(|songs| self.remember_songs(username, songs));
            if let Some(countries) = &self.countries
                && let Some(walked) = countries.last_walk()
            {
                match mine {
                    Some(ids) => {
                        counts.extend(walked.counts_among(
                            countries.lookup(),
                            &settings.country_codes(),
                            &ids,
                        ));
                    }
                    None => counts.extend(self.counts_before(username, COUNTRY_PREFIX)),
                }
            }
        }

        let explicit_filter = self.explicit_filter_now(username);
        let skip_covers_and_instrumentals = self.skips_covers_and_instrumentals();
        let before = self.state.lock().users.get(&user_key(username)).cloned();
        // The artist mixes read the walk once into an index, off the async threads, and draw every
        // artist's mix from it.
        let (library, artist_index) = match library {
            Some(songs) if artists_on => {
                let (songs, index) = tokio::task::spawn_blocking(move || {
                    let index = for_you_lists::ArtistIndex::new(&songs);
                    (songs, index)
                })
                .await?;
                (Some(songs), Some(index))
            }
            library => (library, None),
        };
        let artist_mixes = if artists_on {
            self.build_artist_mixes(
                username,
                library.as_deref().zip(artist_index.as_ref()),
                &settings,
                before.as_ref(),
                cancellation_token,
            )
            .await?
        } else {
            IndexMap::new()
        };
        let (for_you, covers) = if settings.any_for_you() {
            self.build_for_you(
                username,
                library,
                &settings,
                explicit_filter,
                skip_covers_and_instrumentals,
                before.as_ref(),
                cancellation_token,
            )
            .await?
        } else {
            (IndexMap::new(), Vec::new())
        };

        {
            let mut state = self.state.lock();
            let user = user_key(username);
            let previous = state
                .users
                .get(&user)
                .map(|old| old.active.clone())
                .unwrap_or_default();
            let active = active_mixes(&counts, &previous, &settings);
            state.users.insert(
                user,
                UserMixes {
                    active,
                    counts: counts.clone(),
                    counts_utc: Utc::now(),
                    kinds: kinds_of(&settings, explicit_filter, skip_covers_and_instrumentals),
                    for_you,
                    for_you_covers: covers,
                    artist_mixes,
                },
            );
            self.save_locked(&state);
        }
        info!(
            "Mixes for {username}: {} from {} genres and decades",
            counts.values().filter(|count| **count > 0).count(),
            counts.len()
        );
        Ok(true)
    }

    /// The counts the listener's last refresh made for one kind of mix, by its key's prefix.
    fn counts_before(&self, username: &str, prefix: &str) -> Vec<(String, i32)> {
        self.state
            .lock()
            .users
            .get(&user_key(username))
            .map(|before| {
                before
                    .counts
                    .iter()
                    .filter(|(key, _)| key.starts_with(prefix))
                    .map(|(key, count)| (key.clone(), *count))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The listener's artist mixes for this period, from their walk and its
    /// [`for_you_lists::ArtistIndex`] (None when the walk failed, which keeps the artist mixes
    /// they had): their most played artists by [`kept_artist_mixes`], each drawn again from
    /// their own songs and the songs of the artists Last.fm calls like them.
    async fn build_artist_mixes(
        &self,
        username: &str,
        walk: Option<(&[Node], &for_you_lists::ArtistIndex)>,
        settings: &GeneratedPlaylistSettings,
        before: Option<&UserMixes>,
        cancellation_token: &CancellationToken,
    ) -> anyhow::Result<IndexMap<String, ArtistMix>> {
        let previous = before
            .map(|before| before.artist_mixes.clone())
            .unwrap_or_default();
        let Some((library, index)) = walk else {
            return Ok(previous);
        };
        let count = usize::try_from(settings.effective_artist_count()).unwrap_or(1);
        let ranked = for_you_lists::top_artists(library, count + ARTIST_MIX_SLACK);
        let keys: Vec<String> = ranked.iter().map(|artist| artist_mix_key(&artist.key)).collect();
        let began: IndexMap<String, DateTime<Utc>> = previous
            .iter()
            .map(|(key, mix)| (key.clone(), mix.since))
            .collect();
        let now = Utc::now();
        let period = period_index(now, settings.effective_refresh_hours());
        let mut mixes: IndexMap<String, ArtistMix> = IndexMap::new();
        for key in kept_artist_mixes(&keys, &began, count, now) {
            if cancellation_token.is_cancelled() {
                return Err(OperationCanceled.into());
            }
            let artist = key.strip_prefix(ARTIST_PREFIX).unwrap_or_default();
            let name = ranked
                .iter()
                .find(|ranked| ranked.key == artist)
                .map(|ranked| ranked.name.clone())
                .or_else(|| previous.get(&key).map(|mix| mix.name.clone()))
                .unwrap_or_default();
            if name.is_empty() {
                continue;
            }
            let similar = self.similar_artists(&name).await;
            let songs = for_you_lists::artist_mix_in(
                library,
                index,
                artist,
                &similar,
                usize::try_from(settings.effective_track_count()).unwrap_or(0),
                settings.effective_max_per_artist(),
                seed(username, &key, period),
            );
            // An artist whose songs have all left the library has no mix.
            if songs.is_empty() {
                continue;
            }
            mixes.insert(
                key.clone(),
                ArtistMix {
                    name,
                    since: began.get(&key).copied().unwrap_or(now),
                    songs: songs.iter().map(|song| song.to_json_string(false)).collect(),
                },
            );
        }
        Ok(mixes)
    }

    /// The artists Last.fm calls like `artist`, by name; none without a Last.fm key or when
    /// Last.fm does not answer. Asked for as many as the artist stations ask for, so the two
    /// share Last.fm's answer.
    async fn similar_artists(&self, artist: &str) -> Vec<String> {
        let Some(last_fm) = self.last_fm.as_ref().filter(|last_fm| last_fm.has_api_key()) else {
            return Vec::new();
        };
        last_fm
            .get_similar_artists(artist, ARTIST_RADIO_NEIGHBOURS)
            .await
            .map(|similar| similar.into_iter().map(|similar| similar.name).collect())
            .unwrap_or_default()
    }

    /// An artist mix's songs as stored for this period.
    fn stored_artist_mix(&self, username: &str, key: &str) -> Vec<Node> {
        self.state
            .lock()
            .users
            .get(&user_key(username))
            .and_then(|state| state.artist_mixes.get(key))
            .map(|mix| {
                mix.songs
                    .iter()
                    .filter_map(|song| Node::parse(song).ok())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Made for you, from one walk of the listener's library as they see it (None when it
    /// failed). A list that could not be made this time (Navidrome or the catalog not answering)
    /// keeps what it had, so a bad day never empties it.
    async fn build_for_you(
        &self,
        username: &str,
        library: Option<Vec<Node>>,
        settings: &GeneratedPlaylistSettings,
        explicit_filter: ExplicitFilter,
        skip_covers_and_instrumentals: bool,
        before: Option<&UserMixes>,
        cancellation_token: &CancellationToken,
    ) -> anyhow::Result<(IndexMap<String, Vec<String>>, Vec<String>)> {
        let mut lists: IndexMap<String, Vec<String>> = IndexMap::new();
        let mut covers = before
            .map(|before| before.for_you_covers.clone())
            .unwrap_or_default();
        let keep_old = |lists: &mut IndexMap<String, Vec<String>>, kind: &str| {
            if let Some(old) = before.and_then(|before| before.for_you.get(kind)) {
                lists.insert(kind.to_string(), old.clone());
            }
        };

        let Some(library) = library else {
            for kind in for_you_lists::KINDS
                .iter()
                .filter(|kind| for_you_on(settings, kind))
            {
                keep_old(&mut lists, kind);
            }
            return Ok((lists, covers));
        };

        let now = Utc::now();
        let period = period_index(now, settings.effective_refresh_hours());
        let rediscover = for_you_lists::rediscover(
            &library,
            now,
            settings.effective_rediscover_months(),
            seed(
                username,
                &format!("{FOR_YOU_PREFIX}{}", for_you_lists::REDISCOVER_KIND),
                period,
            ),
        );
        if settings.rediscover {
            lists.insert(
                for_you_lists::REDISCOVER_KIND.to_string(),
                rediscover.iter().map(|song| song.to_json_string(false)).collect(),
            );
        }
        if settings.deep_cuts {
            let artists: Vec<String> = for_you_lists::top_artists(&library, for_you_lists::DEEP_CUT_ARTISTS)
                .into_iter()
                .map(|artist| artist.key)
                .collect();
            let excluded: std::collections::HashSet<String> = rediscover
                .iter()
                .filter_map(|song| str_of(song, "id").map(str::to_string))
                .collect();
            let deep_cuts = for_you_lists::deep_cuts(
                &library,
                &artists,
                &excluded,
                seed(
                    username,
                    &format!("{FOR_YOU_PREFIX}{}", for_you_lists::DEEP_CUTS_KIND),
                    period,
                ),
            );
            lists.insert(
                for_you_lists::DEEP_CUTS_KIND.to_string(),
                deep_cuts.iter().map(|song| song.to_json_string(false)).collect(),
            );
        }
        // Rediscover and Deep Cuts are ready in moments; New Releases waits on the catalog, a
        // minute or more the first time. Shown at once, a new listener's first lists never wait
        // on it.
        if settings.new_releases {
            self.publish_early(username, &lists);
            match &self.for_you {
                None => keep_old(&mut lists, for_you_lists::NEW_RELEASES_KIND),
                Some(services) => {
                    let built = services
                        .builder
                        .build(
                            &for_you_lists::top_artists(
                                &library,
                                settings.effective_new_release_artists() as usize,
                            ),
                            &library,
                            now.date_naive(),
                            settings.effective_new_release_weeks(),
                            cancellation_token,
                            explicit_filter,
                            skip_covers_and_instrumentals,
                        )
                        .await?;
                    if built.entries.is_empty() && !built.whole {
                        keep_old(&mut lists, for_you_lists::NEW_RELEASES_KIND);
                    } else {
                        lists.insert(
                            for_you_lists::NEW_RELEASES_KIND.to_string(),
                            built
                                .entries
                                .iter()
                                .map(|entry| stored_entry(entry, &services.responses))
                                .collect(),
                        );
                        covers = Vec::new();
                        for cover in built.entries.iter().filter_map(|entry| entry.cover_url.clone()) {
                            if !covers.contains(&cover) {
                                covers.push(cover);
                            }
                            if covers.len() == 4 {
                                break;
                            }
                        }
                    }
                }
            }
        }
        info!(
            "Made for you for {username}: {} from {} library songs",
            lists
                .iter()
                .map(|(kind, songs)| format!("{} {}", for_you_lists::name(kind), songs.len()))
                .collect::<Vec<_>>()
                .join(", "),
            library.len()
        );
        Ok((lists, covers))
    }

    /// Lists shown before the whole build is done, without touching the lists the build is still
    /// reading. A listener with none yet gets a state that is still due, so the build finishes it.
    fn publish_early(&self, username: &str, lists: &IndexMap<String, Vec<String>>) {
        if lists.is_empty() {
            return;
        }
        let mut state = self.state.lock();
        let user = user_key(username);
        match state.users.get_mut(&user) {
            Some(mixes) => {
                for (kind, songs) in lists {
                    mixes.for_you.insert(kind.clone(), songs.clone());
                }
            }
            None => {
                state.users.insert(
                    user,
                    UserMixes {
                        for_you: lists.clone(),
                        ..Default::default()
                    },
                );
            }
        }
    }

    /// Every song of the listener's library as they see it: [`walk_library`].
    async fn walk_library(
        &self,
        auth: &IndexMap<String, String>,
        cancellation_token: &CancellationToken,
    ) -> anyhow::Result<Option<Vec<Node>>> {
        walk_library(&self.proxy, auth, cancellation_token).await
    }

    /// A Made for you list as stored. Each outside song is made again from its seed, with the
    /// same id, so it plays even after a busy day has pushed it out of the outside songs Octo
    /// remembers; the seed itself is never sent out.
    fn stored_for_you(&self, username: &str, kind: &str) -> Vec<Node> {
        let stored: Option<Vec<String>> = self
            .state
            .lock()
            .users
            .get(&user_key(username))
            .and_then(|state| state.for_you.get(kind).cloned());
        let Some(stored) = stored else {
            return Vec::new();
        };
        let mut parsed: Vec<Node> = stored.iter().filter_map(|song| Node::parse(song).ok()).collect();
        for song in &mut parsed {
            let Some(fields) = song.as_object_mut() else {
                continue;
            };
            let Some(node) = fields.shift_remove(SEED_FIELD) else {
                continue;
            };
            if !node.is_object() {
                continue;
            }
            let Ok(seed) = serde_json::from_value::<CatalogSeed>(node.to_value()) else {
                continue;
            };
            if let Some(services) = &self.for_you {
                mint(services.builder.registry(), &seed);
            }
        }
        parsed
    }

    /// Genres by song count. Spellings that differ only in case are one genre, under the
    /// spelling with the most songs; years and the genre blocklist are never a mix of their own.
    async fn count_genres(&self, auth: &IndexMap<String, String>) -> Option<IndexMap<String, i32>> {
        let result = self
            .proxy
            .relay_safe("rest/getGenres", parameters(auth, &[]))
            .await?;
        if result.body.is_empty() {
            return None;
        }
        let root = Node::parse(&String::from_utf8_lossy(&result.body)).ok()?;
        let response = root.get("subsonic-response")?;
        if response.get("status").and_then(Node::as_str) != Some("ok") {
            return None;
        }
        let blocked = self.settings.current().genre.effective_blocklist();
        let rows = match response.get("genres").and_then(|genres| genres.get("genre")) {
            Some(Node::Array(rows)) => Some(rows.as_slice()),
            _ => None,
        };
        Some(parse_genres(rows, &blocked))
    }

    /// The songs of one mix for this period: the same for every request in the period.
    pub async fn materialize(
        &self,
        username: &str,
        playlist: &GeneratedPlaylist,
        auth: &IndexMap<String, String>,
        cancellation_token: &CancellationToken,
    ) -> anyhow::Result<Vec<Node>> {
        if for_you_lists::is_for_you(&playlist.kind) {
            return Ok(self.stored_for_you(username, &playlist.kind));
        }
        if playlist.key.starts_with(ARTIST_PREFIX) {
            return Ok(self.stored_artist_mix(username, &playlist.key));
        }
        let cache_key = draw_key(username, playlist);
        if let Some(cached) = self.drawn.get(&cache_key) {
            return Ok(cached.as_ref().clone());
        }

        let settings = self.settings.current().generated_playlists.clone();
        let mut pool: Vec<Node> = Vec::new();
        if let Some(code) = playlist.key.strip_prefix(COUNTRY_PREFIX) {
            pool = self
                .country_pool(username, code, auth, cancellation_token)
                .await?;
        } else if playlist.kind == "decade" {
            let decade: i32 = playlist
                .key
                .get("decade:".len()..)
                .unwrap_or_default()
                .parse()
                .map_err(|_| {
                    anyhow::anyhow!("The input string '{}' was not in a correct format.", playlist.key)
                })?;
            let extra = [
                ("size", POOL_PAGE.to_string()),
                ("fromYear", decade.to_string()),
                ("toYear", (decade + 9).to_string()),
            ];
            pool.extend(
                self.fetch_songs(auth, "rest/getRandomSongs", "randomSongs", &extra)
                    .await
                    .unwrap_or_default(),
            );
        } else if let Some(year) = playlist.key.strip_prefix(YEAR_PREFIX) {
            let year: i32 = year.parse().map_err(|_| {
                anyhow::anyhow!("The input string '{}' was not in a correct format.", playlist.key)
            })?;
            let extra = [
                ("size", POOL_PAGE.to_string()),
                ("fromYear", year.to_string()),
                ("toYear", year.to_string()),
            ];
            pool.extend(
                self.fetch_songs(auth, "rest/getRandomSongs", "randomSongs", &extra)
                    .await
                    .unwrap_or_default(),
            );
        } else {
            for page in 0..MAX_GENRE_PAGES {
                if cancellation_token.is_cancelled() {
                    return Err(OperationCanceled.into());
                }
                let extra = [
                    ("genre", playlist.label.clone()),
                    ("count", POOL_PAGE.to_string()),
                    ("offset", (page * POOL_PAGE).to_string()),
                ];
                let Some(songs) = self
                    .fetch_songs(auth, "rest/getSongsByGenre", "songsByGenre", &extra)
                    .await
                else {
                    break;
                };
                let full = songs.len() as i32 >= POOL_PAGE;
                pool.extend(songs);
                if !full {
                    break;
                }
            }
        }

        let period = period_index(playlist.period_start_utc, settings.effective_refresh_hours());
        let drawn = select(
            &pool,
            settings.effective_track_count(),
            settings.effective_max_per_artist(),
            settings.effective_new_share(),
            settings.effective_new_days(),
            Utc::now(),
            seed(username, &playlist.key, period),
        );
        if !pool.is_empty() {
            self.drawn.set(
                cache_key,
                Arc::new(drawn.clone()),
                1,
                until(playlist.period_end_utc),
            );
        }
        Ok(drawn)
    }

    /// The songs of one mix for this period if they have been drawn already, else None. Never
    /// fetches.
    pub fn drawn(&self, username: &str, playlist: &GeneratedPlaylist) -> Option<Vec<Node>> {
        if for_you_lists::is_for_you(&playlist.kind) {
            return Some(self.stored_for_you(username, &playlist.kind));
        }
        if playlist.key.starts_with(ARTIST_PREFIX) {
            return Some(self.stored_artist_mix(username, &playlist.key));
        }
        self.drawn
            .get(&draw_key(username, playlist))
            .map(|cached| cached.as_ref().clone())
    }

    /// Keep MIX_NEW_SHARE percent of Discovery Radio for library songs new to the listener.
    /// Discovery is otherwise all outside the library, so what someone added and never played was
    /// the one thing it could not surface. The station's songs come back as they were when there
    /// is nothing to put in.
    pub async fn blend_into_discovery(
        &self,
        username: &str,
        station: &LastFmRadioStation,
        songs: Vec<Song>,
        auth: &IndexMap<String, String>,
    ) -> Vec<Song> {
        let settings = self.settings.current().generated_playlists.clone();
        let share = settings.effective_new_share();
        if share == 0 || station.kind != LastFmRadioStationKind::Discovery || songs.is_empty() {
            return songs;
        }
        let wanted = (songs.len() as f64 * f64::from(share) / 100.0).round() as i32;
        if wanted == 0 {
            return songs;
        }

        // Only the candidates are cached, and applied to the station as it is now: a track that
        // became a library song since must not be put back as it was.
        let cache_key = format!(
            "blend|{}|{}|{}",
            user_key(username),
            station.id,
            dotnet_ticks(station.changed_utc)
        );
        let candidates = match self.drawn.get(&cache_key) {
            Some(candidates) => candidates,
            None => {
                let extra = [("size", BLEND_CANDIDATES.to_string())];
                let Some(fetched) = self
                    .fetch_songs(auth, "rest/getRandomSongs", "randomSongs", &extra)
                    .await
                else {
                    return songs;
                };
                let fetched = Arc::new(fetched);
                let now = Utc::now();
                let expires = if station.valid_until_utc > now {
                    station.valid_until_utc
                } else {
                    now + TimeDelta::hours(1)
                };
                self.drawn.set(cache_key, Arc::clone(&fetched), 1, until(expires));
                fetched
            }
        };
        match blend(
            &songs,
            &candidates,
            wanted,
            settings.effective_new_days(),
            Utc::now(),
        ) {
            Some(blended) => blended,
            None => songs,
        }
    }

    /// The listener's hearted songs with their marks, read from Navidrome once and kept a
    /// minute. None when Navidrome did not answer, so nothing is changed on its say-so.
    pub async fn marks(&self, username: &str, auth: &IndexMap<String, String>) -> Option<Arc<MarksById>> {
        if dotnet::is_blank(username) {
            return None;
        }
        let key = marks_key(username);
        if let Some(cached) = self.marks.get(&key) {
            return Some(cached);
        }
        Some(self.read_starred(username, auth).await?.0)
    }

    /// One getStarred2 read: the marks by id and the songs, both kept a minute.
    async fn read_starred(
        &self,
        username: &str,
        auth: &IndexMap<String, String>,
    ) -> Option<(Arc<MarksById>, Arc<Vec<Node>>)> {
        // Kept by username, so a caller without one would share what it read with every other.
        if dotnet::is_blank(username) {
            return None;
        }
        let songs = self
            .fetch_songs(auth, "rest/getStarred2", "starred2", &[])
            .await?;
        let marks: MarksById = songs
            .iter()
            .filter_map(|song| {
                let id = song.get("id").and_then(Node::as_str)?;
                Some((
                    id.to_string(),
                    Marks {
                        starred: song.get("starred").and_then(Node::as_str).map(str::to_string),
                        rating: song.get("userRating").and_then(node_i64),
                    },
                ))
            })
            .collect();
        let marks = Arc::new(marks);
        let songs = Arc::new(songs);
        self.marks
            .set(marks_key(username), Arc::clone(&marks), 1, MARKS_FOR);
        self.starred_songs
            .set(marks_key(username), Arc::clone(&songs), 1, MARKS_FOR);
        Some((marks, songs))
    }

    /// Liked Songs' id for this listener.
    pub fn liked_id(&self, username: &str) -> String {
        playlist_id(username, for_you_lists::LIKED_KIND)
    }

    /// Liked Songs as a playlist row, when it is on and the listener has hearted something.
    pub async fn liked_row(
        &self,
        username: &str,
        auth: &IndexMap<String, String>,
    ) -> Option<GeneratedPlaylist> {
        let settings = self.settings.current().generated_playlists.clone();
        if !settings.liked_songs || dotnet::is_blank(username) {
            return None;
        }
        let marks = self.marks(username, auth).await?;
        if marks.is_empty() {
            return None;
        }
        // The list changed when its newest heart was given; now only when no heart says when.
        let newest = marks
            .values()
            .filter_map(|mark| mark.starred.as_deref())
            .filter_map(|starred| DateTime::parse_from_rfc3339(starred).ok())
            .map(|starred| starred.with_timezone(&Utc))
            .max()
            .unwrap_or_else(Utc::now);
        let count = marks.len().min(settings.effective_liked_songs_max() as usize);
        Some(GeneratedPlaylist {
            id: self.liked_id(username),
            key: for_you_lists::LIKED_KIND.to_string(),
            kind: for_you_lists::LIKED_KIND.to_string(),
            label: settings.effective_liked_songs_name(),
            name: settings.effective_liked_songs_name(),
            owner: username.trim().to_string(),
            pool_size: i32::try_from(count).unwrap_or(i32::MAX),
            period_start_utc: newest,
            period_end_utc: newest,
        })
    }

    /// Liked Songs' songs: the listener's hearted songs, newest heart first (then by id), at most
    /// the setting's number, with the songs they saved as streaming favorites beside Navidrome's
    /// hearts (a heart with every download source off saves instead of fetching, and read_starred
    /// relays straight to Navidrome, so those never arrive here on their own). None when
    /// Navidrome did not answer, or for a caller without a username.
    pub async fn liked_songs(&self, username: &str, auth: &IndexMap<String, String>) -> Option<Vec<Node>> {
        if dotnet::is_blank(username) {
            return None;
        }
        let max = self
            .settings
            .current()
            .generated_playlists
            .effective_liked_songs_max() as usize;
        let songs = match self.starred_songs.get(&marks_key(username)) {
            Some(songs) => songs,
            None => self.read_starred(username, auth).await?.1,
        };
        let mut songs: Vec<Node> = songs.as_ref().clone();
        self.append_saved_songs(username, &mut songs);
        let text = |song: &Node, key: &str| song.get(key).and_then(Node::as_str).unwrap_or("").to_string();
        songs.sort_by(|a, b| {
            text(b, "starred")
                .cmp(&text(a, "starred"))
                .then_with(|| text(a, "id").cmp(&text(b, "id")))
        });
        songs.truncate(max);
        Some(songs)
    }

    /// The listener's saved outside songs join the hearts, one node each: id, title, artist,
    /// album, duration and the moment it was saved as `starred`, marked `isExternal` the way
    /// the builder marks every outside song so [`Self::overlay`] leaves their own marks alone.
    /// A heart whose library copy Navidrome already holds wins the row, so nothing is listed
    /// twice.
    fn append_saved_songs(&self, username: &str, songs: &mut Vec<Node>) {
        let known: std::collections::HashSet<String> = songs
            .iter()
            .filter_map(|song| song.get("id").and_then(Node::as_str).map(str::to_string))
            .collect();
        for saved in self.saved.list(username) {
            if known.contains(&saved.id) {
                continue;
            }
            songs.push(Node::from_value(&serde_json::json!({
                "id": saved.id,
                "title": saved.title,
                "artist": saved.artist,
                "album": saved.album,
                "duration": saved.duration,
                "coverArt": saved.cover_art,
                "starred": saved.saved.to_rfc3339_opts(SecondsFormat::Millis, true),
                "isExternal": true,
            })));
        }
    }

    /// The listener hearted or unhearted something: their hearts are read again next time.
    pub fn forget_marks(&self, username: &str) {
        self.marks.forget(&marks_key(username));
        self.starred_songs.forget(&marks_key(username));
    }

    /// The listener rated a song through Octo, and Navidrome took it.
    pub fn rated(&self, username: &str, id: &str, rating: i64) {
        if dotnet::is_blank(username) || id.is_empty() {
            return;
        }
        let user = user_key(username);
        {
            let mut rated = self.rated.lock();
            let mine = rated.entry(user).or_default();
            mine.shift_remove(id);
            mine.insert(id.to_string(), (rating, Instant::now()));
            while mine.len() > RATED_KEPT {
                mine.shift_remove_index(0);
            }
        }
        let key = marks_key(username);
        if let Some(cached) = self.marks.get(&key)
            && let Some(marks) = cached.get(id)
        {
            let mut patched = cached.as_ref().clone();
            patched.insert(
                id.to_string(),
                Marks {
                    rating: Some(rating),
                    ..marks.clone()
                },
            );
            self.marks.set(key, Arc::new(patched), 1, MARKS_FOR);
        }
    }

    /// Puts the listener's hearts and ratings as they are now onto library songs drawn
    /// earlier. Outside songs keep their own marks, which Navidrome does not hold. A hearted
    /// song's rating is the one Navidrome read; a rating set through Octo counts for the rest,
    /// for a minute.
    pub fn overlay(&self, username: &str, songs: &mut [Node], marks: &MarksById) {
        let rated: HashMap<String, i64> = self
            .rated
            .lock()
            .get(&user_key(username))
            .map(|mine| {
                mine.iter()
                    .filter(|(_, (_, at))| at.elapsed() < MARKS_FOR)
                    .map(|(id, (rating, _))| (id.clone(), *rating))
                    .collect()
            })
            .unwrap_or_default();
        for song in songs {
            let Some(fields) = song.as_object_mut() else {
                continue;
            };
            if matches!(fields.get("isExternal"), Some(Node::Bool(true))) {
                continue;
            }
            let Some(id) = fields.get("id").and_then(Node::as_str).map(str::to_string) else {
                continue;
            };
            let mark = marks.get(&id);
            match mark.and_then(|m| m.starred.as_deref()) {
                Some(starred) => {
                    fields.insert("starred".into(), Node::String(starred.to_string()));
                }
                None => {
                    fields.shift_remove("starred");
                }
            }
            // Navidrome leaves a hearted song's rating of 0 out, so a heart read without one is 0.
            let rating = match mark {
                Some(mark) => Some(mark.rating.unwrap_or(0)),
                None => rated.get(&id).copied(),
            };
            match rating {
                Some(0) => {
                    fields.shift_remove("userRating");
                }
                Some(rating) => {
                    fields.insert("userRating".into(), Node::Number(rating.to_string()));
                }
                // Navidrome leaves a rating of 0 out, so no rating here is not known to be 0.
                None => {}
            }
        }
    }

    /// The songs of one Subsonic call: [`fetch_songs`].
    async fn fetch_songs(
        &self,
        auth: &IndexMap<String, String>,
        endpoint: &str,
        container: &str,
        extra: &[(&str, String)],
    ) -> Option<Vec<Node>> {
        fetch_songs(&self.proxy, auth, endpoint, container, extra).await
    }

    fn load(&self) {
        let Some(path) = self.state_path.as_ref().filter(|path| path.is_file()) else {
            return;
        };
        let read = state_file::read_all_text(path)
            .map_err(|error| error.to_string())
            .and_then(|text| serde_json::from_str::<StateDocument>(&text).map_err(|error| error.to_string()));
        match read {
            Ok(document) => {
                let Some(users) = document.users else { return };
                let mut state = self.state.lock();
                for (user, mixes) in users {
                    state.users.insert(user, mixes);
                }
            }
            Err(message) => {
                // Only counts: the next list rebuilds them, so a bad file is set aside, not fatal.
                warn!("Mix state could not be read ({message}); it will be rebuilt");
                let mut corrupt = path.as_os_str().to_owned();
                corrupt.push(format!(".corrupt-{}", dotnet_ticks(Utc::now())));
                // Best effort.
                let _ = std::fs::rename(path, PathBuf::from(corrupt));
            }
        }
    }

    fn save_locked(&self, state: &Mixes) {
        let Some(path) = &self.state_path else { return };
        let document = StateDocument {
            users: Some(state.users.clone()),
        };
        if let Err(error) = state_file::save_atomic(path, &octo_core::json::to_string(&document)) {
            warn!("Mix state could not be written: {error}");
        }
    }
}

/// One entry of New Releases as stored: a library song as Navidrome sent it, or an outside song
/// with what it is made from.
fn stored_entry(entry: &Entry, responses: &SubsonicResponseBuilder) -> String {
    if let Some(library) = &entry.library {
        return library.to_json_string(false);
    }
    let Some(outside) = &entry.outside else {
        return Node::object().to_json_string(false);
    };
    let mut song = Node::from_value(&serde_json::Value::Object(
        responses.convert_song_to_json(outside),
    ));
    if let (Some(seed), Some(fields)) = (&entry.seed, song.as_object_mut())
        && let Ok(value) = serde_json::to_value(seed)
    {
        fields.insert(SEED_FIELD.to_string(), Node::from_value(&value));
    }
    song.to_json_string(false)
}

/// Every song of the listener's library as they see it, 500 at a time, up to
/// [`MAX_WALK_PAGES`] pages; None when Navidrome did not answer, even part way: lists made
/// from part of a library would show songs the listener owns as outside ones and miss their
/// plays, so the lists they have stay instead.
pub(crate) async fn walk_library(
    proxy: &SubsonicProxyService,
    auth: &IndexMap<String, String>,
    cancellation_token: &CancellationToken,
) -> anyhow::Result<Option<Vec<Node>>> {
    let mut all: Vec<Node> = Vec::new();
    for page in 0..MAX_WALK_PAGES {
        if cancellation_token.is_cancelled() {
            return Err(OperationCanceled.into());
        }
        let extra = [
            // Navidrome's whole library: an empty phrase, as the duplicate scan walks it.
            ("query", "\"\"".to_string()),
            ("songCount", POOL_PAGE.to_string()),
            ("songOffset", (page * POOL_PAGE).to_string()),
            ("albumCount", "0".to_string()),
            ("artistCount", "0".to_string()),
        ];
        let Some(songs) = fetch_songs(proxy, auth, "rest/search3", "searchResult3", &extra).await else {
            return Ok(None);
        };
        let full = songs.len() as i32 >= POOL_PAGE;
        all.extend(songs);
        if !full {
            return Ok(Some(all));
        }
    }
    info!(
        "Made for you read the first {} songs of a bigger library",
        all.len()
    );
    Ok(Some(all))
}

/// The songs of one Subsonic call, or None when it did not answer: an empty list is a real
/// answer and None is not, and only the first may replace what is known.
pub(crate) async fn fetch_songs(
    proxy: &SubsonicProxyService,
    auth: &IndexMap<String, String>,
    endpoint: &str,
    container: &str,
    extra: &[(&str, String)],
) -> Option<Vec<Node>> {
    let extra: Vec<(&str, &str)> = extra.iter().map(|(key, value)| (*key, value.as_str())).collect();
    let Some(result) = proxy.relay_safe(endpoint, parameters(auth, &extra)).await else {
        debug!("Mix lookup {endpoint} failed: no answer");
        return None;
    };
    if result.body.is_empty() {
        return None;
    }
    let root = match Node::parse(&String::from_utf8_lossy(&result.body)) {
        Ok(root) => root,
        Err(error) => {
            debug!("Mix lookup {endpoint} failed: {error}");
            return None;
        }
    };
    let response = root.get("subsonic-response")?;
    if response.get("status").and_then(Node::as_str) != Some("ok") {
        return None;
    }
    Some(
        match response.get(container).and_then(|songs| songs.get("song")) {
            Some(Node::Array(songs)) => songs.iter().filter(|song| song.is_object()).cloned().collect(),
            _ => Vec::new(),
        },
    )
}

fn marks_key(username: &str) -> String {
    format!("marks|{}", user_key(username))
}

/// A whole number, as a JSON number or a string of one.
fn node_i64(node: &Node) -> Option<i64> {
    match node {
        Node::Number(text) | Node::String(text) => text.parse().ok(),
        _ => None,
    }
}

fn draw_key(username: &str, playlist: &GeneratedPlaylist) -> String {
    format!(
        "{}|{}|{}",
        user_key(username),
        playlist.id,
        dotnet_ticks(playlist.period_start_utc)
    )
}

/// An absolute expiry as the time left until it.
fn until(expires: DateTime<Utc>) -> Duration {
    (expires - Utc::now()).to_std().unwrap_or(Duration::ZERO)
}

/// The caller's own parameters, whatever auth they carry (a token, a password or an API key), as
/// the sync catalog does, minus anything that named the playlist being read. In insertion order
/// with a removed key's slot reused by the next key added, which is the order the query is
/// written in.
pub(crate) fn parameters(auth: &IndexMap<String, String>, extra: &[(&str, &str)]) -> Vec<(String, String)> {
    let mut parameters: DotnetDictionary<String> = DotnetDictionary::new();
    for (key, value) in auth {
        parameters.set(key.clone(), value.clone());
    }
    parameters.remove("id");
    parameters.remove("playlistId");
    for (key, value) in extra {
        parameters.set((*key).to_string(), (*value).to_string());
    }
    parameters.set("f".to_string(), "json".to_string());
    parameters
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

#[cfg(test)]
#[path = "generated_playlist_service_tests.rs"]
mod tests;
