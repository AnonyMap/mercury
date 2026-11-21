# Mercury — plan

Mercury is a Rust re-implementation of the *client* side of Nous Research's Hermes Agent: one
self-contained AppImage, no Python/uv/Node install step, a desktop GUI as the primary surface and
a terminal mode for SSH/headless use. It speaks two backends: Nous Portal (OAuth device-code login,
same endpoints Hermes uses) and any OpenAI-compatible `/v1/chat/completions` server, with the local
machine and LAN treated as the first-class case rather than an afterthought.

## 1. What "emulate most of Hermes" means here

Hermes today is a very large Python system (CLI, TUI, desktop app, messaging gateway for a dozen
chat platforms, cron, browser automation over CDP, RL trajectory tooling, kanban, plugins, MCP).
Re-implementing all of it would be a rewrite of a 200k-star project, not a client. Mercury takes the
part a single user touches every day — the agent loop and its core tools — and drops the
infrastructure that only makes sense on a long-running server.

| Hermes feature | Mercury | Notes |
|---|---|---|
| Agent loop with native tool calling, iteration cap (90) | **Yes** | Streaming; reasoning (`reasoning_content` / `<think>`) shown separately |
| `terminal` tool with dangerous-command approval | **Yes** | Persistent working dir per session; ask / yolo / deny modes |
| `read_file`, `write_file`, `patch`, `search_files` | **Yes** | `patch` = unique find/replace with whitespace-tolerant fallback; search respects `.gitignore` |
| `web_extract` | **Yes** | HTML → text, truncated |
| `web_search` | **Partial** | SearXNG (self-hosted, LAN-friendly) or Brave API key. Nous Tool Gateway not implemented |
| `memory` (MEMORY.md / USER.md, `§` delimited, char caps, frozen snapshot per session) | **Yes** | Same file format; can point at `~/.hermes/memories` |
| Skills (`SKILL.md`, progressive disclosure, `skills_list` / `skill_view` / `skill_manage`, `/<skill>`) | **Yes** | Reads Hermes skill folders as-is |
| `todo_list`, `session_search`, `clarify` | **Yes / Yes / via chat** | Sessions in SQLite with FTS5 |
| Slash commands: `/new /reset /model /retry /undo /compress /usage /skills /personality /stop /help /title /yolo /sessions /search` | **Yes** | Same names |
| Context files (AGENTS.md, SOUL.md, .hermes.md) | **Yes** | |
| Context compression (auto + `/compress`) | **Yes** | Summarise middle turns with the active model |
| Nous Portal OAuth (device code), token refresh, inference URL allowlist | **Yes** | `client_id` configurable — see risk R1 |
| Nous Portal API key | **Yes** | Preset provider |
| Provider switching without restart | **Yes** | Model picker in the top bar + `/model provider:model` |
| Import existing Hermes setup | **Yes** | config.yaml model/provider, `.env` keys, memories, skills path |
| MCP client (stdio + streamable HTTP) | **Yes (v0.2)** | Hermes `mcp_servers` format, imported from profiles |
| `delegate_task` subagents | **Yes (v0.2)** | ≤3 parallel, fresh context, depth 1, approvals forwarded |
| `clarify`, `process_manage`, checkpoints + `/rollback`, `/insights` | **Yes (v0.2)** | Checkpoints are file snapshots, not Hermes's shadow git repo |
| Profiles: pick up from an existing Hermes install | **Yes (v0.2)** | Copy or in-place; history imported read-only from `state.db` |
| Profiles (`-p`, `profile list/use/create/delete`, separate chats/memory/skills per profile) | **Yes (v0.3)** | Providers shared across profiles |
| Messaging gateway (Telegram/Discord/Slack/…), cron, browser/CDP, computer use, image gen, TTS, execute_code sandbox, kanban, plugins, RL/trajectory tools | **No** | Server-side or very large surfaces |

## 2. Ease of install and navigation (the actual reason to build this)

* **Install** = download one `Mercury-x86_64.AppImage`, `chmod +x`, run. TLS is rustls with the
  system CA store, SQLite is statically bundled, GL/X11/Wayland are `dlopen`ed from the host, so the
  binary depends only on glibc.
* **First run** shows a three-way onboarding card instead of a wizard: *Find local models*
  (probes Ollama :11434, LM Studio :1234, llama.cpp :8080, vLLM :8000, KoboldCpp :5001, Jan :1337),
  *Sign in to Nous Portal* (shows the code + opens the browser), *Add endpoint*. If `~/.hermes`
  exists, an *Import from Hermes* button appears.
* **LAN scan**: opt-in sweep of the local /24 on the same ports, so a GPU box in the closet is one
  click away.
* **Navigation**: sessions sidebar with full-text search, provider/model picker always visible, tool
  calls rendered as collapsible cards, approvals as an inline banner (not a modal), settings as tabs.
