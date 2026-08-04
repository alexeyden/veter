#!/usr/bin/env python3
"""
Claude Code Stop hook: display images a response referred to, inline.

Claude Code has no image output of its own in the terminal, and cannot
be taught any -- but it does not need to be. It only has to leave two
things behind in its own output:

    [IMAGE: <id>] <path-to-image>
    ```
    <blank lines -- the reserved gap>
    ```

The marker names a location; the fenced block reserves the rows. Both
are ordinary parts of the message, so Claude Code's renderer accounts
for them and stays consistent -- which is the whole trick. An outside
process must never inject the blank rows itself: a TUI dead-reckons its
frame position, so scrolling underneath it strands the previous frame.

The marker line stays visible: the image lands on the row *below* it, so
the line reads as the image's caption. Anything after the path is
caption prose and is ignored here -- `[IMAGE: q3] /tmp/q3.png -- revenue
by quarter` names the same file as the bare form.

The fence matters. Plain blank lines are collapsed to one by the
markdown renderer; a fenced code block's blank body survives verbatim
and renders with no border or background.

This hook reads the finished response, finds those markers, and hands
each one to `vplace`, which anchors an image to the marker string.

A hook is the right place to do this, and not only for convenience: the
terminal anchors to the *most recent* row containing the marker, so the
string must not appear on screen below the line it names. A hook runs
as a silent subprocess and never echoes anything. Running `vplace` by
hand in an interactive shell does echo it -- the command line becomes
the newest match and the image anchors to the command instead of to the
message. Use `vplace --marker-file` when testing manually.

Install (~/.claude/settings.json):

    { "hooks": { "Stop": [ { "hooks": [ {
        "type": "command",
        "command": "/path/to/claude-code-hook.py"
    } ] } ] } }

Test without Claude Code:

    ./claude-code-hook.py --transcript some.jsonl
"""
import argparse
import datetime
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time

# Diagnostics. A Stop hook runs detached from the UI, so a wrong answer
# is otherwise invisible -- the only evidence is where the image landed.
# On by default while this is a prototype (one short line per firing);
# set VPLACE_HOOK_LOG="" to silence it.
# /tmp rather than ~/.claude: a hook may run sandboxed, and a denied
# write to the log would look exactly like a hook that never fired.
LOG = os.environ.get("VPLACE_HOOK_LOG", "/tmp/vplace-hook.log")


def log(msg):
    if not LOG:
        return
    try:
        with open(LOG, "a", encoding="utf8") as f:
            f.write(f"{datetime.datetime.now().isoformat(timespec='seconds')} {msg}\n")
    except OSError:
        pass

# Short, ASCII, line-leading by convention: the terminal matches this
# against a single grid row, so a marker long enough to wrap could
# never be found. Only `[IMAGE: <id>]` is handed to vplace as the
# anchor -- the path and any caption prose after it are free to be as
# long as they like, and to wrap.
#
# The id is what keeps the readable form safe. The terminal anchors to
# the bottom-most matching row, so a bare `[IMAGE]` would collide with
# any prose mentioning it; `[IMAGE: q3]` collides only with itself.
MARKER_RE = re.compile(r"\[IMAGE:[ \t]*([A-Za-z0-9_.-]{1,32})\][ \t]+(\S+)")

# The fenced block immediately following a marker: its blank body is the
# space that was actually reserved.
# `match(text, pos)` already anchors at pos, so no \A here -- \A would
# mean "start of string" and never match after the marker.
FENCE_RE = re.compile(r"[ \t]*\n[ \t]*(?:\n[ \t]*)?```[^\n]*\n(.*?)\n?```", re.S)


def reserved_rows(text, marker_end):
    """Rows the message actually reserved after a marker.

    Returns the row count; 0 if a gap was written but renders as no
    rows at all; None if there is no gap to read.

    The instruction tells Claude to size the gap from `vplace --measure`,
    but that is a counting step done by hand, and getting it wrong draws
    the image over the text below. Reading the gap back out of the
    message makes the placement self-correcting: whatever was reserved
    is the ceiling, so a miscount costs a smaller image instead of
    covering someone's paragraph.

    A fence with N blank lines occupies N rows on screen, the first of
    them directly under the marker line -- which is where vplace's
    default `--offset-y 1` starts drawing. So the blank-line count is
    the image's row budget, one for one.
    """
    # Measure from the end of the marker *line*, not the end of the
    # regex match: the match stops after the path, and the line may
    # carry caption prose past it. The fence begins at the next break.
    eol = text.find("\n", marker_end)
    if eol < 0:
        return None
    m = FENCE_RE.match(text, eol)
    if not m:
        return None
    body = m.group(1)
    if body.strip():
        return None  # a real code block, not a reserved gap
    if not text[m.end():].strip():
        # A gap that ends the message reserves nothing. The renderer
        # trims the message's trailing whitespace, which for a body of
        # nothing but blank lines empties the block, and an empty block
        # draws no rows. This is the one place where the message text
        # and the screen disagree, so the count cannot come from the
        # body -- counting it drew a full-height image over the prompt.
        return 0
    return body.count("\n") + 1

