//! The live radio stations: a Navidrome station whose `streamUrl` is a YouTube URL is
//! published under Octo's own `/radio/live/{id}` endpoint, which serves an endless MP3 from
//! the shim's `/live/stream`. The list rewrite (`getInternetRadioStations`) and the serving
//! endpoint share this module; the Last.fm machine's own stations and tokens are not touched.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, LazyLock};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;
use bytes::Bytes;
use futures::{Stream, StreamExt};
use octo_core::json::dom::Node;
use octo_subsonic::xml::XElement;
use parking_lot::Mutex;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tracing::{info, warn};

use super::action_helpers::{SubsonicCall, is_successful_subsonic_response};
use super::personal_playlists::auth_of;
use crate::app::AppState;
use crate::services::subsonic::RelayResponse;

/// How many live radio streams play at once. Deliberately its own count, apart from the
/// Last.fm machine's cap: a live pipe holds one shim ffmpeg open for hours, and a shared
/// cap would let four live channels starve the family radio's own streams.
pub(crate) const LIVE_STREAM_CAP: usize = 4;

/// The live streams' own slots, one set per application state.
pub(crate) fn live_stream_slots() -> Arc<Semaphore> {
    Arc::new(Semaphore::new(LIVE_STREAM_CAP))
}

/// How long one relayed station row is reused, so a stream does not re-ask Navidrome per play.
const STATION_CACHE_TTL: Duration = Duration::from_secs(60);

/// The resolved rows, per station id: `None` is a row that is not there (or not a YouTube
/// URL), cached exactly like a found one so an unknown id does not hammer the relay.
static STATION_CACHE: LazyLock<Mutex<HashMap<String, (Instant, Option<LiveStation>)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// A Navidrome station this server can serve live: its name and the video its streamUrl names.
#[derive(Clone, PartialEq, Eq, Debug)]
struct LiveStation {
    name: String,
    video_id: String,
}