* **Terminal mode** for SSH: `mercury chat`, `mercury login nous`, `mercury discover --lan`,
  `mercury models`, `mercury doctor`, `mercury import-hermes`, `mercury sessions`. With no display,
  plain `mercury` falls back to the terminal REPL.

## 3. Architecture

Single crate, library + binary. Core is UI-agnostic so it's testable without a display.

```
src/
  config.rs      paths (XDG, MERCURY_HOME override), TOML config, provider presets
  provider.rs    OpenAI-compatible client: /models, streaming SSE chat with tool_call deltas,
                 reasoning extraction, usage, stream_options fallback
  nous.rs        device-code flow (RFC 8628 + Vercel edge back-off), refresh via
                 x-nous-refresh-token, inference host allowlist, 0600 token store
  discovery.rs   localhost + LAN probing of known OpenAI-compatible ports
  hermes_import  read ~/.hermes config.yaml / .env / memories / skills
  agent.rs       loop, event stream, cancellation, approvals, compression, system prompt build
  approval.rs    dangerous-command detection
  session.rs     SQLite sessions/messages + FTS5
  memory.rs      MEMORY.md / USER.md store
  skills.rs      SKILL.md discovery, frontmatter parsing, create/edit/delete
  commands.rs    slash command parser
  tools/         terminal, files, search, web, memory, todo, skills, session_search
  gui/           eframe/egui front end
  cli.rs         subcommands + REPL
```

The agent runs on a Tokio runtime and emits `AgentEvent`s over a channel; the GUI and REPL are two
consumers of the same stream. Approvals are a request/response pair on that channel, so the loop
blocks only the tool, not the UI.

## 4. Test plan

| Layer | What is tested | How |
|---|---|---|
| Unit | config round-trip, presets, path override; SSE parser incl. split chunks, `[DONE]`, tool-call delta assembly, `<think>` splitting; approval patterns (positive and negative cases); memory add/replace/remove/limits/delimiter; skill frontmatter parsing; slash-command parsing; patch semantics; truncation; Hermes config/.env import | `cargo test` |
| Integration (mock servers) | streaming chat; full tool-call round trip (model asks for `write_file`, file appears, result is sent back, final answer); approval deny path; cancellation; `/models` listing; `stream_options` 400 fallback; Nous device flow happy path, `authorization_pending` → success, `slow_down`, `access_denied`, refresh with header, allowlist rejection of a hostile `inference_base_url`; local discovery against a fake Ollama port | axum servers on ephemeral ports in `tests/` |
| Storage | session create/append/list/rename/delete, FTS search, undo | temp `MERCURY_HOME` |
| GUI | onboarding renders when no provider; model picker lists models; sending a message renders the streamed reply; approval banner approve/deny; settings tabs reachable; slash `/new` clears | `egui_kittest` headless harness (AccessKit queries, no GPU) |
| Packaging | AppImage runs `--version`, `doctor`, one-shot chat against a mock server; GUI starts under Xvfb and renders (screenshot) | shell script `packaging/smoke.sh` |
| Hygiene | `cargo clippy -- -D warnings`, `cargo fmt --check` | |

## 5. Risks and open questions

* **R1 — OAuth client identity.** Nous Portal's device flow is keyed to a registered `client_id`; the
  only public one is Hermes's own (`hermes-cli`). Mercury uses it by default so login works today,
  but that is Mercury presenting itself as Hermes. It's configurable (`nous.client_id`); the right fix
  is asking Nous for a Mercury client id before any public release.
* **R2 — Portal edge firewall.** Hermes's issue tracker documents Vercel WAF challenges
  (`x-vercel-mitigated`) and a device-approval page that sometimes never approves. Mercury backs off
  on edge responses instead of failing, and offers the API-key path as the fallback.
* **R3 — Local models and tools.** Many small local models emit malformed or no tool calls.
  Per-provider `tools = false` turns Mercury into a plain chat client for those, and malformed
  tool-call JSON is returned to the model as an error rather than crashing the turn.
* **R4 — glibc floor.** The AppImage is only as portable as the glibc it was linked against. Build
  in the oldest supported distro container (or with `cargo zigbuild --target
  x86_64-unknown-linux-gnu.2.31`) for releases.
* **R5 — Token refresh rotation.** Mercury keeps its own Nous tokens instead of reusing
  `~/.hermes/auth.json`, because if refresh tokens rotate, two clients refreshing one grant would log
  each other out.

## 6. Milestones

1. Core provider layer + Nous auth + discovery, with tests.
2. Agent loop, tools, storage, commands, with tests.
3. GUI and terminal front ends, harness tests.
4. AppImage packaging + smoke tests.
5. (v2) MCP client, `delegate_task`, cron, aarch64 AppImage, Nous Tool Gateway.

## 7. Status (v0.1.0, 2026-11-06)

Built and verified:

