#!/usr/bin/env bash
# Smoke-test a built AppImage end to end:
#   1. it starts and reports its version (no FUSE needed: --appimage-extract-and-run)
#   2. the binary's glibc requirement
#   3. terminal mode: add a provider, doctor, one-shot chat with a real tool call (mock model)
#   4. Hermes import, profiles with separate chats, MCP
#   5. GUI mode under Xvfb: window appears, a message round-trips, clipboard paste works, screenshot saved
# Usage: packaging/smoke.sh path/to/Mercury.AppImage [screenshot-dir]
set -euo pipefail
APP="$(readlink -f "$1")"
SHOTS="${2:-$(mktemp -d)}"
HERE="$(cd "$(dirname "$0")" && pwd)"
export APPIMAGE_EXTRACT_AND_RUN=1
export MERCURY_HOME="$(mktemp -d)"
WORK="$(mktemp -d)"
PORT=$((20000 + RANDOM % 20000))
pass() { echo "PASS  $*"; }
fail() { echo "FAIL  $*"; exit 1; }

python3 "$HERE/mock_llm.py" "$PORT" & MOCK=$!
trap 'kill $MOCK 2>/dev/null || true; [[ -n "${GUI:-}" ]] && kill $GUI 2>/dev/null || true; [[ -n "${XVFB:-}" ]] && kill $XVFB 2>/dev/null || true' EXIT
sleep 0.5

v="$("$APP" --version)"; [[ "$v" == mercury* ]] && pass "version: $v" || fail "version"

EX="$(mktemp -d)"; (cd "$EX" && "$APP" --appimage-extract usr/bin/mercury >/dev/null)
need="$(objdump -T "$EX/squashfs-root/usr/bin/mercury" | grep -o 'GLIBC_[0-9.]*' | sort -Vu | tail -1)"
pass "highest glibc symbol: $need"
deps="$(objdump -p "$EX/squashfs-root/usr/bin/mercury" | awk '/NEEDED/{print $2}' | tr '\n' ' ')"
pass "dynamic deps: $deps"

"$APP" add-provider "Mock box" "http://127.0.0.1:$PORT/v1" --model hermes-4-14b >/dev/null
"$APP" doctor | tee "$SHOTS/doctor.txt" | grep -q "ok, 3 models" && pass "doctor sees the provider" || fail "doctor"
"$APP" models | grep -q qwen3-coder-30b && pass "model listing" || fail "models"

out="$(cd "$WORK" && touch marker.txt && "$APP" chat -q "What is in this project?" 2>&1)"
echo "$out" > "$SHOTS/chat.txt"
grep -q "ls -la" <<<"$out" && grep -q "^    total" <<<"$out" && pass "one-shot chat ran the terminal tool" || fail "chat: $out"
grep -q "cargo test" <<<"$out" && pass "final answer streamed" || fail "answer"

out="$(cd "$WORK" && mkdir -p build && "$APP" chat -q "delete the build dir" 2>&1)"
[[ -d "$WORK/build" ]] && grep -q "denied" <<<"$out" && pass "dangerous command denied non-interactively" || fail "approval: $out"

"$APP" sessions | grep -q "What is in this project" && pass "session saved and listed" || fail "sessions"
"$APP" sessions --search marker | grep -q "marker" && pass "full-text search" || fail "search"

# Pick up from a (fake) Hermes install, and talk to an MCP server.
H="$(mktemp -d)/.hermes"; mkdir -p "$H/memories" "$H/profiles/coder"
printf 'model:\n  provider: custom\n  default: hermes-4-14b\n  base_url: http://127.0.0.1:%s/v1\nmcp_servers:\n  mock:\n    command: python3\n    args: ["%s"]\n' "$PORT" "$HERE/../tests/fixtures/mock_mcp.py" > "$H/config.yaml"
printf 'Prefers metric units' > "$H/memories/USER.md"
printf 'model:\n  default: Hermes-4-70B\n' > "$H/profiles/coder/config.yaml"
python3 - "$H/state.db" <<'PY'
import sqlite3, sys, time
c = sqlite3.connect(sys.argv[1])
c.executescript("CREATE TABLE sessions (id TEXT PRIMARY KEY, source TEXT, model TEXT, started_at REAL, title TEXT);"
                "CREATE TABLE messages (id INTEGER PRIMARY KEY, session_id TEXT, role TEXT, content TEXT, timestamp REAL);")