/// A YouTube video id: 11 characters of the URL-safe base64 alphabet YouTube ids are made of.
fn is_video_id(id: &str) -> bool {
    id.len() == 11
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Whether `host` is a YouTube page host: `youtu.be` or any `…youtube.com` subdomain
/// (`www.`, `m.`, `music.`…). A host that merely ends in the word (`evilyoutube.com`) is not.
fn is_you_tube_host(host: &str) -> bool {
    host == "youtu.be"
        || host
            .strip_suffix("youtube.com")
            .is_some_and(|prefix| prefix.is_empty() || prefix.ends_with('.'))
}

/// The video id a YouTube streamUrl names, or None when it is not one. The forms a radio
/// station can be saved with: `youtube.com/watch?v=`, `youtu.be/<id>`, `/live/<id>` and
/// `/embed/<id>` (any scheme, any `…youtube.com` subdomain). `shorts/` is deliberately not
/// one of them: a short is a finite video, not a channel, and the live endpoint would serve
/// it one buffer's worth and go quiet — those URLs stay untouched, for the client to play as
/// it always has.
pub(crate) fn you_tube_video_id(stream_url: &str) -> Option<String> {
    let url = stream_url.trim();
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    let (authority, tail) = rest.split_once('/')?;
    if !is_you_tube_host(&authority.to_ascii_lowercase()) {
        return None;
    }
    let (path, query) = tail.split_once('?').unwrap_or((tail, ""));
    let path = path.trim_end_matches('/');
    let id = if authority.eq_ignore_ascii_case("youtu.be") {
        Some(path)
    } else if path == "watch" {
        query.split('&').find_map(|pair| pair.strip_prefix("v="))
    } else {
        path.strip_prefix("live/").or_else(|| path.strip_prefix("embed/"))
    }?;
    is_video_id(id).then(|| id.to_string())
}

/// A relayed `getInternetRadioStations` answer with every YouTube `streamUrl` rewritten to
/// this server's own live endpoint. The row keeps its id and name; a plain streamUrl (any
/// other radio URL, an Octo station's `/radio/stream/{token}`) passes through as it was. A
/// body that does not parse passes through unchanged too: the rewrite is an enhancement,
/// never a new failure mode for the list.
pub(super) fn rewrite_live_station_urls(relay: RelayResponse, call: &SubsonicCall) -> RelayResponse {
    let live_url = |id: &str| format!("{}://{}/radio/live/{id}", call.scheme(), call.host());
    let rewritten = if call.format.eq_ignore_ascii_case("json") {
        rewrite_json(&relay.body, &live_url)
    } else {
        rewrite_xml(&relay.body, &live_url)
    };
    match rewritten {
        Some(body) => RelayResponse {
            body: Bytes::from(body),
            content_type: relay.content_type,
        },
        None => relay,
    }
}

/// The JSON rewrite: every `internetRadioStations.internetRadioStation` row whose
/// `streamUrl` is a YouTube URL gets the live endpoint's. None when the body is not the
/// object it should be.
fn rewrite_json(body: &[u8], live_url: &dyn Fn(&str) -> String) -> Option<Vec<u8>> {
    let text = std::str::from_utf8(body).ok()?;
    let mut root = Node::parse(text).ok()?;
    let rows = root
        .as_object_mut()?
        .get_mut("subsonic-response")?
        .as_object_mut()?
        .get_mut("internetRadioStations")?
        .as_object_mut()?
        .get_mut("internetRadioStation")?;
    let Node::Array(rows) = rows else {
        return None;
    };
    for row in rows {
        rewrite_one(row, live_url);
    }
    Some(root.to_json_string(false).into_bytes())
}

fn rewrite_one(row: &mut Node, live_url: &dyn Fn(&str) -> String) {
    let Some(object) = row.as_object_mut() else {
        return;
    };
    let id = object.get("id").and_then(Node::as_str);
    let stream_url = object.get("streamUrl").and_then(Node::as_str);
    if let (Some(id), Some(stream_url)) = (id, stream_url)
        && you_tube_video_id(stream_url).is_some()
    {
        object.insert("streamUrl".into(), Node::String(live_url(id)));
    }
}

/// The XML rewrite, the JSON one's twin over `XElement` attributes.
fn rewrite_xml(body: &[u8], live_url: &dyn Fn(&str) -> String) -> Option<Vec<u8>> {
    let mut root = XElement::parse(&String::from_utf8_lossy(body)).ok()?;
    let holder = root
        .elements_mut()
        .find(|element| element.name == "internetRadioStations")?;
    for element in holder.elements_mut() {
        if element.name != "internetRadioStation" {
            continue;
        }
        if let (Some(id), Some(stream_url)) = (element.attribute("id"), element.attribute("streamUrl"))
            && you_tube_video_id(stream_url).is_some()
        {
            element.set_attr("streamUrl", live_url(id));
        }
    }
    Some(root.to_xml_string().into_bytes())
}

/// `GET/HEAD /radio/live/{station-id}`: the station row from Navidrome (relayed as this
/// server's own service identity when the caller named nobody — `auth_of`'s stand-in),
/// answered as the radio machine answers: `audio/mpeg`, no Content-Length, ICY name and
/// bitrate, bytes forwarded from the shim. A listener hanging up drops the body, which
/// drops the shim response — and that close is the shim's signal to kill its ffmpeg.
pub async fn live_station_stream(
    State(state): State<AppState>,
    Path(station_id): Path<String>,
    req: Request,
) -> Response {
    let call = match SubsonicCall::read(&state, req).await {
        Ok(call) => call,
        Err(response) => return response,
    };
    let Some(station) = live_station(&state, &call, &station_id).await else {
        return empty(StatusCode::NOT_FOUND);
    };

    let bitrate = state
        .settings
        .current()
        .last_fm
        .effective_radio_stream_bitrate_kbps();
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("audio/mpeg"));
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store, no-transform"),
    );
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("none"));
    // A station name that cannot be sent as a header value answers 503 (the machine's rule).
    let Ok(name) = HeaderValue::from_str(&station.name) else {
        warn!("Live radio stream failed for station {station_id}");
        return empty(StatusCode::SERVICE_UNAVAILABLE);
    };
    headers.insert("icy-name", name);
    headers.insert("icy-br", HeaderValue::from(bitrate));
    if call.parts.method == axum::http::Method::HEAD {
        let mut response = Response::new(Body::empty());
        *response.headers_mut() = headers;
        return response;
    }

    let Ok(slot) = state.live_radio_slots.clone().try_acquire_owned() else {
        info!("Live radio stream refused for {station_id}: all {LIVE_STREAM_CAP} slots play");
        return short(StatusCode::SERVICE_UNAVAILABLE, "Too many live radio streams");
    };
    info!("Live radio stream opened for {} ({station_id})", station.name);
    match state
        .you_tube_resolver
        .open_live_stream(&station.video_id, bitrate, Some(&station.name))
        .await
    {
        Ok(upstream) => {
            let body = LiveBody {
                audio: upstream.bytes_stream().boxed(),
                _slot: slot,
            };
            let mut response = Response::new(Body::from_stream(body));
            *response.headers_mut() = headers;
            response
        }
        Err(refused) => {
            warn!("Live radio source refused for {station_id}: shim status {refused}");
            short(
                StatusCode::BAD_GATEWAY,
                "The live radio source could not be opened",
            )
        }
    }
}