* **102 automated tests, all passing** — 66 unit, 16 agent end-to-end, 8 Nous OAuth, 3 discovery/web,
  9 headless GUI (egui_kittest). `cargo clippy --all-targets` and `cargo fmt --check` clean.
* **AppImage smoke test (`packaging/smoke.sh`), 12/12 passing** against the packaged artifact: version,
  glibc floor (highest symbol GLIBC_2.30, only glibc libraries linked), doctor, model listing,
  one-shot chat with a real tool call, non-interactive denial of a dangerous command, session
  persistence + full-text search, GUI launch under Xvfb and a full GUI chat round-trip.
* AppImage size 11 MB (27 MB binary uncompressed).

Bugs the GUI tests caught and that were fixed: combo boxes without accessible names, missing glyphs
(icons rendered as boxes), toasts overlapping the Send/Back buttons and swallowing clicks, and
duplicate widget IDs when two replies contained identical markdown tables.

Not verified here: a live Nous Portal sign-in (needs a real account) and real local inference
servers (all model traffic in tests is a scripted mock that speaks the same wire format).

## 8. v0.2 (2026-11-06)

User feedback: more Hermes features, pick up from an existing Hermes install, text too small, a way to
change the logo.

* **Hermes adoption.** Mercury reads Hermes's own layout: a profile is any directory with one of
  `config.yaml`, `.env`, `SOUL.md`, `profile.yaml`, `auth.json`, `state.db` (Hermes's rule), the root is
  `default`, others live in `profiles/<name>`. History comes from `state.db` (`sessions`/`messages`,
  only `active = 1` rows, hidden sessions skipped), columns detected at runtime so older/newer Hermes
  schemas both work; transcripts are repaired (orphan tool results dropped, unanswered tool calls given
  placeholder results) so they can be continued. Imports are tagged with their origin and are idempotent.
* **In-place vs copy.** Hermes's docs warn against two agents writing one profile at once; in-place is
  offered for switching over, copy for running both.
* **Nous OAuth is not shared** — single-use refresh tokens (see README).
* **Text size**: base type ~24% larger, auto-sized from monitor width on first run (1.0 at 1920 pt,
  1.75 at 3840), controls in the top bar, shortcuts, persisted; egui's own keyboard zoom is disabled so
  it can't drift from the saved value.
* **Custom logo**: any image the user owns; copied into Mercury's data folder as a square PNG (so it
  survives the original moving), shown in the top bar, the empty-chat screen and as the window icon.
* Tests: 127 automated (was 102) + extended AppImage smoke test.

## 9. v0.3 (2026-11-08)

User feedback: choose the profile from a dropdown; pasting a path into the Hermes field didn't work
(and a file browser would be better); label local providers (there may be several); chats should be
separate per profile.

* **Clipboard was off everywhere.** Mercury builds eframe with `default-features = false` (to pick the
  GL backend), which also dropped its `clipboard` feature, so Ctrl+V/Ctrl+C silently did nothing in
  every text box, and `links` was dropped too, so links in replies didn't open. Both features are back. The smoke
  test now pastes through a real X11 clipboard owner (`packaging/xclipserve.py`); run against the v0.2
  AppImage it fails, against v0.3 it passes.
* **Paths as people paste them**: surrounding quotes, `file://` URIs (drag from a file manager, with
  `%20` decoding) and `~` are accepted in the Hermes folder, logo and working-directory fields.
* **Browse…** next to every path field: the system dialog via the XDG desktop portal (or zenity),
  falling back to a built-in folder browser when the desktop has neither. A "cancel" that comes back
  in under 250 ms is treated as "no dialog available". The built-in browser shows hidden folders
  when looking for `~/.hermes`.
* **Mercury profiles**, modelled on Hermes's: `default` is the data-dir root (so v0.2 data is the
  default profile with no migration); others live in `profiles/<name>/` with their own `sessions.db`,
  memories, skills, SOUL.md, checkpoints and `profile.toml` (model, personality, MCP servers, Hermes
  link). Providers, Nous sign-in, appearance and tool settings stay shared in `config.toml`. Sidebar
  dropdown to switch or create; Settings > Profiles & Hermes to open/delete (deleting asks first and
  says how many chats go with it); `-p NAME`, `MERCURY_PROFILE`, `mercury profile …` in the terminal.
  Switching saves the current window's settings first, so a provider labelled in one profile shows up
  in the others.
* **Hermes import by dropdown**: pick the Hermes profile, then the Mercury profile it goes into (same
  name by default, created if needed). Each Hermes profile becomes its own Mercury profile with its own
  chats, so importing `coder` no longer mixes its history into `default`.
* **Provider labels**: an editable Label per provider (Settings > Models & providers, or
  `mercury label-provider ID LABEL`); the picker shows `label · local/LAN/cloud · host:port`.
* Accessibility: every text field now has a screen-reader name (the GUI tests fail on unnamed widgets).
* Tests: 136 automated (was 127) + AppImage smoke test 22/22 (was 17).
