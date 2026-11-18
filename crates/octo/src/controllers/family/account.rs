//! Signing in, joining, and everything a person does for themselves on the Family page: their
//! plan, their login, the devices Octo saw them use, requests, saved songs, own library,
//! password, audio quality, push topic and Last.fm.

use axum::body::Bytes;
use axum::extract::{Path, RawQuery, State};
use axum::http::{StatusCode, header};
use axum::response::Response;
use octo_core::common::dotnet::{eq_ignore_case, is_blank};
use octo_core::family::abilities::{RequestQuality, Role};
use octo_core::family::quality::{DeviceQualityMode, QualityPreference, StreamQuality};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::{info, warn};

use super::{Web, body_as, family_error, no_content, ok, refusal, require_on};
use crate::app::AppState;
use crate::services::admin::{SessionKind, SignInThrottle};
use crate::services::family::FamilyService;
use crate::services::family::family_service::{NewRequest, quality_json};
use crate::services::last_fm::LastFmConnectError;
use crate::services::notifications::notification_service::is_member_topic;

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct SignInBody {
    pub username: String,
    pub password: String,
}

/// What Navidrome said of a sign-in.
enum NavidromeSignIn {
    Admin(Bytes),
    NotAdmin,
    Wrong,
    Limited,
    Unreachable(String),
}

async fn navidrome_sign_in(state: &AppState, username: &str, password: &str) -> NavidromeSignIn {
    let url = state
        .settings
        .current()
        .subsonic
        .url
        .as_deref()
        .unwrap_or("")
        .trim_end_matches('/')
        .to_string();
    if url.is_empty() {
        return NavidromeSignIn::Unreachable("Navidrome's address is not set yet.".into());
    }
    let payload = json!({ "username": username, "password": password }).to_string();
    let reply = state
        .http
        .post(format!("{url}/auth/login"))
        .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
        .body(payload)
        .send()
        .await;
    match reply {
        Err(e) => {
            NavidromeSignIn::Unreachable(crate::services::http_client_factory::connect_failure_message(&e))
        }
        Ok(r) if r.status() == reqwest::StatusCode::TOO_MANY_REQUESTS => NavidromeSignIn::Limited,
        Ok(r) if !r.status().is_success() => NavidromeSignIn::Wrong,
        Ok(r) => match r.bytes().await {
            Err(e) => NavidromeSignIn::Unreachable(e.to_string()),
            Ok(bytes) => match serde_json::from_slice::<Value>(&bytes) {
                Ok(document) if document.get("isAdmin") == Some(&Value::Bool(true)) => {
                    NavidromeSignIn::Admin(bytes)
                }
                Ok(_) => NavidromeSignIn::NotAdmin,
                Err(e) => NavidromeSignIn::Unreachable(e.to_string()),
            },
        },
    }
}

fn signed_in_answer(web: &Web, token: &str, value: &Value) -> Response {
    let mut res = ok(value);
    res.headers_mut().append(header::SET_COOKIE, web.cookie(token));
    res
}

