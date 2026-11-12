#!/usr/bin/env bash
# Live smoke for the release gate: the built images against the real world, not just each
# other. Hermetic checks (ci.yml, the build itself) cannot see YouTube's anti-bot wall or a
# Subsonic stack that answers with the wrong shape -- SOAK-1b shipped because every gate was
# hermetic. This script is the armor ordered for the public branch: it runs the built shim
# against live YouTube and the built Octo against a real Navidrome, and fails the release
# when either side misbehaves.
#
# Inputs (environment):
#   SHIM_IMAGE  full image ref of the shim built by this run (docker.yml passes the merge
#               job's tag)
#   OCTO_IMAGE  full image ref of Octo built by this run
#   ND_IMAGE    Navidrome image to run Octo against (default deluan/navidrome:latest)
#
# Part 1 (YouTube, live): shim container -> /health, /search/list must answer hits, and the
#   first hit's /stream must hand over audio bytes (~64 KiB of them; the whole track is never
#   pulled). A resolve that hits YouTube's "confirm you're not a bot" wall fails here.
# Part 2 (Navidrome, live in CI): Octo beside a real Navidrome -> ping, search3 (an empty
#   catalog answers ok), then playlist create/get/rename/delete/getPlaylists through Octo's
#   overlay, so a release cannot ship a Subsonic surface that breaks clients.

set -euo pipefail

die() { echo "::error::$*"; exit 1; }
log() { echo "[live-smoke] $*"; }

for tool in curl jq docker; do
  command -v "$tool" >/dev/null 2>&1 || die "$tool is required but not installed"
done

SHIM_IMAGE="${SHIM_IMAGE:?SHIM_IMAGE (the built shim image) is required}"
OCTO_IMAGE="${OCTO_IMAGE:?OCTO_IMAGE (the built Octo image) is required}"
ND_IMAGE="${ND_IMAGE:-deluan/navidrome:latest}"

NET=live-smoke-net
SHIM_PORT=5011
OCTO_PORT=5274
ND_ADMIN_USER=admin
ND_ADMIN_PASS=smoke-admin-pass
# Subsonic auth with the password hex-encoded (p=enc:...): not plain on the URL, and unlike
# token auth it works against a Navidrome admin created by ND_DEVAUTOCREATEADMINPASSWORD,
# whose stored hash the token check does not match.
ND_PASS_HEX="$(printf '%s' "$ND_ADMIN_PASS" | od -An -tx1 | tr -d ' \n')"
SUBSONIC_AUTH="u=$ND_ADMIN_USER&p=enc:$ND_PASS_HEX&v=1.16.1&c=live-smoke&f=json"

cleanup() {
  for name in live-smoke-shim live-smoke-octo live-smoke-navidrome; do
    docker rm -f "$name" >/dev/null 2>&1 || true
  done
  docker network rm "$NET" >/dev/null 2>&1 || true
}
trap cleanup EXIT

wait_for() { # wait_for <url> <what>
  local i
  for i in $(seq 1 60); do
  if curl -fsS --max-time 5 "$1" >/dev/null 2>&1; then return 0; fi
    sleep 2
  done
  die "$2 never came up: $1"
}

# The JSON the endpoint answered, or a fail: every assertion below reads these.
sub() { # sub <endpoint-query> <what>  -> echoes the body, dies on a non-2xx
  local body code
  body="$(curl -sS --max-time 60 -w '\n%{http_code}' "http://localhost:$OCTO_PORT/rest/$1&$SUBSONIC_AUTH")" \
    || die "$2: request failed"
  code="${body##*$'\n'}"
  body="${body%$'\n'*}"
  [[ "$code" == 2* ]] || die "$2: HTTP $code, body: $body"
  jq -e '.["subsonic-response"].status == "ok"' >/dev/null 2>&1 <<<"$body" \
    || die "$2: subsonic-response is not ok: $body"
  echo "$body"
}

docker network create "$NET" >/dev/null

# ---------- Part 1: the shim against live YouTube ----------

log "part 1: shim $SHIM_IMAGE against live YouTube"
docker run -d --name live-smoke-shim --network "$NET" -p "127.0.0.1:$SHIM_PORT:8080" \
  "$SHIM_IMAGE" >/dev/null || die "could not start the shim container"
wait_for "http://localhost:$SHIM_PORT/health" "the shim's /health"

# A search must answer hits. The query's content does not matter, only that YouTube answers
# it; "Mozart" is stable, well-indexed and safe.
SEARCH="$(curl -fsS --max-time 40 "http://localhost:$SHIM_PORT/search/list?q=Mozart&limit=3")" \
  || die "/search/list failed against live YouTube (bot wall or network)"