# Images are decorative; a hook that fails must never break the
# session. Every failure path below exits 0.
OK = 0


def _size(path):
    try:
        return os.path.getsize(path)
    except OSError:
        return -1


def wait_for_final_block(path, grow_timeout=2.0, settle=0.3, settle_max=1.5, step=0.1):
    """Wait for the turn's final assistant block to reach the transcript.

    A Stop hook fires *before* that block is written, so reading
    immediately yields the previous one -- which is exactly how an image
    ends up anchored to the previous turn's marker.

    Two phases, because either predicate alone is wrong: waiting only
    for the file to stop growing returns instantly (it has not started
    growing yet), and waiting only for growth can catch a half-written
    record. So: wait for the file to grow, then for it to settle.

    `grow_timeout` is deliberately short: most turns carry no marker,
    and every one of them pays this wait. If a write lands later than
    that we return "no-growth" and the marker is picked up by the next
    firing instead -- late, but the dedupe file keeps it from being
    drawn twice.

    Returns a short status string for the log.
    """
    start = _size(path)
    deadline = time.time() + grow_timeout
    while time.time() < deadline:
        if _size(path) > start:
            break
        time.sleep(step)
    else:
        return "no-growth"

    deadline = time.time() + settle_max
    last, stable_since = -1, None
    while time.time() < deadline:
        cur = _size(path)
        if cur != last:
            last, stable_since = cur, time.time()
        elif time.time() - stable_since >= settle:
            return "settled"
        time.sleep(step)
    return "settle-timeout"


def placed_path(session_id):
    """State file recording markers already drawn this session."""
    safe = re.sub(r"[^A-Za-z0-9_.-]", "_", session_id or "nosession")
    return os.path.join(tempfile.gettempdir(), f"vplace-placed-{safe}")


def load_placed(session_id):
    try:
        with open(placed_path(session_id), encoding="utf8") as f:
            return {ln.strip() for ln in f if ln.strip()}
    except OSError:
        return set()


def remember_placed(session_id, ident):
    try:
        with open(placed_path(session_id), "a", encoding="utf8") as f:
            f.write(ident + "\n")
    except OSError:
        pass


def last_assistant_text(transcript_path):
    """Concatenated text of the final assistant turn, or ''."""
    try:
        with open(transcript_path, "r", encoding="utf8", errors="replace") as f:
            lines = f.readlines()
    except OSError:
        return ""

    for line in reversed(lines):
        line = line.strip()
        if not line:
            continue
        try:
            rec = json.loads(line)
        except json.JSONDecodeError:
            continue
        if rec.get("type") != "assistant":
            continue
        msg = rec.get("message") or {}
        content = msg.get("content")
        if isinstance(content, str):
            return content
        if isinstance(content, list):
            parts = [
                c.get("text", "")
                for c in content
                if isinstance(c, dict) and c.get("type") == "text"
            ]
            if parts:
                return "\n".join(parts)
    return ""


# (label, message text, expected reserved rows). Run with --self-test.
# The gap parser is the only real logic in this file and it silently
# degrades when wrong -- an undersized clamp shrinks an image, a missed
# one lets it cover text -- so it is worth pinning.
_SELF_TESTS = [
    ("correct 5-line gap", "[IMAGE: a] /x.png\n```\n\n\n\n\n\n```\nafter", 5),
    ("undersized gap", "[IMAGE: a] /x.png\n```\n\n\n```\nafter", 2),
    ("blank line before fence", "[IMAGE: a] /x.png\n\n```\n\n\n\n```\nafter", 3),
    ("language-tagged fence", "[IMAGE: a] /x.png\n```text\n\n\n\n```\nafter", 3),
    ("no fence", "[IMAGE: a] /x.png\nplain text", None),
    ("real code block", "[IMAGE: a] /x.png\n```\nfn main() {}\n```\n", None),
    # A gap the renderer trims away. Reads as a correct message and
    # parses as one; the rows are simply not on screen.
    ("gap ends the message", "[IMAGE: a] /x.png\n```\n\n\n\n\n\n```", 0),
    ("gap ends the message, trailing newline", "[IMAGE: a] /x.png\n```\n\n\n```\n", 0),
    # The caption form: prose after the path must not hide the fence.
    (
        "caption prose after the path",
        "[IMAGE: a] /x.png -- revenue by quarter\n```\n\n\n\n\n\n```\nafter",
        5,
    ),
]

# (label, message text, expected (id, path) or None). The marker syntax
# is a contract with the instruction text in ~/.claude/CLAUDE.md, so a
# silent regex drift is a silent loss of every image.
_MARKER_TESTS = [
    ("canonical", "[IMAGE: chart1] /tmp/c.png", ("chart1", "/tmp/c.png")),
    ("no space after colon", "[IMAGE:chart1] /tmp/c.png", ("chart1", "/tmp/c.png")),
    ("caption prose", "[IMAGE: q3] /tmp/q3.png -- by quarter", ("q3", "/tmp/q3.png")),
    ("relative path", "[IMAGE: r] out/plot.png", ("r", "out/plot.png")),
    ("no id", "[IMAGE] /tmp/c.png", None),
    ("no path", "[IMAGE: chart1]", None),
]