/// `POST /api/family/auth`: a member signs in with their account password; the owner, or
/// anyone with a Navidrome account, with their Navidrome password.
pub async fn sign_in(State(state): State<AppState>, web: Web, body: Bytes) -> Response {
    let request: SignInBody = match body_as(&body) {
        Ok(request) => request,
        Err(res) => return res,
    };
    let username = request.username.trim().to_string();
    if is_blank(&username) || request.password.is_empty() {
        return refusal(StatusCode::UNAUTHORIZED, "Type your username and password.");
    }
    let throttle = &state.family.sign_in_throttle;
    // Tries count per address and account (so signing in to one account never clears the
    // wrong tries at another), per peer Octo can vouch for, and per account from everywhere,
    // where an address that signed in to the account before counts apart from new ones.
    let keys = state
        .family_bridge
        .try_keys(&web.address, &web.peer, &username, SignInThrottle::MAX_FAILURES);
    let keys = keys.keys();
    let known = |user: &str| state.family_bridge.known.signed_in(&web.address, user);
    let throttled = |wait| {
        family_error(&crate::services::family::FamilyError::Throttled(
            format!(
                "Too many wrong sign-ins. Try again in {}.",
                SignInThrottle::minutes(wait)
            ),
            wait,
        ))
    };
    if let Some(wait) = throttle.retry_after_any(keys) {
        return throttled(wait);
    }
    let enabled = state.settings.current().family.enabled;
    if let Some(member) = state.family_store.member(&username) {
        if !enabled {
            return refusal(StatusCode::NOT_FOUND, "Family is off on this server.");
        }
        if !member.has_password() {
            return refusal(
                StatusCode::UNAUTHORIZED,
                if member.signed_up() {
                    "Your password needs choosing again. Ask whoever runs this server for a new sign-in link."
                } else {
                    "Open your invite link to set your password first."
                },
            );
        }
        // The try counts before the password is checked, so tries sent all at once cannot each
        // pass the throttle while the others are still being checked. It counts with the
        // member's sign-ins everywhere else too (the apps, the web player, signed calls), so
        // guesses spread over every surface share one account limit.
        if let Some(wait) = throttle.attempt_all(keys) {
            return throttled(wait);
        }
        let everywhere = state.family_bridge.try_keys(
            &web.address,
            &web.peer,
            &member.username,
            crate::services::family::member_bridge::MAX_FAILURES,
        );
        if !state.family_bridge.throttle.attempt_by(&everywhere) {
            throttle.forgive_all(keys);
            return throttled(crate::services::family::member_bridge::FAILURE_WINDOW);
        }
        if !FamilyService::check_member_password(&state, &member, &request.password) {
            return refusal(StatusCode::UNAUTHORIZED, "Wrong username or password.");
        }
        if member.suspended {
            return refusal(
                StatusCode::FORBIDDEN,
                "This account is paused by the family owner.",
            );
        }
        // This try was right: it is taken back everywhere, and the address's wrong tries at
        // this account are cleared.
        throttle.forgive_all(keys);
        state.family_bridge.throttle.forgive_by(&everywhere);
        throttle.succeeded(&keys[0].0);
        known(&member.username);
        let token = state
            .browse_sessions
            .create_family(&member.username, SessionKind::Member);
        info!("FAMILY page sign-in for {}", member.username);
        return signed_in_answer(
            &web,
            &token,
            &json!({ "username": member.username, "role": Role::from(member.preset) }),
        );
    }
    match navidrome_sign_in(&state, &username, &request.password).await {
        NavidromeSignIn::Unreachable(why) => {
            warn!("FAMILY page sign-in could not reach Navidrome: {why}");
            refusal(
                StatusCode::BAD_GATEWAY,
                "Octo could not reach Navidrome to check your password. Try again in a minute.",
            )
        }
        NavidromeSignIn::Limited => refusal(
            StatusCode::TOO_MANY_REQUESTS,
            "Navidrome is limiting sign-ins right now. Wait half a minute and try again.",
        ),
        NavidromeSignIn::Wrong => {
            throttle.failed_all(keys);
            refusal(StatusCode::UNAUTHORIZED, "Wrong username or password.")
        }
        NavidromeSignIn::NotAdmin if !enabled => {
            refusal(StatusCode::NOT_FOUND, "Family is off on this server.")
        }
        NavidromeSignIn::NotAdmin => {
            throttle.succeeded(&keys[0].0);
            known(&username);
            let token = state
                .browse_sessions
                .create_family(&username, SessionKind::FamilyGuest);
            signed_in_answer(
                &web,
                &token,
                &json!({ "username": username, "role": Role::Unmanaged }),
            )
        }
        NavidromeSignIn::Admin(login) => {
            throttle.succeeded(&keys[0].0);
            known(&username);
            state.navidrome_identity.capture_login(&login);
            let owner = state.family_store.owner();
            let role = if owner.is_empty() || eq_ignore_case(&owner, &username) {
                Role::Owner
            } else {
                Role::Unmanaged
            };
            let token = state
                .browse_sessions
                .create_family(&username, SessionKind::FamilyNavidrome);
            info!("FAMILY page sign-in for {username} (Navidrome admin)");
            signed_in_answer(&web, &token, &json!({ "username": username, "role": role }))
        }
    }
}

