# Changelog

## 2026.11.11 — YouTube search, mixed personal playlists, streaming favorites

This fork's first delivery, in three independent parts. Upstream:
[winters27/octo](https://github.com/winters27/octo) (base: `475ec631`, release
`2026.10.28`). Each part is one commit on this branch, so any of them can be
reverted on its own.

### 1. Search reaches YouTube Music and YouTube (`feat(search)`)

The outside search core no longer needs a Last.fm key to answer. It queries the
YouTube Music `songs` and `videos` catalogs in parallel, interleaves them, and
falls back to a plain YouTube search through `yt-dlp` when neither has the
track. A row in the answer always says where it came from
(`octoSuggestedBy`: `YouTube Music (song)`, `YouTube Music (video)`, `YouTube`).

**A video and a song are two different things and are never merged into one
row** — the same recording released both ways shows up twice, one row each,
and the choice stays with the listener. Alternative versions of a recording
(`Extended Mix`, `Live`, `Radio Edit`, a DJ set…) always stay separate rows.
Duplicate uploads of the *same* recording are folded, keeping the video row
ahead of the song row.

Measured on the live catalog: searching `K3KHA - Lost in Sound` with no Last.fm
key returns the official video (`vBvDiLKTDWA`, 619 s) first, the song release
(`rwqD5PrajEA`, 619 s) right behind it as its own row, and the 56-minute DJ set
as a separate row.

The stream resolver also got a fast extraction profile (one combined
`--extractor-args`, skipping the configs, initial data and subtitles it does not
need), applied only when resolving a stream URL — searches are untouched. The
video page itself is always fetched: YouTube's bot check lives there, and
skipping it gets the resolve rejected.

### 2. Mixed personal playlists, REST and native (`feat(playlists)`)

A personal playlist holds local library songs and outside (YouTube) tracks side
by side, in both protocols Subsonic clients use (Subsonic REST and Navidrome's
native API — Feishin works over either). Adding a track to a playlist never
downloads anything.

- Every position keeps a **full song snapshot** and the **registry seed** it
  was minted with, so after a server restart — or after the id registry is
  wiped — `getPlaylist`, `getSong`, `stream` and the native endpoints answer
  from the snapshot and the registry rebuilds itself.
- Playback picks the source per position: a local file plays from disk
  (`PREFER_LOCAL`), and the moment the file disappears the position stays on
  the list and quietly returns to the YouTube stream.
- The overlay only mirrors **local ids** to Navidrome (Navidrome never sees an
  outside id); a plain playlist stays Navidrome's own unless a mixed edit
  adopts it.
- Folders: a playlist's `folder` field, a `Folder / Subfolder / Name` prefix in
  its name, and a `#folder:Path` tag in its comment stay in sync — paths with
  spaces included.
- Explicit downloads, on the owner's terms: starring a playlist queues only
  the positions with no local file, `rest/download?id=…&save=1` fetches the
  outside song it streams, and the owner's "Request a copy in FLAC" action
  playlist fetches what is added to it (only the owner's, and only on adds).
- One atomic read-modify-write per store mutation — parallel edits to the same
  playlist cannot lose each other's changes.

### 3. Streaming favorites when `DOWNLOAD_ON_STAR=false` (`fix(stars)`)

With heart downloads off, starring an outside song or album used to send the
outside id to Navidrome, which rejected it. Now it saves a streaming favorite
under the signed-in owner (the same `family-saved.json` store the family
members use): the favorite shows up in `getStarred`/`getStarred2` (songs and
albums), the native `/api/song` answer carries `starred`/`starredAt`, the
Liked Songs playlist appends it, and unstarring removes it. The default
semantics — a heart downloads the best copy — and every family-member path are
untouched.

---

Verified as a whole: 4476 tests passing, `clippy -D warnings` clean, and the
family machinery (`rules.rs`, the heart write gate, family playlists) byte
identical to upstream.
