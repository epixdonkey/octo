//! `getInternetRadioStations` and the Continuous Radio stream behind the station URLs it
//! publishes, `radio/stream/{token}`.

use std::pin::Pin;
use std::task::{Context, Poll};

use axum::body::Body;
use axum::extract::{OriginalUri, Path, Request, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures::{Stream, StreamExt};
use octo_core::models::radio::LastFmRadioStation;
use octo_subsonic::SubsonicReply;
use serde_json::{Value, json};
use tokio::sync::oneshot;
use tokio_util::io::ReaderStream;
use tokio_util::sync::{CancellationToken, DropGuard};
use tracing::{info, warn};

use super::action_helpers::{
    RequestAborted, SubsonicCall, bootstrap_radio_profile, is_successful_subsonic_response, pairs,
    queue_refresh_if_stale, stream_stations,
};
use super::caller_checks::listener_name_of;
use super::playlists::{merge_json, merge_xml};
use crate::app::AppState;
use crate::services::last_fm::LastFmRadioStreamSession;
use crate::services::last_fm::icy_metadata_stream::DEFAULT_INTERVAL;
use octo_core::last_fm::last_fm_radio_stream_service::READY_POOL_SIZE;

pub async fn get_internet_radio_stations(State(state): State<AppState>, req: Request) -> Response {
    let aborted = RequestAborted::new();
    let response = stations_inner(&state, req, &aborted).await;
    aborted.answered();
    response
}

async fn stations_inner(state: &AppState, req: Request, aborted: &RequestAborted) -> Response {
    let call = match SubsonicCall::read(state, req).await {
        Ok(call) => call,
        Err(response) => return response,
    };
    let format = call.format.clone();
    let relay = call
        .proxy
        .relay_safe("rest/getInternetRadioStations", &call.parameters)
        .await;
    let relay = match relay {
        Some(relay) if !relay.body.is_empty() && is_successful_subsonic_response(&relay.body, &format) => {
            relay
        }
        Some(relay) if !relay.body.is_empty() => {
            return call.file(&relay.body, relay.content_type.as_deref());
        }
        _ => {
            return state
                .subsonic_response_builder
                .create_error(&format, 0, "Unable to authenticate with Navidrome")
                .into_response();
        }
    };

    let username = listener_name_of(state, &call.parameters, &call.proxy, &call.parts.headers).await;
    // A Navidrome row whose streamUrl is a YouTube URL is served by this server's own live
    // endpoint, so the list publishes that URL instead. Every other row (a plain radio URL,
    // an Octo station's token URL further down) keeps exactly what it came with.
    let relay = super::live_radio::rewrite_live_station_urls(relay, &call);
    bootstrap_radio_profile(state, &call.proxy, &username, &call.parameters).await;
    let stations = stream_stations(state, &username);
    info!(
        "Continuous Radio discovery requested for {username}: {} eligible stations",
        stations.len()
    );
    queue_refresh_if_stale(state, &username);
    if stations.is_empty() {
        return call.file(&relay.body, relay.content_type.as_deref());
    }

    let streams = state.last_fm_radio_streams.scoped(call.proxy.clone());
    let sessions = &state.last_fm_radio_stream_sessions;
    let mut cold_sessions: Vec<LastFmRadioStreamSession> = Vec::new();

    // A station whose ready pool is already cached is published straight away.
    let mut prepared: Vec<(LastFmRadioStation, String)> = Vec::new();
    for station in &stations {
        let token = sessions.issue(&username, &station.id, pairs(&call.parameters), None);
        let Some(session) = sessions.get(&token, None) else {
            continue;
        };
        let ready_pool = streams.get_ready_pool(&session);
        if ready_pool.is_empty() {
            // Station-list requests are latency-sensitive and some clients cancel
            // them after only a few seconds. Warm every cache miss independently,
            // but never make an already-ready station wait for slower siblings.
            cold_sessions.push(session);
            sessions.remove(&token);
            continue;
        }
        if !sessions.attach_ready_pool(&token, &ready_pool, None) {
            sessions.remove(&token);
            continue;
        }
        if ready_pool.len() < READY_POOL_SIZE
            && let Some(attached) = sessions.get(&token, None)
        {
            streams.warm_ready_pool(&attached);
        }
        prepared.push((station.clone(), token));
    }

    // A completely cold install still publishes one usable starter in the
    // same response. The remaining stations are already warming above and
    // will appear on the client's next ordinary refresh.
    if prepared.is_empty() {
        let first = &stations[0];
        match prepare_starter(state, &streams, &username, first, &call, aborted).await {
            Ok(Some(starter)) => prepared.push(starter),
            Ok(None) => {}
            Err(error) => {
                warn!("Could not merge Octo stations into getInternetRadioStations: {error:#}");
                return call.file(&relay.body, relay.content_type.as_deref());
            }
        }
        // PrepareStarter owns this station's cold-path production and starts
        // its runway warm only after the publication pool is attached. Do not
        // race it with the cache-miss warmer discovered above.
        cold_sessions.retain(|session| session.station_id != first.id);
    }
    for cold_session in &cold_sessions {
        streams.warm_ready_pool(cold_session);
    }
    info!(
        "Continuous Radio discovery published {}/{} ready stations for {username}",
        prepared.len(),
        stations.len()
    );
    // PathBase is always empty here: Octo is mounted at the root.
    let stream_url = |token: &str| format!("{}://{}/radio/stream/{token}", call.scheme(), call.host());
    let merged = if format.eq_ignore_ascii_case("json") {
        let rows = prepared.iter().map(|(station, token)| -> Value {
            json!({
                "id": station.id,
                "name": station.name,
                "streamUrl": stream_url(token),
                "coverArt": station.id,
            })
        });
        merge_json(&relay.body, "internetRadioStations", "internetRadioStation", rows)
            .map(|body| SubsonicReply::file(body, "application/json"))
    } else {
        let rows = prepared.iter().map(|(station, token)| {
            vec![
                ("id".to_string(), station.id.clone()),
                ("name".to_string(), station.name.clone()),
                ("streamUrl".to_string(), stream_url(token)),
                ("coverArt".to_string(), station.id.clone()),
            ]
        });
        merge_xml(&relay.body, "internetRadioStations", "internetRadioStation", rows)
            .map(|body| SubsonicReply::file(body, "application/xml"))
    };
    match merged {
        Some(reply) => reply.into_response(),
        None => {
            warn!("Could not merge Octo stations into getInternetRadioStations");
            call.file(&relay.body, relay.content_type.as_deref())
        }
    }
}

/// The one station a cold install publishes in the same answer: its first track prepared,
/// waited for at most `EffectiveStarterPublishTimeout`.
async fn prepare_starter(
    state: &AppState,
    streams: &crate::services::last_fm::LastFmRadioStreamService,
    username: &str,
    station: &LastFmRadioStation,
    call: &SubsonicCall,
    aborted: &RequestAborted,
) -> anyhow::Result<Option<(LastFmRadioStation, String)>> {
    let sessions = &state.last_fm_radio_stream_sessions;
    let token = sessions.issue(username, &station.id, pairs(&call.parameters), None);
    let Some(session) = sessions.get(&token, None) else {
        return Ok(None);
    };
    // The preparation runs on its own: a bound that runs out stops the waiting, not the work.
    let preparing = {
        let streams = streams.clone();
        let session = session.clone();
        let cancel = aborted.token.clone();
        tokio::spawn(async move { streams.prepare_for_publication(&session, &cancel).await })
    };

    // A cold starter is a YouTube fetch plus a transcode, tens of seconds on a
    // small box, and many clients give up on a list request well before that.
    // Answer inside the bound instead. The cache produces the track under its
    // own single-flight regardless of who is still waiting, so the station is
    // simply on the next refresh; the warmer below keeps its runway filling.
    let bound = state
        .settings
        .current()
        .last_fm
        .effective_starter_publish_timeout();
    let ready_pool = match bound {
        Some(limit) => {
            let mut preparing = preparing;
            match tokio::time::timeout(limit, &mut preparing).await {
                Ok(joined) => joined,
                Err(_) => {
                    sessions.remove(&token);
                    streams.warm_ready_pool(&session);
                    info!(
                        "Continuous Radio starter for {} not ready within {}s; publishing on the next refresh",
                        station.name,
                        limit.as_secs()
                    );
                    return Ok(None);
                }
            }
        }
        None => preparing.await,
    };
    let ready_pool = ready_pool.map_err(|error| anyhow::anyhow!("the preparation failed: {error}"))??;
    if ready_pool.is_empty() {
        sessions.remove(&token);
        return Ok(None);
    }
    if !sessions.attach_ready_pool(&token, &ready_pool, None) {
        sessions.remove(&token);
        return Ok(None);
    }
    if ready_pool.len() < READY_POOL_SIZE
        && let Some(attached) = sessions.get(&token, None)
    {
        streams.warm_ready_pool(&attached);
    }
    Ok(Some((station.clone(), token)))
}

/// `radio/stream/{token:length(48)}`, GET and HEAD. A token of any other length is not this
/// route: it falls through to the catch-all, which relays it.
pub async fn stream_generated_radio(
    State(state): State<AppState>,
    Path(token): Path<String>,
    req: Request,
) -> Response {
    if token.encode_utf16().count() != 48 {
        return crate::http::catch_all::catch_all(State(state), as_sent(req)).await;
    }
    let session = state.last_fm_radio_stream_sessions.get(&token, None);
    let streams = state.last_fm_radio_streams.clone();
    let station = session.as_ref().and_then(|session| streams.resolve(session));
    let (Some(session), Some(station)) = (session, station) else {
        return empty(StatusCode::NOT_FOUND);
    };

    let settings = state.settings.current();
    let last_fm = &settings.last_fm;
    let icy_asked = req
        .headers()
        .get_all("Icy-MetaData")
        .iter()
        .map(|value| String::from_utf8_lossy(value.as_bytes()).into_owned())
        .collect::<Vec<_>>()
        .join(",");
    let include_icy_metadata = last_fm.enable_icy_metadata && icy_asked.trim() == "1";
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("audio/mpeg"));
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store, no-transform"),
    );
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("none"));
    // A station name that cannot be sent as a header value answers 503.
    let Ok(name) = HeaderValue::from_str(&station.name) else {
        warn!("Continuous Radio stream failed for station {}", station.id);
        return empty(StatusCode::SERVICE_UNAVAILABLE);
    };
    headers.insert("icy-name", name);
    headers.insert(
        "icy-br",
        HeaderValue::from(last_fm.effective_radio_stream_bitrate_kbps()),
    );
    if include_icy_metadata {
        headers.insert("icy-metaint", HeaderValue::from(DEFAULT_INTERVAL));
    }
    if req.method() == Method::HEAD {
        let mut response = Response::new(Body::empty());
        *response.headers_mut() = headers;
        return response;
    }
    info!(
        "Continuous Radio stream opened for {} by {}",
        station.name, session.username
    );

    // The stream writes into a pipe the response body reads. The listener hanging up drops the
    // body, which cancels the stream; a failure once bytes have gone out aborts the connection.
    let (writer, reader) = tokio::io::duplex(64 * 1024);
    let cancel = CancellationToken::new();
    let (done_tx, done_rx) = oneshot::channel::<Option<String>>();
    let streaming = cancel.clone();
    let station_id = station.id.clone();
    let streams = streams.scoped(state.subsonic_proxy.clone());
    tokio::spawn(async move {
        let mut writer = writer;
        let result = streams
            .stream(&session, &mut writer, &streaming, include_icy_metadata)
            .await;
        let failure = match result {
            Ok(()) => None,
            // A radio stream normally ends because the listener stopped playback.
            Err(_) if streaming.is_cancelled() => None,
            Err(error) => {
                warn!("Continuous Radio stream failed for station {station_id}: {error:#}");
                Some(error.to_string())
            }
        };
        drop(writer);
        let _ = done_tx.send(failure);
    });
    let body = RadioBody {
        audio: ReaderStream::new(reader).boxed(),
        done: Some(done_rx),
        _cancel_on_drop: cancel.drop_guard(),
    };
    let mut response = Response::new(Body::from_stream(body));
    *response.headers_mut() = headers;
    response
}

/// The request with the path the client sent, for the catch-all (which relays it as spelled).
fn as_sent(mut req: Request) -> Request {
    if let Some(OriginalUri(uri)) = req.extensions().get::<OriginalUri>().cloned() {
        *req.uri_mut() = uri;
    }
    req
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

/// The radio's audio, then the stream's outcome: an error ends the body with an error, which
/// aborts the connection.
struct RadioBody {
    audio: futures::stream::BoxStream<'static, std::io::Result<Bytes>>,
    done: Option<oneshot::Receiver<Option<String>>>,
    _cancel_on_drop: DropGuard,
}

impl Stream for RadioBody {
    type Item = std::io::Result<Bytes>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if let Poll::Ready(item) = self.audio.poll_next_unpin(cx) {
            if let Some(item) = item {
                return Poll::Ready(Some(item));
            }
            let Some(done) = self.done.as_mut() else {
                return Poll::Ready(None);
            };
            return match Pin::new(done).poll(cx) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(outcome) => {
                    self.done = None;
                    match outcome {
                        Ok(Some(failure)) => Poll::Ready(Some(Err(std::io::Error::other(failure)))),
                        _ => Poll::Ready(None),
                    }
                }
            };
        }
        Poll::Pending
    }
}

use std::future::Future;