/// `GET /api/family/session`: who is signed in, and what the page should show them.
pub async fn session(State(state): State<AppState>, web: Web) -> Response {
    let signed = match web.signed(&state).await {
        Ok(signed) => signed,
        Err(res) => return res,
    };
    let settings = state.settings.current();
    let member = state.family_store.member(signed.user());
    let manager = settings.family.enabled && FamilyService::may_manage(&state, &signed.actor);
    let approver = settings.family.enabled && FamilyService::may_approve(&state, &signed.actor);
    let links = state.family.link_status(&state);
    let place = crate::services::family::members_place::place(&state, None);
    let members_path = place.path.clone();
    // Only the people who manage the family are told where its music lives.
    let members_place = (manager || signed.actor.is_owner()).then_some(place);
    // Only the people who can change the server's settings are told how.
    let proxy_warning = web
        .untrusted_proxy
        .filter(|_| manager || signed.actor.is_owner())
        .map(|proxy| settings.family.untrusted_proxy_fix(proxy));
    ok(&json!({
        "username": member.as_ref().map_or(signed.user().to_string(), |m| m.username.clone()),
        "role": signed.actor.role,
        "displayName": member.as_ref().map_or(signed.user().to_string(), |m| m.shown_name().to_string()),
        "managed": member.is_some(),
        "importPlaylists": member.as_ref().is_some_and(|m| m.abilities().import_playlists),
        "manager": manager,
        "approver": approver,
        "owner": signed.actor.is_owner(),
        "familyEnabled": settings.family.enabled,
        // The prefix every member's page shows next to the family playlists, so anyone can
        // found one from an app; blank means the capture is off. Managers alone may set it.
        "playlistPrefix": settings.family.playlist_prefix.clone(),
        "place": web.place,
        "manageFromAway": settings.family.manage_from_away,
        "membersPath": members_path,
        "membersPlace": members_place,
        "links": links.as_ref().map(|status| status.links),
        "linksMessage": links.as_ref().and_then(|status| status.message.clone()),
        "linksFix": links.as_ref().and_then(|status| status.fix.clone()),
        "proxyWarning": proxy_warning,
        "server": web.base(&state),
        "publicUrl": crate::services::family::public_address::public_url(&state),
        "homeOnly": web.link_bases(&state, signed.user()).home_only,
        // Only the owner saves the outside address, so only the owner is offered one.
        "suggestedPublicUrl": signed
            .actor
            .is_owner()
            .then(|| state.family.suggested_public_url())
            .flatten(),
    }))
}