VIDEO_ID="$(jq -r '.hits[0].video_id // empty' <<<"$SEARCH")"
[[ -n "$VIDEO_ID" ]] || die "/search/list answered no hits: $SEARCH"
log "/search/list answered $(jq '.hits | length' <<<"$SEARCH") hits; first video $VIDEO_ID"

# The first hit's stream must hand over audio bytes. ~64 KiB is enough to prove the resolve
# produced a working googlevideo URL; the whole track is never pulled.
STREAM_HEAD=/tmp/live-smoke-stream-head.bin
CODE="$(curl -sS --max-time 90 -o "$STREAM_HEAD" -w '%{http_code}' \
  -r 0-65535 "http://localhost:$SHIM_PORT/stream?id=$VIDEO_ID")" \
  || { docker logs live-smoke-shim 2>&1 | tail -30; die "/stream failed for $VIDEO_ID"; }
[[ "$CODE" == 200 || "$CODE" == 206 ]] || { docker logs live-smoke-shim 2>&1 | tail -30; die "/stream answered HTTP $CODE for $VIDEO_ID"; }
BYTES="$(stat -c%s "$STREAM_HEAD")"
[[ "$BYTES" -gt 1024 ]] || die "/stream answered only $BYTES bytes for $VIDEO_ID"
log "/stream answered HTTP $CODE with $BYTES bytes of audio (first hit resolves and plays)"

# ---------- Part 2: Octo beside a real Navidrome ----------

log "part 2: $OCTO_IMAGE beside a real Navidrome ($ND_IMAGE)"
docker run -d --name live-smoke-navidrome --network "$NET" \
  -e "ND_DEVAUTOCREATEADMINPASSWORD=$ND_ADMIN_PASS" \
  "$ND_IMAGE" >/dev/null || die "could not start Navidrome"
docker run -d --name live-smoke-octo --network "$NET" -p "127.0.0.1:$OCTO_PORT:8080" \
  -e "Subsonic__Url=http://live-smoke-navidrome:4533" \
  -e "Subsonic__AdminUsername=$ND_ADMIN_USER" \
  -e "Subsonic__AdminPassword=$ND_ADMIN_PASS" \
  -e "Subsonic__StorageMode=Stream" \
  -e "YouTube__ShimUrl=http://live-smoke-shim:8080" \
  -v "$(mktemp -d):/music" \
  "$OCTO_IMAGE" >/dev/null || die "could not start the Octo container"

# ping goes through Octo's overlay to Navidrome, so it is up only once both are.
wait_for "http://localhost:$OCTO_PORT/rest/ping.view?$SUBSONIC_AUTH" "Octo/Navidrome's ping"
sub "ping.view?" "ping" >/dev/null
log "ping: ok through Octo's overlay"

# search3 on an empty catalog: the answer must be ok (an empty result), not an error.
sub "search3.view?query=mozart&songCount=1&artistCount=1&albumCount=1&" "search3 (empty catalog)" >/dev/null
log "search3: ok on the empty catalog"

# Playlist CRUD through Octo: create -> read -> rename -> delete -> gone.
CREATED="$(sub "createPlaylist.view?name=live-smoke-playlist&" "createPlaylist")"
PLAYLIST_ID="$(jq -r '.["subsonic-response"].playlist.id // empty' <<<"$CREATED")"
[[ -n "$PLAYLIST_ID" ]] || die "createPlaylist answered no playlist id: $CREATED"
log "createPlaylist: id $PLAYLIST_ID"

GOT="$(sub "getPlaylist.view?id=$PLAYLIST_ID&" "getPlaylist")"
jq -e --arg id "$PLAYLIST_ID" '.["subsonic-response"].playlist.id == $id' >/dev/null 2>&1 <<<"$GOT" \
  || die "getPlaylist did not answer the created playlist: $GOT"
log "getPlaylist: answered the created playlist"

sub "updatePlaylist.view?playlistId=$PLAYLIST_ID&name=live-smoke-playlist-renamed&" "updatePlaylist (rename)" >/dev/null
GOT="$(sub "getPlaylist.view?id=$PLAYLIST_ID&" "getPlaylist after rename")"
jq -e '.["subsonic-response"].playlist.name == "live-smoke-playlist-renamed"' >/dev/null 2>&1 <<<"$GOT" \
  || die "the rename did not stick: $GOT"
log "updatePlaylist: rename stuck"

sub "deletePlaylist.view?id=$PLAYLIST_ID&" "deletePlaylist" >/dev/null
LIST="$(sub "getPlaylists.view?" "getPlaylists")"
jq -e --arg id "$PLAYLIST_ID" '(.["subsonic-response"].playlists.playlist // []) | any(.id == $id) | not' \
  >/dev/null 2>&1 <<<"$LIST" \
  || die "the deleted playlist is still listed: $LIST"
log "deletePlaylist: the playlist is gone"

log "live smoke passed: YouTube search+stream live, Octo+Navidrome ping/search3/playlist CRUD"
