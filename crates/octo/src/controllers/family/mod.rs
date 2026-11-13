//! The Family page's API, `/api/family/*`: JSON in and out, signed in with the `octo_family`
//! cookie (HttpOnly, SameSite=Strict, path `/api/family`). Every write carries
//! `X-Octo-Family: 1`, which a page on another site cannot add without a preflight that is
//! never approved. Members reach their own things; managers (the owner and anyone with Manage
//! family) reach everyone's, and only from home when `Family:ManageFromAway` is off.
//!
//! The dashboard's Family entry is here too, under `/api/admin/family`, behind the dashboard's
//! own sign-in.

// Every refusal here is the answer itself, sent at once.
#![allow(clippy::result_large_err)]

pub mod account;
pub mod address;
pub mod admin_entry;
pub mod handover;
pub mod imports;
pub mod keep_ready;
pub mod listening;
pub mod manage;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod wiring_tests;

use crate::services::family::public_address::{self, LinkBases};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use axum::body::{Body, Bytes};
use axum::extract::{ConnectInfo, DefaultBodyLimit, FromRequestParts};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use octo_core::family::abilities::Role;
use octo_core::family::device::{NetworkPlace, client_origin};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::app::AppState;
use crate::controllers::admin::plumbing::request_cookie;
use crate::http::routes::RouteSet;
use crate::middleware::forwarded::RequestScheme;
use crate::services::admin::SessionKind;
use crate::services::family::family_service::{Actor, FamilyError};
use crate::services::family::member_bridge::SignIn;
use crate::services::family::{FamilyService, Member};
use octo_subsonic::subsonic_request_parser::{RequestParts, RequestReading, read_request};

/// The Family page's sign-in cookie.
pub const COOKIE: &str = "octo_family";
/// The header every write carries.
pub const WRITE_HEADER: &str = "X-Octo-Family";
/// The largest body a sign-in, a join or a password change may carry.
pub const SIGN_IN_BODY: usize = 16 * 1024;

const SIGN_IN_FIRST: &str = "Sign in first.";
const FAMILY_OFF: &str = "Family is off on this server.";
const AWAY_OFF: &str = "Managing the family from away from home is off on this server.";
const NOT_A_MANAGER: &str = "Only people who manage the family can do that.";
const NOT_AN_APPROVER: &str = "Only people who approve requests can do that.";
const MEMBERS_ONLY: &str = "This is for family members.";
const PAUSED: &str = "This account is paused by the family owner.";

/// Every route of the Family page's API and the dashboard's Family entry.
pub fn routes() -> RouteSet {
    use account::*;
    use manage::*;
    RouteSet::new()
        .route(
            "/api/family/auth",
            post(sign_in).layer(DefaultBodyLimit::max(SIGN_IN_BODY)),
        )
        .route("/api/family/session", get(session))
        .route(
            "/api/family/handoff",
            post(address::handoff).layer(DefaultBodyLimit::max(SIGN_IN_BODY)),
        )
        .route("/api/family/public-address", put(address::put_public_address))
        .route(
            "/api/family/public-address/check",
            post(address::check_public_address),
        )
        .route("/family/hello", get(address::hello))
        .route("/api/family/signout", post(sign_out))
        .route(
            "/api/family/join",
            post(join).layer(DefaultBodyLimit::max(SIGN_IN_BODY)),
        )
        .route("/api/family/me", get(me))
        .route("/api/family/me/login", get(my_login))
        .route("/api/family/me/devices", get(my_devices))
        .route(
            "/api/family/me/devices/{id}",
            put(change_my_device).delete(forget_my_device),
        )
        .route("/api/family/me/requests", get(my_requests).post(ask))
        .route("/api/family/me/requests/{id}", delete(cancel_my_request))
        .route("/api/family/me/saved", get(my_saved))
        .route("/api/family/me/saved/{id}", delete(forget_saved))
        .route("/api/family/me/library", get(my_library))
        .route("/api/family/me/library/{id}", delete(remove_from_my_library))
        .route(
            "/api/family/me/password",
            put(change_password).layer(DefaultBodyLimit::max(SIGN_IN_BODY)),
        )
        .route("/api/family/me/quality", put(set_quality))
        .route("/api/family/me/ntfy", put(set_ntfy))
        .route("/api/family/me/lastfm", get(my_last_fm))
        .route("/api/family/me/listening", get(listening::my_listening))
        .route(
            "/api/family/me/keep-ready",
            get(keep_ready::my_keep_ready).put(keep_ready::set_my_keep_ready),
        )
        .route("/api/family/me/imports", get(imports::my_imports))
        .route("/api/family/me/imports/text", post(imports::import_text))
        .route(
            "/api/family/me/imports/file",
            post(imports::import_file).layer(DefaultBodyLimit::max(imports::FILE_BODY)),
        )
        .route("/api/family/cover/{id}", get(cover))
        .route("/api/family/me/lastfm/{action}", post(last_fm_action))
        .route("/api/family/members", get(members).post(add_member))
        .route(
            "/api/family/members/{name}",
            put(edit_member).delete(remove_member),
        )
        .route("/api/family/members/{name}/invite", post(invite_member))
        .route("/api/family/members/{name}/repair", post(repair_member))
        .route("/api/family/members/{name}/reset", post(reset_member))
        .route("/api/family/requests", get(requests))
        .route("/api/family/requests/{id}/approve", post(approve))
        .route("/api/family/requests/{id}/decline", post(decline))
        .route("/api/family/devices", get(devices))
        .route("/api/family/listening", get(listening::family_listening))
        .route("/api/family/devices/{id}", delete(forget_device))
        .route("/api/family/playlists", get(playlists).post(create_playlist))
        .route(
            "/api/family/playlists/{id}",
            put(change_playlist).delete(delete_playlist),
        )
        .route("/api/family/playlist-prefix", post(set_playlist_prefix))
        .route("/api/family/load", get(load))
        .route("/api/family/enable", post(enable))
        .route("/api/family/libraries", get(libraries))
        .route("/api/family/unrated", get(unrated))
        .route("/api/family/unrated/mark", post(mark_unrated))
        .merge(handover::routes())
        .merge(admin_entry::routes())
}