def self_test():
    bad = 0
    for label, text, expect in _SELF_TESTS:
        m = MARKER_RE.search(text)
        got = reserved_rows(text, m.end()) if m else "no-marker"
        if got != expect:
            bad += 1
            print(f"FAIL gap/{label}: got {got!r}, expected {expect!r}")
    for label, text, expect in _MARKER_TESTS:
        m = MARKER_RE.search(text)
        got = (m.group(1), m.group(2)) if m else None
        if got != expect:
            bad += 1
            print(f"FAIL marker/{label}: got {got!r}, expected {expect!r}")
    total = len(_SELF_TESTS) + len(_MARKER_TESTS)
    print(f"{total - bad}/{total} passed")
    return 1 if bad else 0


def find_vplace():
    """Prefer a vplace sitting next to this script, then $PATH."""
    here = os.path.join(os.path.dirname(os.path.abspath(__file__)), "vplace")
    if os.path.isfile(here) and os.access(here, os.X_OK):
        return here
    return shutil.which("vplace")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--transcript", help="bypass stdin; for manual testing")
    ap.add_argument("--dry-run", action="store_true")
    ap.add_argument("--self-test", action="store_true",
                    help="check the gap parser and exit")
    a = ap.parse_args()

    if a.self_test:
        return self_test()

    cwd = os.getcwd()
    session = ""
    if a.transcript:
        transcript = a.transcript
    else:
        try:
            payload = json.load(sys.stdin)
        except (json.JSONDecodeError, ValueError):
            return OK
        transcript = payload.get("transcript_path")
        cwd = payload.get("cwd") or cwd
        session = payload.get("session_id") or ""
        if not transcript:
            return OK
        # The turn's final block is still being written when we fire.
        log(f"wait={wait_for_final_block(transcript)}")

    text = last_assistant_text(transcript)
    # Log unconditionally, before any early return: "fired but found
    # nothing" and "never fired" are different diagnoses and must not
    # look identical in the log.
    log(f"fired: transcript={transcript} textlen={len(text)} head={text[:60]!r}")
    if not text:
        return OK

    # Never redraw a marker already placed this session. Without this,
    # a firing that reads a stale block re-places an older turn's image
    # — the "one turn behind" failure, repeated.
    already = load_placed(session)
    seen = set()
    found = []
    for m in MARKER_RE.finditer(text):
        ident, path = m.group(1), m.group(2)
        if ident in seen or ident in already:
            continue
        seen.add(ident)
        rows = reserved_rows(text, m.end())
        if rows == 0:
            # The gap was written but ends the message, so it rendered
            # as nothing and there is no room to draw into. Nothing can
            # be done from out here -- space is reserved in-band or not
            # at all -- so degrade to the caption line, which still
            # names the file. Drawing anyway covers whatever the
            # renderer put below, up to and including the prompt.
            log(f"  {ident}: SKIP, gap fence ends the message (reserves no rows)")
            continue
        # Markers inside the message are relative to where the session
        # is running, matching how Claude writes paths.
        resolved = path if os.path.isabs(path) else os.path.join(cwd, path)
        if os.path.isfile(resolved):
            found.append((ident, resolved, rows))

    log(f"  markers={[(i, r) for i, _, r in found]}")
    if not found:
        return OK

    vplace = find_vplace()
    if not vplace:
        log("  ERROR: vplace not found")
        print("vplace-hook: vplace not found on PATH", file=sys.stderr)
        return OK
    for ident, path, rows in found:
        # --offset-y is vplace's default (1, i.e. the row under the
        # caption), calibrated to this renderer. --offset-x 2 matches
        # the indent Claude Code renders message bodies at, so the
        # image lines up with the text above it.
        cmd = [vplace, path, "--marker", f"[IMAGE: {ident}]", "--offset-x", "2"]
        # Clamp to the gap the message actually reserved. A miscounted
        # fence then costs a smaller image, not a paragraph drawn over.
        if rows:
            cmd += ["--max-rows", str(rows)]
        if a.dry_run:
            print(" ".join(cmd))
            continue
        try:
            r = subprocess.run(cmd, capture_output=True, timeout=20)
            log(
                f"  {ident}: rc={r.returncode} "
                f"out={r.stdout.decode('utf8', 'replace').strip()!r} "
                f"err={r.stderr.decode('utf8', 'replace').strip()!r}"
            )
            if r.returncode == 0:
                remember_placed(session, ident)
            else:
                print(
                    f"vplace-hook: {ident}: {r.stderr.decode('utf8', 'replace').strip()}",
                    file=sys.stderr,
                )
        except (OSError, subprocess.SubprocessError) as e:
            log(f"  {ident}: EXCEPTION {e}")
            print(f"vplace-hook: {ident}: {e}", file=sys.stderr)
    return OK


if __name__ == "__main__":
    sys.exit(main())
