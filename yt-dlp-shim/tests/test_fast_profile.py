"""The stream-resolve fast profile keeps its skips but never skips the watch page (SOAK-1b).

player_skip=webpage starves yt-dlp of the challenge/consent context YouTube needs before it
hands out formats, so every resolve hit the "confirm you're not a bot" wall. These tests pin
that the resolve run never carries it again, that the remaining skip segments stay, and that
ytsearch* runs keep their plain, fast-args-free shape.
"""
import json
import os
import sys

import pytest

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

app_module = pytest.importorskip("app")

client = app_module.app.test_client()

# What a successful resolve run prints: the FORMAT_PRINT line and the URL (-g's line).
FORMAT_LINE = "140|5689421|m4a|130|NA"
URL = "https://rr3---sn-1.googlevideo.com/videoplayback?id=o-A"

ROWS = [{"id": "killy", "title": "KILLY - Killamonjaro", "duration": 161, "channel": "KILLY"}]


@pytest.fixture
def runs(monkeypatch):
    """Records every yt-dlp run's arguments; resolves answer a URL, searches answer rows."""
    with app_module._URL_CACHE_LOCK:
        app_module._URL_CACHE.clear()
    with app_module._SEARCH_CACHE_LOCK:
        app_module._SEARCH_CACHE.clear()
    state = {"args": []}

    def fake_run(args, *a, **k):
        state["args"].append(list(args))
        asked = " ".join(args)
        if "ytsearch" in asked:
            return "\n".join(json.dumps(r) for r in ROWS)
        if "watch?v=" in asked:
            return f"{FORMAT_LINE}\n{URL}"
        return ""

    monkeypatch.setattr(app_module, "_run", fake_run)
    yield state
    with app_module._URL_CACHE_LOCK:
        app_module._URL_CACHE.clear()
    with app_module._SEARCH_CACHE_LOCK:
        app_module._SEARCH_CACHE.clear()


def _extractor_args(state):
    """Every --extractor-args value the recorded runs carried."""
    values = []
    for args in state["args"]:
        for i, arg in enumerate(args):
            if arg == "--extractor-args" and i + 1 < len(args):
                values.append(args[i + 1])
    return values


def test_the_resolve_run_never_skips_the_watch_page(runs):
    assert app_module._resolve_url("killy", bg=False) == URL
    values = _extractor_args(runs)
    assert values, "the resolve run must carry its fast --extractor-args"
    assert all("player_skip=webpage" not in v and "webpage" not in v for v in values), values


def test_the_resolve_run_keeps_the_other_skip_segments(runs):
    assert app_module._resolve_url("killy", bg=False) == URL
    values = _extractor_args(runs)
    assert values
    for segment in ("player_skip=configs,initial_data", "skip=translated_subs,dash"):
        assert all(segment in v for v in values), (segment, values)


def test_the_resolve_run_carries_the_extractor_args_in_one_argument(runs):
    assert app_module._resolve_url("killy", bg=False) == URL
    resolve_runs = [a for a in runs["args"] if "watch?v=killy" in " ".join(a)]
    assert resolve_runs
    for args in resolve_runs:
        joined = " ".join(args)
        assert "--extractor-args youtube:" in joined


def test_the_fallback_run_after_a_bad_player_client_also_keeps_the_page(runs, monkeypatch):
    # The client run gives no URL, so the -default fallback run fires; it must carry the
    # skips but still fetch the page.
    state = {"client_runs": 0}

    def fake_run(args, *a, **k):
        runs["args"].append(list(args))
        asked = " ".join(args)
        if "ytsearch" in asked:
            return "\n".join(json.dumps(r) for r in ROWS)
        if "player_client" in asked:
            state["client_runs"] += 1
            return ""  # the client run never answers a URL
        return f"{FORMAT_LINE}\n{URL}"

    monkeypatch.setattr(app_module, "_run", fake_run)
    assert app_module._resolve_url("killy", bg=False) == URL
    assert state["client_runs"] >= 1
    values = _extractor_args(runs)
    assert values
    assert all("webpage" not in v for v in values), values


def test_ytsearch_runs_carry_no_fast_extractor_args(runs):
    app_module._resolve_url("killy", bg=False)  # a resolve first, so both shapes are recorded
    answer = client.get("/search?q=KILLY Killamonjaro&duration=161").get_json()
    assert answer["video_id"] == "killy"
    for args in runs["args"]:
        if any("ytsearch" in a for a in args):
            assert "--extractor-args" not in args, args