// ------------------------------------------------------------------------------------------
// Answers
// ------------------------------------------------------------------------------------------

/// A JSON answer that no cache keeps.
pub fn answer(status: StatusCode, value: &Value) -> Response {
    let mut res = Response::new(Body::from(value.to_string()));
    *res.status_mut() = status;
    let headers = res.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    res
}

pub fn ok(value: &Value) -> Response {
    answer(StatusCode::OK, value)
}

pub fn no_content() -> Response {
    let mut res = StatusCode::NO_CONTENT.into_response();
    res.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    res
}

pub fn refusal(status: StatusCode, message: &str) -> Response {
    answer(status, &json!({ "error": message }))
}

/// A family error as the page reads it.
pub fn family_error(e: &FamilyError) -> Response {
    let status = match e {
        FamilyError::Invalid(_) => StatusCode::BAD_REQUEST,
        FamilyError::NotFound(_) => StatusCode::NOT_FOUND,
        FamilyError::Refused(_) => StatusCode::FORBIDDEN,
        FamilyError::Conflict(_) => StatusCode::CONFLICT,
        FamilyError::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
        FamilyError::Throttled(..) => StatusCode::TOO_MANY_REQUESTS,
    };
    let mut res = refusal(status, e.message());
    if let FamilyError::Throttled(_, wait) = e
        && let Ok(value) = HeaderValue::from_str(&wait.num_seconds().max(1).to_string())
    {
        res.headers_mut().insert(header::RETRY_AFTER, value);
    }
    res
}

/// The body as `T`, or the 400 to answer with.
#[allow(clippy::result_large_err)] // the error is the answer itself, sent at once
pub fn body_as<T: DeserializeOwned + Default>(body: &Bytes) -> Result<T, Response> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(T::default());
    }
    serde_json::from_slice(body).map_err(|_| {
        refusal(
            StatusCode::BAD_REQUEST,
            "Octo could not read that. Reload the Family page and try again.",
        )
    })
}

// ------------------------------------------------------------------------------------------
// The request: who, from where
// ------------------------------------------------------------------------------------------

/// What every Family page request carries: the writes' header check passed, the client
/// address, home or away, and the page's own address. A request signed like a Subsonic call
/// (an Octo app's) carries its checked sign-in too.
pub struct Web {
    pub headers: HeaderMap,
    pub address: IpAddr,
    /// The address Octo can vouch for, whatever hops the request names.
    pub peer: IpAddr,
    pub place: NetworkPlace,
    /// A reverse proxy on a home network that Octo does not trust, when the request came
    /// through one (everyone then counts as away until it is listed).
    pub untrusted_proxy: Option<IpAddr>,
    pub https: bool,
    /// `scheme://host` as the browser reached Octo.
    pub origin: String,
    /// The caller of a Subsonic-signed request, checked.
    pub signed_call: Option<Signed>,
}

/// A query read once, as the family gate reads one.
fn read_query(query: &str) -> RequestReading {
    read_request(&RequestParts {
        query: Some(query),
        ..Default::default()
    })
}