/// The station row behind one id: the cached answer, else one relay of
/// `rest/getInternetRadioStations` (JSON, as this server's own identity when the caller
/// named nobody). Only a definite answer is cached — a relay that failed or an envelope
/// that says the sign-in failed stays uncached, so the next attempt asks again.
async fn live_station(state: &AppState, call: &SubsonicCall, station_id: &str) -> Option<LiveStation> {
    if let Some((at, cached)) = STATION_CACHE.lock().get(station_id)
        && at.elapsed() < STATION_CACHE_TTL
    {
        return cached.clone();
    }
    let mut auth = auth_of(state, &call.parameters);
    auth.push(("f".to_string(), "json".to_string()));
    let relay = state
        .subsonic_proxy
        .relay_safe("rest/getInternetRadioStations", auth)
        .await?;
    if !is_successful_subsonic_response(&relay.body, "json") {
        return None;
    }
    let found = station_rows(&relay.body)
        .into_iter()
        .find(|(id, _)| id == station_id)
        .map(|(_, station)| station);
    STATION_CACHE
        .lock()
        .insert(station_id.to_string(), (Instant::now(), found.clone()));
    found
}

/// The station rows of a relayed JSON answer: id, name and the video a YouTube streamUrl
/// names (a row without one is skipped — nothing to serve live).
fn station_rows(body: &[u8]) -> Vec<(String, LiveStation)> {
    let Ok(text) = std::str::from_utf8(body) else {
        return Vec::new();
    };
    let Ok(root) = Node::parse(text) else {
        return Vec::new();
    };
    let Some(rows) = root
        .as_object()
        .and_then(|root| root.get("subsonic-response"))
        .and_then(|response| response.get("internetRadioStations"))
        .and_then(|stations| stations.get("internetRadioStation"))
    else {
        return Vec::new();
    };
    let Node::Array(rows) = rows else {
        return Vec::new();
    };
    rows.iter()
        .filter_map(|row| {
            let video_id = you_tube_video_id(row.get("streamUrl")?.as_str()?)?;
            Some((
                row.get("id")?.as_str()?.to_string(),
                LiveStation {
                    name: row.get("name")?.as_str()?.to_string(),
                    video_id,
                },
            ))
        })
        .collect()
}

/// A status with a short plain body.
fn short(status: StatusCode, message: &str) -> Response {
    let mut response = Response::new(Body::from(message.to_string()));
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));
    response
}

/// A status with nothing else.
fn empty(status: StatusCode) -> Response {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(header::CONTENT_LENGTH, HeaderValue::from_static("0"));
    response
}

/// The shim's bytes as a response body, holding this stream's slot: dropping the body drops
/// the shim response (closing its connection — the shim's kill signal) and frees the slot.
struct LiveBody {
    audio: futures::stream::BoxStream<'static, Result<Bytes, reqwest::Error>>,
    _slot: OwnedSemaphorePermit,
}

impl Stream for LiveBody {
    type Item = std::io::Result<Bytes>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.audio.poll_next_unpin(cx).map_err(std::io::Error::other)
    }
}

#[cfg(test)]
#[path = "live_radio_tests.rs"]
mod tests;