c.execute("insert into sessions values ('h1','cli','m',?, 'Hermes chat about telescopes')", (time.time(),))
c.execute("insert into messages(session_id,role,content,timestamp) values ('h1','user','which telescope for planets?',0)")
c.execute("insert into messages(session_id,role,content,timestamp) values ('h1','assistant','A 6-inch Dobsonian.',0)")
c.commit()
PY
"$APP" hermes-profiles "$H" | grep -q "coder" && pass "Hermes profiles listed" || fail "hermes-profiles"
"$APP" import-hermes "$H" --from default | tee "$SHOTS/import.txt" | grep -q "1 conversation" && pass "Hermes profile imported" || fail "import: $(cat "$SHOTS/import.txt")"
"$APP" sessions --search telescope | grep -q "telescopes" && pass "Hermes history searchable in Mercury" || fail "imported search"
"$APP" mcp | tee "$SHOTS/mcp.txt" | grep -q "mcp_mock_echo" && pass "MCP server from Hermes config connects" || fail "mcp: $(cat "$SHOTS/mcp.txt")"
# Profiles: Hermes `coder` becomes its own Mercury profile, with its own chats.
"$APP" import-hermes "$H" --from coder | grep -q "Mercury profile “coder”" && pass "Hermes coder imported as its own profile" || fail "import coder"
"$APP" profile list | grep -q coder && pass "profile list" || fail "profile list"
! "$APP" sessions --search telescope | grep -q telescopes && pass "coder doesn't see default's chats" || fail "chats leaked across profiles"
"$APP" -p default sessions --search telescope | grep -q telescopes && pass "-p default still has them" || fail "-p default"
"$APP" profile use default >/dev/null
"$APP" use mock-box hermes-4-14b >/dev/null

if command -v Xvfb >/dev/null && python3 -c "import Xlib" 2>/dev/null; then
    DISP=":$((90 + RANDOM % 9))"
    Xvfb "$DISP" -screen 0 1280x860x24 >/dev/null 2>&1 & XVFB=$!
    sleep 1
    (cd "$WORK" && DISPLAY="$DISP" "$APP" >"$SHOTS/gui.log" 2>&1) & GUI=$!
    sleep 5
    kill -0 $GUI 2>/dev/null && pass "GUI started" || fail "GUI exited: $(cat "$SHOTS/gui.log")"
    DISPLAY="$DISP" python3 "$HERE/xdrive.py" focus Mercury click 650 700 sleep 0.4 type "What is in this project?" key Return sleep 4
    DISPLAY="$DISP" import -window root -crop 1180x780+0+0 "$SHOTS/gui.png"
    sessions="$("$APP" sessions | grep -c "What is in this project" || true)"
    [[ "$sessions" -ge 2 ]] && pass "GUI round trip saved a second session" || fail "GUI chat did not complete"
    # Paste through the real X11 clipboard (v0.2 shipped without clipboard support).
    DISPLAY="$DISP" python3 "$HERE/xclipserve.py" "pasted zebra42 from the clipboard" 10 >"$SHOTS/clip.log" & CLIP=$!
    sleep 0.5
    DISPLAY="$DISP" python3 "$HERE/xdrive.py" focus Mercury click 650 700 sleep 0.3 ctrl v sleep 0.5 key Return sleep 4
    kill $CLIP 2>/dev/null || true
    "$APP" sessions --search zebra42 | grep -q zebra42 && pass "clipboard paste reaches the app" || fail "paste: $(cat "$SHOTS/clip.log")"
    kill $GUI; sleep 0.5
    # A custom logo and a bigger text size, as a user would set them.
    cp "$HERE/mercury.png" "$MERCURY_HOME/custom-logo.png"
    sed -i "s|^logo = .*|logo = \"custom\"\ncustom_logo = \"$MERCURY_HOME/custom-logo.png\"|; s/^font_scale = .*/font_scale = 1.3/" "$MERCURY_HOME/config.toml"
    (cd "$WORK" && DISPLAY="$DISP" "$APP" >"$SHOTS/gui2.log" 2>&1) & GUI=$!
    sleep 5
    DISPLAY="$DISP" import -window root -crop 1180x780+0+0 "$SHOTS/gui-custom-logo.png"
    kill -0 $GUI 2>/dev/null && pass "GUI restarts with a custom logo at 130%" || fail "GUI restart"
    kill $GUI
else
    echo "SKIP  GUI (needs Xvfb + python-xlib)"
fi
echo "all smoke tests passed; artefacts in $SHOTS"