/// Whether a query read once carries a Subsonic sign-in: `u` with `p`, or with `t` and `s`.
fn carries_sign_in(reading: &RequestReading) -> bool {
    let sign_in = SignIn::of(reading, None);
    !sign_in.users.is_empty()
        && (!sign_in.passwords.is_empty() || (!sign_in.tokens.is_empty() && !sign_in.salts.is_empty()))
}

fn wrong_sign_in() -> Response {
    answer(
        StatusCode::UNAUTHORIZED,
        &json!({ "error": "Wrong username or password", "signIn": true }),
    )
}

/// Checks a Subsonic sign-in on a Family API call: a member's against their devices (as the
/// family gate does on `/rest`), anyone else's with Navidrome, with exactly the sign-in read
/// from the query once. Wrong ones count against the address, the peer and the account.
async fn check_signed_call(
    state: &AppState,
    reading: &RequestReading,
    headers: &HeaderMap,
    address: IpAddr,
    peer: IpAddr,
    place: octo_core::family::device::NetworkPlace,
) -> Result<Signed, Response> {
    use crate::services::family::member_bridge::{self, MAX_FAILURES, Verdict};
    let sign_in = SignIn::of(reading, None);
    let users = sign_in.named_users();
    let [user] = users.as_slice() else {
        return Err(wrong_sign_in());
    };
    let user = user.to_string();
    let throttle = &state.family_bridge.throttle;
    let known = &state.family_bridge.known;
    if let Some(member) = state.family_store.member(&user) {
        let keys = state
            .family_bridge
            .try_keys(&address, &peer, &member.username, MAX_FAILURES);
        if throttle.blocked_by(&keys) {
            return Err(wrong_sign_in());
        }
        let password =
            crate::services::family::passwords::open(&state.family_vault, &member.account_password);
        match member_bridge::verify(password.as_deref(), &sign_in, false) {
            Verdict::Verified => {}
            Verdict::Missing => return Err(wrong_sign_in()),
            Verdict::Refused => {
                throttle.failed_by(&keys);
                return Err(wrong_sign_in());
            }
        };
        known.signed_in(&address, &member.username);
        if member.suspended {
            return Err(refusal(StatusCode::FORBIDDEN, PAUSED));
        }
        // The app's own device, told apart by what it sends, as the gate tells it apart, and
        // recorded now that its sign-in checked out.
        let seen = crate::middleware::family_gate::sighting(headers, &reading.parameters());
        let device_id = crate::services::family::device_registry::detected_device_id(&member.username, &seen);
        let mut caller = crate::services::family::caller::FamilyCaller::member(
            &member.username,
            member.preset,
            member.abilities(),
        );
        caller.device_id = Some(device_id.clone());
        caller.place = place;
        caller.address = address;
        state.family_devices.seen(&caller, &seen);
        return Ok(Signed {
            actor: Actor {
                user: member.username.clone(),
                role: Role::from(member.preset),
            },
            kind: SessionKind::Member,
            token: String::new(),
            device_id: Some(device_id),
        });
    }
    let keys = state.family_bridge.try_keys(&address, &peer, &user, MAX_FAILURES);
    if throttle.blocked_by(&keys) {
        return Err(wrong_sign_in());
    }
    // Navidrome says whether the owner's (or anyone else's) sign-in is right: exactly the one
    // read here, so the account checked is the account named.
    let configured = state
        .settings
        .current()
        .subsonic
        .url
        .as_deref()
        .is_some_and(|url| !url.trim().is_empty());
    if !configured {
        return Err(refusal(
            StatusCode::SERVICE_UNAVAILABLE,
            "Navidrome's address is not set yet.",
        ));
    }
    let parameters = reading.parameters();
    let Some(credential) = crate::middleware::family_gate::credential_of(reading, &parameters, Some(&user))
    else {
        throttle.failed_by(&keys);
        return Err(wrong_sign_in());
    };
    match state
        .credential_check
        .check(Some(&credential), &state.subsonic_proxy)
        .await
    {
        Ok(crate::services::subsonic::CredentialVerdict::Accepted) => known.signed_in(&address, &user),
        Ok(crate::services::subsonic::CredentialVerdict::Refused) => {
            throttle.failed_by(&keys);
            return Err(wrong_sign_in());
        }
        _ => {
            return Err(refusal(
                StatusCode::BAD_GATEWAY,
                "Octo could not reach Navidrome to check who is asking.",
            ));
        }
    }
    let owner = state.family_store.is_owner(&user);
    Ok(Signed {
        actor: Actor {
            user,
            role: if owner { Role::Owner } else { Role::Unmanaged },
        },
        kind: if owner {
            SessionKind::FamilyNavidrome
        } else {
            SessionKind::FamilyGuest
        },
        token: String::new(),
        device_id: None,
    })
}