/// `POST /api/family/signout`.
pub async fn sign_out(State(state): State<AppState>, web: Web) -> Response {
    state.browse_sessions.revoke(web.token().as_deref());
    let mut res = no_content();
    res.headers_mut().append(header::SET_COOKIE, Web::cookie_gone());
    res
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct JoinBody {
    pub token: String,
    pub password: String,
    pub display_name: Option<String>,
}

/// `POST /api/family/join`: an invite link's holder sets their account password and is signed
/// in.
pub async fn join(State(state): State<AppState>, web: Web, body: Bytes) -> Response {
    if let Err(res) = require_on(&state) {
        return res;
    }
    let request: JoinBody = match body_as(&body) {
        Ok(request) => request,
        Err(res) => return res,
    };
    if request.token.trim().is_empty() {
        return refusal(
            StatusCode::BAD_REQUEST,
            "This link is not complete. Open it again.",
        );
    }
    match state
        .family
        .join(
            &state,
            &web.throttle_key(),
            &request.token,
            &request.password,
            request.display_name.as_deref(),
        )
        .await
    {
        Ok(username) => {
            let token = state
                .browse_sessions
                .create_family(&username, SessionKind::Member);
            signed_in_answer(&web, &token, &json!({ "username": username }))
        }
        Err(e) => family_error(&e),
    }
}

/// `GET /api/family/me`: the `family.me` object, with the member's push topic.
pub async fn me(State(state): State<AppState>, web: Web) -> Response {
    let signed = match web.anyone(&state).await {
        Ok(signed) => signed,
        Err(res) => return res,
    };
    let mut me = state
        .family
        .me_json(&state, signed.user(), signed.device_id.as_deref(), web.place);
    if let (Value::Object(map), Some(member)) = (&mut me, state.family_store.member(signed.user())) {
        map.insert("ntfyTopic".into(), json!(member.ntfy_topic));
    }
    ok(&me)
}

/// `GET /api/family/me/devices`.
pub async fn my_devices(State(state): State<AppState>, web: Web) -> Response {
    let signed = match web.anyone(&state).await {
        Ok(signed) => signed,
        Err(res) => return res,
    };
    let devices: Vec<Value> = FamilyService::devices_of(&state, signed.user())
        .iter()
        .map(|d| state.family.device_json(&state, d, signed.device_id.as_deref()))
        .collect();
    ok(&Value::Array(devices))
}

/// `GET /api/family/me/login`: what the signed-in person types into any app: their username
/// and the server addresses (their password is the one they chose).
pub async fn my_login(State(state): State<AppState>, web: Web) -> Response {
    let signed = match web.anyone(&state).await {
        Ok(signed) => signed,
        Err(res) => return res,
    };
    let bases = web.link_bases(&state, signed.user());
    ok(&FamilyService::login_json(&state, signed.user(), &bases))
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct DeviceChangeBody {
    pub quality: Option<String>,
    pub name: Option<String>,
}

/// Reads `Account` or `App`.
pub fn quality_mode(text: &str) -> Option<DeviceQualityMode> {
    match text.trim().to_ascii_lowercase().as_str() {
        "account" => Some(DeviceQualityMode::Account),
        "app" => Some(DeviceQualityMode::App),
        _ => None,
    }
}

/// `PUT /api/family/me/devices/{id}`: how the device picks its quality, and its name.
pub async fn change_my_device(
    State(state): State<AppState>,
    web: Web,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    let signed = match web.anyone(&state).await {
        Ok(signed) => signed,
        Err(res) => return res,
    };
    let request: DeviceChangeBody = match body_as(&body) {
        Ok(request) => request,
        Err(res) => return res,
    };
    let mode = match request.quality.as_deref() {
        None => None,
        Some(text) => match quality_mode(text) {
            Some(mode) => Some(mode),
            None => return refusal(StatusCode::BAD_REQUEST, "Choose Account or App."),
        },
    };
    match state
        .family
        .set_device(&state, signed.user(), &id, mode, request.name.as_deref())
    {
        Ok(device) => ok(&state.family.device_json(&state, &device, None)),
        Err(e) => family_error(&e),
    }
}

/// `DELETE /api/family/me/devices/{id}`: "Remove from list". The device is recorded again if
/// it signs in again.
pub async fn forget_my_device(State(state): State<AppState>, web: Web, Path(id): Path<String>) -> Response {
    let signed = match web.anyone(&state).await {
        Ok(signed) => signed,
        Err(res) => return res,
    };
    match state.family.forget_device(&state, &signed.actor, &id, false) {
        Ok(()) => no_content(),
        Err(e) => family_error(&e),
    }
}

/// `GET /api/family/me/requests`.
pub async fn my_requests(State(state): State<AppState>, web: Web) -> Response {
    let signed = match web.anyone(&state).await {
        Ok(signed) => signed,
        Err(res) => return res,
    };
    let list: Vec<Value> = FamilyService::requests(&state, Some(signed.user()), None)
        .iter()
        .map(|r| FamilyService::request_json(&state, r))
        .collect();
    ok(&Value::Array(list))
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct AskBody {
    pub id: String,
    pub quality: Option<String>,
}

/// `POST /api/family/me/requests`: asks for a copy.
pub async fn ask(State(state): State<AppState>, web: Web, body: Bytes) -> Response {
    let (signed, _) = match web.member(&state).await {
        Ok(found) => found,
        Err(res) => return res,
    };
    let request: AskBody = match body_as(&body) {
        Ok(request) => request,
        Err(res) => return res,
    };
    if request.id.trim().is_empty() {
        return refusal(StatusCode::BAD_REQUEST, "Say which song to ask for.");
    }
    let quality = match request.quality.as_deref() {
        None | Some("") => RequestQuality::Best,
        Some(text) => match RequestQuality::parse(text) {
            Some(quality) => quality,
            None => return refusal(StatusCode::BAD_REQUEST, "Choose Best, Flac or Mp3."),
        },
    };
    let hooks = state.family.hooks(&state);
    match hooks
        .requests
        .create(NewRequest {
            username: signed.user().to_string(),
            target: request.id.trim().to_string(),
            quality,
            kind: None,
        })
        .await
    {
        Ok(made) => ok(&FamilyService::request_json(&state, &made)),
        Err(e) => family_error(&e),
    }
}

/// `DELETE /api/family/me/requests/{id}`: cancels one of the member's own waiting requests.
pub async fn cancel_my_request(State(state): State<AppState>, web: Web, Path(id): Path<String>) -> Response {
    let (signed, _) = match web.member(&state).await {
        Ok(found) => found,
        Err(res) => return res,
    };
    match state
        .family
        .hooks(&state)
        .requests
        .cancel(&id, signed.user())
        .await
    {
        Ok(request) => ok(&FamilyService::request_json(&state, &request)),
        Err(e) => family_error(&e),
    }
}

/// `GET /api/family/me/saved`.
pub async fn my_saved(State(state): State<AppState>, web: Web) -> Response {
    let (signed, _) = match web.member(&state).await {
        Ok(found) => found,
        Err(res) => return res,
    };
    ok(&state.family.hooks(&state).saved.list(signed.user()))
}

/// `DELETE /api/family/me/saved/{id}`.
pub async fn forget_saved(State(state): State<AppState>, web: Web, Path(id): Path<String>) -> Response {
    let (signed, _) = match web.member(&state).await {
        Ok(found) => found,
        Err(res) => return res,
    };
    if state.family.hooks(&state).saved.remove(signed.user(), &id).await {
        no_content()
    } else {
        refusal(StatusCode::NOT_FOUND, "That is not in your saved songs.")
    }
}

/// `GET /api/family/me/library?offset=&size=`.
pub async fn my_library(State(state): State<AppState>, web: Web, RawQuery(query): RawQuery) -> Response {
    let (signed, member) = match web.member(&state).await {
        Ok(found) => found,
        Err(res) => return res,
    };
    let number = |name: &str, default: usize| {
        url::form_urlencoded::parse(query.as_deref().unwrap_or("").as_bytes())
            .find(|(k, _)| k == name)
            .and_then(|(_, v)| v.trim().parse::<usize>().ok())
            .unwrap_or(default)
    };
    let (offset, size) = (number("offset", 0), number("size", 100).clamp(1, 500));
    let hooks = state.family.hooks(&state);
    let page = hooks.my_library.list(signed.user(), offset, size);
    ok(&json!({
        "songs": page.songs,
        "total": page.total,
        "usedBytes": hooks.storage.used_bytes(signed.user()),
        "limitBytes": member.abilities().storage_limit_bytes().unwrap_or(0),
    }))
}

/// `DELETE /api/family/me/library/{id}`.
pub async fn remove_from_my_library(
    State(state): State<AppState>,
    web: Web,
    Path(id): Path<String>,
) -> Response {
    let (signed, _) = match web.member(&state).await {
        Ok(found) => found,
        Err(res) => return res,
    };
    match state
        .family
        .hooks(&state)
        .my_library
        .remove(signed.user(), &id)
        .await
    {
        Ok(()) => no_content(),
        Err(e) => family_error(&e),
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct PasswordBody {
    pub current: String,
    pub next: String,
}

/// `PUT /api/family/me/password`: "Sign out everywhere". Every app needs the new password,
/// and every web player and Family page session ends; the page that asked gets a new sign-in
/// of its own.
pub async fn change_password(State(state): State<AppState>, web: Web, body: Bytes) -> Response {
    let (signed, _) = match web.member(&state).await {
        Ok(found) => found,
        Err(res) => return res,
    };
    let request: PasswordBody = match body_as(&body) {
        Ok(request) => request,
        Err(res) => return res,
    };
    let keys = state.family_bridge.try_keys(
        &web.address,
        &web.peer,
        signed.user(),
        crate::services::family::member_bridge::MAX_FAILURES,
    );
    match state
        .family
        .change_password(&state, signed.user(), &request.current, &request.next, &keys)
        .await
    {
        Ok(()) => {
            let mut res = no_content();
            if web.signed_call.is_none() {
                let token = state
                    .browse_sessions
                    .create_family(signed.user(), SessionKind::Member);
                res.headers_mut().append(header::SET_COOKIE, web.cookie(&token));
            }
            res
        }
        Err(e) => family_error(&e),
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct QualityBody {
    pub home: Option<String>,
    pub away: Option<String>,
}

/// Parses an audio quality choice, or None when it is left out.
pub fn quality_choice(text: Option<&str>) -> Result<Option<StreamQuality>, &'static str> {
    match text.map(str::trim).filter(|t| !t.is_empty()) {
        None => Ok(None),
        Some(text) => StreamQuality::parse(text)
            .map(Some)
            .ok_or("Choose Original, High, Standard or DataSaver."),
    }
}

/// Sets `user`'s audio quality, keeping what is left out. Answers the new preference.
pub fn set_preference(
    state: &AppState,
    user: &str,
    home: Option<StreamQuality>,
    away: Option<StreamQuality>,
) -> QualityPreference {
    let mut preference = state.family_store.preference(user);
    if let Some(home) = home {
        preference.home = home;
    }
    if let Some(away) = away {
        preference.away = away;
    }
    state.family_store.set_preference(user, preference);
    preference
}

/// `PUT /api/family/me/quality`: anyone signed in, the owner included.
pub async fn set_quality(State(state): State<AppState>, web: Web, body: Bytes) -> Response {
    let signed = match web.anyone(&state).await {
        Ok(signed) => signed,
        Err(res) => return res,
    };
    let request: QualityBody = match body_as(&body) {
        Ok(request) => request,
        Err(res) => return res,
    };
    let (home, away) = match (
        quality_choice(request.home.as_deref()),
        quality_choice(request.away.as_deref()),
    ) {
        (Ok(home), Ok(away)) => (home, away),
        (Err(words), _) | (_, Err(words)) => return refusal(StatusCode::BAD_REQUEST, words),
    };
    let preference = set_preference(&state, signed.user(), home, away);
    ok(&quality_json(
        &preference,
        &FamilyService::abilities_of(&state, signed.user()),
    ))
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct NtfyBody {
    pub topic: String,
}

/// `PUT /api/family/me/ntfy`: the member's own push topic (empty turns pushes off).
pub async fn set_ntfy(State(state): State<AppState>, web: Web, body: Bytes) -> Response {
    let (_, mut member) = match web.member(&state).await {
        Ok(found) => found,
        Err(res) => return res,
    };
    let request: NtfyBody = match body_as(&body) {
        Ok(request) => request,
        Err(res) => return res,
    };
    let topic = request.topic.trim();
    if !topic.is_empty() && !is_member_topic(topic) {
        return refusal(
            StatusCode::BAD_REQUEST,
            "Use 1 to 64 letters, numbers, dashes or underscores for the topic.",
        );
    }
    member.ntfy_topic = topic.to_string();
    state.family_store.upsert_member(member);
    no_content()
}

/// `GET /api/family/cover/{id}?size=`: a cover for the page (requests, saved songs), fetched
/// as the person signed in: a member with their own sign-in, the owner with Octo's admin one.
pub async fn cover(
    State(state): State<AppState>,
    web: Web,
    Path(id): Path<String>,
    RawQuery(query): RawQuery,
) -> Response {
    let signed = match web.anyone(&state).await {
        Ok(signed) => signed,
        Err(res) => return res,
    };
    let size = url::form_urlencoded::parse(query.as_deref().unwrap_or("").as_bytes())
        .find(|(k, _)| k == "size")
        .and_then(|(_, v)| v.trim().parse::<u32>().ok())
        .unwrap_or(120)
        .clamp(32, 600)
        .to_string();
    let mut parameters: Vec<(String, String)> = match state.family_credentials.fresh(signed.user()) {
        Some(credential) => credential.parameters(&[]).into_iter().collect(),
        None => match state.navidrome_identity.get_scan_auth() {
            Some((user, token, salt)) => vec![
                ("u".into(), user),
                ("t".into(), token),
                ("s".into(), salt),
                ("v".into(), "1.16.1".into()),
                ("c".into(), "octo".into()),
            ],
            None => Vec::new(),
        },
    };
    parameters.push(("id".into(), id));
    parameters.push(("size".into(), size));
    let encoded: String = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(parameters)
        .finish();
    let Ok(req) = axum::http::Request::builder()
        .method(axum::http::Method::GET)
        .uri(format!("/rest/getCoverArt?{encoded}"))
        .body(axum::body::Body::empty())
    else {
        return refusal(StatusCode::BAD_REQUEST, "That is not a cover.");
    };
    let mut res = match crate::controllers::subsonic::media::get_cover_art(State(state.clone()), req).await {
        Ok(res) => res,
        Err(e) => axum::response::IntoResponse::into_response(e),
    };
    res.headers_mut().insert(
        header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("private, max-age=86400"),
    );
    res
}

/// `GET /api/family/me/lastfm`: whether the member's plays go to Last.fm.
pub async fn my_last_fm(State(state): State<AppState>, web: Web) -> Response {
    let (signed, _) = match web.member(&state).await {
        Ok(found) => found,
        Err(res) => return res,
    };
    let settings = state.settings.current();
    let ready = !is_blank(&settings.last_fm.api_key) && !is_blank(&settings.last_fm.api_secret);
    let user = state
        .last_fm_scrobbles
        .users([signed.user()])
        .into_iter()
        .find(|u| eq_ignore_case(&u.user, signed.user()));
    ok(&json!({
        "available": ready,
        "user": user,
    }))
}

/// `POST /api/family/me/lastfm/{action}`: connect (the page to approve Octo on last.fm),
/// finish, cancel or disconnect, for the member signed in.
pub async fn last_fm_action(State(state): State<AppState>, web: Web, Path(action): Path<String>) -> Response {
    let (signed, _) = match web.member(&state).await {
        Ok(found) => found,
        Err(res) => return res,
    };
    let user = signed.user().to_string();
    let scrobbles = &state.last_fm_scrobbles;
    match action.to_ascii_lowercase().as_str() {
        "connect" => match scrobbles.begin_connect(&user).await {
            Ok(url) => ok(&json!({ "url": url })),
            Err(e) => refusal(StatusCode::BAD_REQUEST, &e.message),
        },
        "finish" => match scrobbles.finish_connect(&user).await {
            Ok(session) => ok(&json!({ "lastFmUser": session.last_fm_user })),
            Err(LastFmConnectError::Refused(e)) => refusal(StatusCode::CONFLICT, &e.message),
            Err(e) => {
                warn!("FAMILY Last.fm connect for {user} could not be saved: {e}");
                refusal(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Last.fm approved Octo, but Octo could not save it. Ask the owner to check Octo's settings file.",
                )
            }
        },
        "cancel" => {
            scrobbles.cancel_connect(&user);
            no_content()
        }
        "disconnect" => match scrobbles.disconnect(&user) {
            Ok(_) => no_content(),
            Err(e) => refusal(StatusCode::BAD_REQUEST, &e.message),
        },
        _ => refusal(
            StatusCode::NOT_FOUND,
            "Use connect, finish, cancel or disconnect.",
        ),
    }
}
