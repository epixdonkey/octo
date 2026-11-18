"""/live/stream: the endless-MP3 contract (headers, no metaint, no loudness), the abr
clamps, the hang-up kill, and the re-resolve/restart self-healing. No network and no real
ffmpeg: the transcode is stood in for by `cat` (bytes then death) and a sleeping Python
(the bytes stop and the hang-up must kill it) — a lifecycle witness, not an audio one."""
import os
import subprocess
import sys

import pytest

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

app = pytest.importorskip("app")


def one_then_none(url):
    """A resolve that answers `url` once (the pre-response resolve) and never again."""
    answers = iter([url])
    return lambda video_id, bg=False: next(answers, None)


def test_the_headers_body_and_silence_of_a_live_answer(monkeypatch, tmp_path):
    measured = []
    monkeypatch.setattr(app, "_maybe_measure_loudness",
                        lambda video_id, url: measured.append(video_id))
    monkeypatch.setattr(app, "_resolve_url", one_then_none("https://manifest.example/live.m3u8"))
    payload = tmp_path / "live.mp3"
    payload.write_bytes(b"ID3" + b"x" * 100)
    spawned = []
    real_args = app._live_ffmpeg_args

    def cat_args(manifest, abr):
        spawned.append((manifest, abr))
        return ["cat", str(payload)]

    monkeypatch.setattr(app, "_live_ffmpeg_args", cat_args)
    monkeypatch.setattr(app, "_LIVE_RETRY_BACKOFF_SEC", ())
    answer = app.app.test_client().get("/live/stream?id=vid-live&abr=128&name=Lofi%20Live")
    assert answer.status_code == 200
    assert answer.headers["Content-Type"] == "audio/mpeg"
    assert answer.headers["Cache-Control"] == "no-store, no-transform"
    assert answer.headers["Accept-Ranges"] == "none"
    assert answer.headers["icy-br"] == "128"
    assert answer.headers["icy-name"] == "Lofi Live"
    assert "icy-metaint" not in answer.headers
    assert "Content-Length" not in answer.headers
    # The one pipe's whole output, exactly once, and no loudness was ever measured.
    assert answer.data == b"ID3" + b"x" * 100
    assert spawned == [("https://manifest.example/live.m3u8", 128)]
    assert measured == []


@pytest.mark.parametrize(
    "query,abr",
    [("", "192"), ("&abr=96", "96"), ("&abr=999", "320"), ("&abr=10", "64"), ("&abr=junk", "192")],
)
def test_abr_is_clamped_to_the_radio_range(query, abr):
    answer = app.app.test_client().head(f"/live/stream?id=vid-head{query}")
    assert answer.status_code == 200
    assert answer.headers["icy-br"] == abr
    assert answer.data == b""


def test_a_missing_id_is_a_400_and_a_nameless_call_omits_icy_name():
    assert app.app.test_client().get("/live/stream").status_code == 400
    answer = app.app.test_client().head("/live/stream?id=vid-anon")
    assert answer.status_code == 200
    assert "icy-name" not in answer.headers


def test_a_channel_that_cannot_be_resolved_is_a_502(monkeypatch):
    monkeypatch.setattr(app, "_resolve_url", lambda video_id, bg=False: None)
    answer = app.app.test_client().get("/live/stream?id=vid-dead")
    assert answer.status_code == 502
    assert answer.get_json() == {"error": "no_audio_url"}


def test_a_dead_pipe_re_resolves_restarts_and_then_closes_quietly(monkeypatch, tmp_path):
    answers = iter(["https://manifest.example/first.m3u8", "https://manifest.example/second.m3u8"])
    monkeypatch.setattr(app, "_resolve_url", lambda video_id, bg=False: next(answers, None))
    payload = tmp_path / "payload.bin"
    payload.write_bytes(b"ABC")
    asked = []

    def cat_args(manifest, abr):
        asked.append((manifest, abr))
        return ["cat", str(payload)]

    monkeypatch.setattr(app, "_live_ffmpeg_args", cat_args)
    monkeypatch.setattr(app, "_LIVE_RETRY_BACKOFF_SEC", (0.0, 0.0))
    import logging
    lines = []

    class Capture(logging.Handler):
        def emit(self, record):
            lines.append(record.getMessage())

    # Under pytest the module's basicConfig is a no-op (the root already has a handler),
    # which leaves this logger effectively at WARNING; the lifecycle lines are INFO.
    app.log.setLevel(logging.INFO)
    capture = Capture()
    app.log.addHandler(capture)
    try:
        answer = app.app.test_client().get("/live/stream?id=vid-restart&abr=96")
        consumed = answer.data  # the generator runs here, lazily
    finally:
        app.log.removeHandler(capture)
    assert answer.status_code == 200
    # Two pipes, each cat's whole file, nothing else between the attempts; the third
    # re-resolve answered nothing, so the answer ends there — quietly.
    assert consumed == b"ABCABC"
    assert asked == [
        ("https://manifest.example/first.m3u8", 96),
        ("https://manifest.example/second.m3u8", 96),
    ]
    # The lifecycle the log now tells on its own: opened with its time to the first byte,
    # each attempt's end with its exit code and byte count, the quiet close.
    assert any("opened, ttfb_ms=" in m and "vid-restart" in m for m in lines), lines
    assert sum("ffmpeg ended rc=" in m for m in lines) == 2, lines
    assert any("closing quietly after 3 attempts" in m for m in lines), lines
    assert app.app.test_client().get("/health").get_json()["live_streams"] == 0