impl FromRequestParts<AppState> for Web {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Response> {
        let write = !matches!(parts.method, Method::GET | Method::HEAD | Method::OPTIONS);
        let marked = parts
            .headers
            .get(WRITE_HEADER)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.trim() == "1");
        // A Subsonic-signed call cannot be made by another website, so it needs no header. The
        // query is read once, for this and for checking the sign-in.
        let reading = parts.uri.query().map(read_query);
        let signed = reading.as_ref().is_some_and(carries_sign_in);
        if write && !marked && !signed {
            return Err(refusal(
                StatusCode::FORBIDDEN,
                "Family changes must come from the Family page.",
            ));
        }
        let settings = state.settings.current();
        let peer = parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|info| info.0.ip())
            .unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        let forwarded: Vec<String> = parts
            .headers
            .get_all("x-forwarded-for")
            .iter()
            .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
            .collect();
        let forwarded = (!forwarded.is_empty()).then(|| forwarded.join(","));
        let socket_peer = peer;
        let origin = client_origin(
            peer,
            forwarded.as_deref(),
            &settings.family.trusted_proxy_list(),
            &settings.family.home_network_list(),
        );
        let (address, place, peer) = (origin.address, origin.place, origin.peer);
        let https = parts
            .extensions
            .get::<RequestScheme>()
            .is_some_and(|s| s.0.eq_ignore_ascii_case("https"));
        let host = parts
            .headers
            .get(header::HOST)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|h| !h.is_empty())
            .unwrap_or("localhost")
            .to_string();
        let request_origin = format!("{}://{host}", if https { "https" } else { "http" });
        state.family.note_origin(
            &request_origin,
            public_address::forwarded_origin(
                socket_peer,
                &parts.headers,
                https,
                &settings.family.trusted_proxy_list(),
            ),
        );
        let signed_call = match (signed, reading.as_ref()) {
            (true, Some(reading)) => {
                Some(check_signed_call(state, reading, &parts.headers, address, peer, place).await?)
            }
            _ => None,
        };
        Ok(Web {
            headers: parts.headers.clone(),
            address,
            peer,
            place,
            untrusted_proxy: origin.untrusted_proxy,
            https,
            origin: request_origin,
            signed_call,
        })
    }
}

/// Someone signed in on the Family page, or an app's signed call.
#[derive(Debug, Clone)]
pub struct Signed {
    pub actor: Actor,
    pub kind: SessionKind,
    /// The session's token; empty for a signed call.
    pub token: String,
    /// The member's device that signed the call, for a signed call.
    pub device_id: Option<String>,
}

impl Signed {
    pub fn user(&self) -> &str {
        &self.actor.user
    }
}

impl Web {
    /// The session cookie's token.
    pub fn token(&self) -> Option<String> {
        request_cookie(&self.headers, COOKIE).filter(|t| !t.trim().is_empty())
    }

    /// The key the sign-in throttles count by.
    pub fn throttle_key(&self) -> String {
        self.address.to_string()
    }

    /// The address members' apps and links use.
    pub fn base(&self, state: &AppState) -> String {
        FamilyService::public_base(state, &self.origin)
    }

    /// The bases for links made for `user`.
    pub fn link_bases(&self, state: &AppState, user: &str) -> LinkBases {
        LinkBases::new(state, &self.origin, public_address::away_allowed(state, user))
    }

    /// The cookie that keeps `token` for the Family page's API.
    pub fn cookie(&self, token: &str) -> HeaderValue {
        let secure = if self.https { "; secure" } else { "" };
        let age = crate::services::admin::BrowseSessionStore::FAMILY_TTL.num_seconds();
        HeaderValue::from_str(&format!(
            "{COOKIE}={}; max-age={age}; path=/api/family{secure}; samesite=strict; httponly",
            octo_core::common::dotnet::escape_data_string(token)
        ))
        .unwrap_or_else(|_| HeaderValue::from_static("octo_family="))
    }

    /// The cookie that removes the sign-in.
    pub fn cookie_gone() -> HeaderValue {
        HeaderValue::from_static(
            "octo_family=; expires=Thu, 01 Jan 1970 00:00:00 GMT; path=/api/family; samesite=strict; httponly",
        )
    }

