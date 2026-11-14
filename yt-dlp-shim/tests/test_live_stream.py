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
    answer = app.app.test_client().get("/live/stream?id=vid-restart&abr=96")
    assert answer.status_code == 200
    # Two pipes, each cat's whole file, nothing else between the attempts; the third
    # re-resolve answered nothing, so the answer ends there — quietly.
    assert answer.data == b"ABCABC"
    assert asked == [
        ("https://manifest.example/first.m3u8", 96),
        ("https://manifest.example/second.m3u8", 96),
    ]
    assert app.app.test_client().get("/health").get_json()["live_streams"] == 0


def test_a_hung_up_caller_kills_the_transcode_and_frees_the_counter(monkeypatch):
    monkeypatch.setattr(app, "_resolve_url", one_then_none("https://manifest.example/live.m3u8"))
    # A full 64 KiB chunk first (a blocking read returns only when the whole chunk is
    # there), then the process holds the pipe open producing nothing — mid-read, the
    # exact state a listener's hang-up must interrupt.
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
    assert next(answer.response) == b"y" * 65536
    answer.close()
    # Killed on the spot (a negative return code, not the 10 s exit), and the counter
    # the /health of the shim reports is back to zero.
    assert spawned[0].wait(timeout=5) != 0
    assert app.app.test_client().get("/health").get_json()["live_streams"] == 0