def test_a_hung_up_caller_kills_the_transcode_and_frees_the_counter(monkeypatch):
    monkeypatch.setattr(app, "_resolve_url", one_then_none("https://manifest.example/live.m3u8"))
    # A full first chunk (a blocking read returns only when the whole chunk is there),
    # then the process holds the pipe open producing nothing — mid-read, the exact state
    # a listener's hang-up must interrupt.
    script = ("import sys, time; sys.stdout.buffer.write(b'y' * 65536); "
              "sys.stdout.buffer.flush(); time.sleep(10)")
    monkeypatch.setattr(app, "_live_ffmpeg_args", lambda manifest, abr: [sys.executable, "-c", script])
    real_popen = subprocess.Popen
    spawned = []

    def recording(*args, **kwargs):
        proc = real_popen(*args, **kwargs)
        spawned.append(proc)
        return proc

    monkeypatch.setattr(app.subprocess, "Popen", recording)
    answer = app.app.test_client().get("/live/stream?id=vid-hangup&abr=192", buffered=False)
    assert next(answer.response) == b"y" * (16 * 1024)
    answer.close()
    # Killed on the spot (a negative return code, not the 10 s exit), and the counter
    # the /health of the shim reports is back to zero.
    assert spawned[0].wait(timeout=5) != 0
    assert app.app.test_client().get("/health").get_json()["live_streams"] == 0


def test_the_first_chunk_is_small_so_the_first_byte_leaves_early(monkeypatch, tmp_path):
    """A blocking read returns only when the whole chunk is there, so the first read of an
    attempt is 16 KiB (well under a second at MP3 radio bitrates) and only the rest read
    the full 64 KiB. The reconnect loop production saw lived exactly in the 64 KiB gap."""
    monkeypatch.setattr(app, "_resolve_url", one_then_none("https://manifest.example/live.m3u8"))
    payload = tmp_path / "live.mp3"
    payload.write_bytes(b"x" * 200_000)
    monkeypatch.setattr(app, "_live_ffmpeg_args", lambda manifest, abr: ["cat", str(payload)])
    monkeypatch.setattr(app, "_LIVE_RETRY_BACKOFF_SEC", ())
    answer = app.app.test_client().get("/live/stream?id=vid-first&abr=192", buffered=False)
    sizes = [len(chunk) for chunk in answer.response]
    assert sizes[0] == 16 * 1024
    assert sizes[1] == 64 * 1024
    assert sum(sizes) == 200_000


def test_an_emoji_name_becomes_a_latin1_safe_icy_name(monkeypatch, tmp_path):
    """The production defect: gunicorn refuses a header value that does not encode to
    latin-1 (PEP 3333), so a station named with an emoji took the endpoint down with a 500
    per request. The name is sanitized, never refused — and latin-1's own letters survive."""
    monkeypatch.setattr(app, "_resolve_url", one_then_none("https://manifest.example/live.m3u8"))
    payload = tmp_path / "live.mp3"
    payload.write_bytes(b"ID3data")
    monkeypatch.setattr(app, "_live_ffmpeg_args", lambda manifest, abr: ["cat", str(payload)])
    monkeypatch.setattr(app, "_LIVE_RETRY_BACKOFF_SEC", ())
    answer = app.app.test_client().get(
        "/live/stream?id=vid-emoji&name=Deep%20Techno%2024%2F7%20%F0%9F%94%B4%20Caf%C3%A9")
    assert answer.status_code == 200
    name = answer.headers["icy-name"]
    # What gunicorn would have refused now encodes cleanly, emoji -> '?', é kept.
    name.encode("latin-1")
    assert name == "Deep Techno 24/7 ? Café"
    assert answer.data == b"ID3data"


def test_a_polish_tailed_name_keeps_latin1_letters_and_never_refuses(monkeypatch, tmp_path):
    """The Polish tail: ó is latin-1's own and survives untouched; ą ć ę ł ń ś ź ż live in
    latin-2, one table beyond what a PEP 3333 header value may hold, and each becomes '?' —
    the answer plays on where gunicorn would have 500'd on the raw name."""
    monkeypatch.setattr(app, "_resolve_url", one_then_none("https://manifest.example/live.m3u8"))
    payload = tmp_path / "live.mp3"
    payload.write_bytes(b"ID3data")
    monkeypatch.setattr(app, "_live_ffmpeg_args", lambda manifest, abr: ["cat", str(payload)])
    monkeypatch.setattr(app, "_LIVE_RETRY_BACKOFF_SEC", ())
    answer = app.app.test_client().get(
        "/live/stream?id=vid-polish&name=Radio%20"
        "%C4%85%C4%87%C4%99%C5%82%C5%84%C3%B3%C5%9B%C5%BA%C5%BC%2024%2F7")
    assert answer.status_code == 200
    name = answer.headers["icy-name"]
    name.encode("latin-1")  # the property gunicorn enforces and the raw name broke
    assert name == "Radio ?????ó??? 24/7"
    assert answer.data == b"ID3data"