    /// Who is signed in (by the call's own sign-in or the page's cookie), or the 401 to answer
    /// with.
    pub async fn signed(&self, state: &AppState) -> Result<Signed, Response> {
        if let Some(signed) = &self.signed_call {
            return Ok(signed.clone());
        }
        let token = self.token();
        let Some((user, kind)) = state.browse_sessions.family_session_of(token.as_deref()) else {
            return Err(answer(
                StatusCode::UNAUTHORIZED,
                &json!({ "error": SIGN_IN_FIRST, "signIn": true }),
            ));
        };
        let role = match kind {
            SessionKind::Member => match state.family_store.member(&user) {
                Some(member) if member.suspended => {
                    state.browse_sessions.revoke(token.as_deref());
                    return Err(refusal(StatusCode::FORBIDDEN, PAUSED));
                }
                Some(member) => Role::from(member.preset),
                None => {
                    state.browse_sessions.revoke(token.as_deref());
                    return Err(answer(
                        StatusCode::UNAUTHORIZED,
                        &json!({ "error": SIGN_IN_FIRST, "signIn": true }),
                    ));
                }
            },
            SessionKind::FamilyGuest | SessionKind::Admin => {
                if state.family_store.member(&user).is_some() {
                    state.browse_sessions.revoke(token.as_deref());
                    return Err(answer(
                        StatusCode::UNAUTHORIZED,
                        &json!({ "error": SIGN_IN_FIRST, "signIn": true }),
                    ));
                }
                Role::Unmanaged
            }
            SessionKind::FamilyNavidrome => {
                let owner = state.family_store.owner();
                let may_own = owner.is_empty() || octo_core::common::dotnet::eq_ignore_case(&owner, &user);
                if state.family_store.member(&user).is_some() {
                    // A Navidrome sign-in made before the account joined the family.
                    state.browse_sessions.revoke(token.as_deref());
                    return Err(answer(
                        StatusCode::UNAUTHORIZED,
                        &json!({ "error": SIGN_IN_FIRST, "signIn": true }),
                    ));
                }
                if may_own && state.admin_role_check.still_admin(&user).await {
                    Role::Owner
                } else {
                    Role::Unmanaged
                }
            }
        };
        Ok(Signed {
            actor: Actor { user, role },
            kind,
            token: token.unwrap_or_default(),
            device_id: None,
        })
    }

    /// A signed-in member while Family is on, with their record.
    pub async fn member(&self, state: &AppState) -> Result<(Signed, Member), Response> {
        require_on(state)?;
        let signed = self.signed(state).await?;
        match state.family_store.member(signed.user()) {
            Some(member) => Ok((signed, member)),
            None => Err(refusal(StatusCode::FORBIDDEN, MEMBERS_ONLY)),
        }
    }

    /// Anyone signed in while Family is on.
    pub async fn anyone(&self, state: &AppState) -> Result<Signed, Response> {
        require_on(state)?;
        self.signed(state).await
    }

    fn check_place(&self, state: &AppState) -> Result<(), Response> {
        if self.place == NetworkPlace::Away && !state.settings.current().family.manage_from_away {
            return Err(refusal(StatusCode::FORBIDDEN, AWAY_OFF));
        }
        Ok(())
    }

    /// Someone who manages the family, while Family is on.
    pub async fn manager(&self, state: &AppState) -> Result<Signed, Response> {
        let signed = self.anyone(state).await?;
        if !FamilyService::may_manage(state, &signed.actor) {
            return Err(refusal(StatusCode::FORBIDDEN, NOT_A_MANAGER));
        }
        self.check_place(state)?;
        Ok(signed)
    }

    /// Someone who approves requests, while Family is on.
    pub async fn approver(&self, state: &AppState) -> Result<Signed, Response> {
        let signed = self.anyone(state).await?;
        if !FamilyService::may_approve(state, &signed.actor) {
            return Err(refusal(StatusCode::FORBIDDEN, NOT_AN_APPROVER));
        }
        self.check_place(state)?;
        Ok(signed)
    }

    /// The owner (a Navidrome admin), whether or not Family is on.
    pub async fn owner(&self, state: &AppState) -> Result<Signed, Response> {
        let signed = self.signed(state).await?;
        if !signed.actor.is_owner() {
            return Err(refusal(
                StatusCode::FORBIDDEN,
                "Only the family owner, signed in with their Navidrome admin account, can do that.",
            ));
        }
        self.check_place(state)?;
        Ok(signed)
    }
}

/// The 404 every family route answers while Family is off.
#[allow(clippy::result_large_err)] // the error is the answer itself, sent at once
pub fn require_on(state: &AppState) -> Result<(), Response> {
    if state.settings.current().family.enabled {
        Ok(())
    } else {
        Err(refusal(StatusCode::NOT_FOUND, FAMILY_OFF))
    }
}
