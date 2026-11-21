# Mercury

A desktop agent client in Rust that does the day-to-day job of [Hermes Agent](https://github.com/NousResearch/hermes-agent)
— chat with a model that can run commands, edit files, remember things and learn skills — shipped as
one AppImage. No Python, uv, Node or install script.

It talks to **Nous Portal** (same OAuth device-code sign-in Hermes uses) and to **any OpenAI-compatible
server**, with local machines and the LAN treated as the main case: Mercury finds Ollama, LM Studio,
llama.cpp, vLLM, KoboldCpp and Jan on their usual ports, and can sweep your /24 for a GPU box.

## Install

```bash
chmod +x Mercury-0.3.0-x86_64.AppImage
./Mercury-0.3.0-x86_64.AppImage            # desktop app
./Mercury-0.3.0-x86_64.AppImage chat       # terminal mode (also the default with no display, e.g. over SSH)
```

Needs glibc 2.31+ (Ubuntu 20.04, Debian 11, Fedora 32 and later) and FUSE, like any AppImage. Without
FUSE, run it with `--appimage-extract-and-run`.

## What it does

| | |
|---|---|
| Providers | Nous Portal (OAuth device code, auto-refresh), Nous API key, any `/v1/chat/completions` endpoint; switch from the top bar or `/model provider:model`. Give each one a label ("Basement GPU", "Laptop Ollama") — the picker shows label · kind · host:port |
| Local first | One-click discovery on this machine and the LAN; per-provider "tools off" for models without function calling, and automatic fallback to plain chat when a server says it can't do tools |
| Agent loop | Streaming, native tool calls, reasoning shown separately (`reasoning_content` or `<think>`), 90-iteration cap, Stop at any time |
| Tools | `terminal` (cwd persists, background jobs), `read_file`, `write_file`, `patch`, `search_files`, `web_extract`, `web_search` (SearXNG or Brave), `memory`, `todo_list`, `skills_list` / `skill_view` / `skill_manage`, `session_search` |
| Safety | Dangerous commands (rm -r, sudo, curl \| sh, force-push, disk writes…) need approval; Ask / Deny / YOLO |
| Memory | `MEMORY.md` + `USER.md`, `§`-delimited, char-capped, frozen per session — the Hermes format |
| Skills | `SKILL.md` folders (Hermes / agentskills.io format); run with `/<skill-name>`; Hermes's skills folder can be linked read-only |
| Sessions | SQLite with full-text search; sidebar, rename, delete; auto-compression of long contexts and `/compress` |
| Commands | `/new /retry /undo /compress /usage /insights /model /provider /personality /title /skills /sessions /search /memory /tools /mcp /rollback /cwd /yolo /stop /help` |
| Context | Hermes's rule: first of `.hermes.md` / `HERMES.md` / `AGENTS.md` / `CLAUDE.md` / `.cursorrules`, searched up to the git root; plus `SOUL.md` |
| Profiles | Independent agents in one install, like Hermes profiles: each has its own **chats**, memory, skills, `SOUL.md`, model and MCP servers. Switch from the dropdown at the top of the sidebar (or `-p NAME` / `mercury profile use NAME`). Providers, appearance and tool settings are shared |
| Pick up from Hermes | Point at `~/.hermes` (type, paste, drag in, or **Browse…**), pick a Hermes profile from the dropdown and the Mercury profile it goes into (by default one with the same name): models + keys, MCP servers, personalities, memory, `SOUL.md`, skills and **conversation history** come over. *Copy* (Hermes keeps running separately) or *use in place* (Mercury shares the profile's memory, skills and SOUL.md). Re-running brings over only what's new. Hermes's database is opened read-only |
| MCP | stdio and streamable-HTTP servers, Hermes's `mcp_servers` format; tools appear as `mcp_<server>_<tool>` |
| Subagents | `delegate_task` runs up to 3 sub-tasks in parallel with fresh contexts; approvals still reach you |
| More tools | `clarify` (the agent asks you, with choices), `process_manage` (poll/wait/kill background jobs) |
| Checkpoints | files are snapshotted before every edit; `/rollback` lists them, `/rollback N` undoes the last N |
| Personalities | Hermes's built-in names (kawaii, pirate, noir, philosopher…) plus your custom ones |
| Text size | Bigger default type, sized to your monitor on first run; `A-` / `A+` in the top bar, Ctrl +/-/0, Ctrl + wheel; remembered |
| Logo | The orb, or any image you pick (PNG/JPEG/WebP/GIF/BMP/ICO): Settings > General, or drop the file on the window. Used in the top bar, the empty-chat screen and as the window icon |

Not included (yet): the messaging gateway (Telegram/Discord/…), cron, browser automation, image/TTS
generation, code sandbox, plugins, and Nous's Tool Gateway. See `PLAN.md`.

## Terminal commands

```
mercury chat [-q PROMPT] [--resume ID] [--model provider:model] [--yolo]
mercury login | logout                      Nous Portal
mercury discover [--lan] [--add]            find local servers
mercury add-provider NAME URL [--key K] [--key-env VAR] [--model M] [--no-tools]
mercury use PROVIDER [MODEL] | providers | models [PROVIDER]
mercury label-provider ID LABEL             e.g. label-provider ollama "Basement GPU"
mercury profile list | use NAME | create NAME | delete NAME [--yes]
mercury -p NAME <command>                   run any command in another profile
mercury hermes-profiles [PATH]              list Hermes profiles
mercury import-hermes [PATH] [--from HERMES_PROFILE] [--into MERCURY_PROFILE] [--in-place] [--no-sessions]
mercury mcp | sessions [--search Q] | doctor
```

Files: `~/.config/mercury/config.toml` (shared: providers, appearance, tools), `~/.local/share/mercury/`
(the `default` profile's chats, memories, skills, plus Nous sign-in) and `~/.local/share/mercury/profiles/<name>/`
for every other profile. Set `MERCURY_HOME` to keep everything in one folder, `MERCURY_PROFILE` to pick a profile.

## Build and test

```bash
cargo test                          # unit, mock-server integration and headless GUI tests
cargo clippy --all-targets          # clean
packaging/build-appimage.sh         # dist/Mercury-<ver>-x86_64.AppImage (glibc 2.31 floor via cargo-zigbuild)
packaging/smoke.sh dist/Mercury-*.AppImage   # end-to-end checks of the packaged app, incl. GUI under Xvfb
```

## A note on Nous sign-in

Nous Portal only recognises registered OAuth clients, and the only public one is Hermes's own
(`hermes-cli`). Mercury uses it by default so sign-in works, and you can change it under
Settings → Nous Portal → Advanced. Before distributing Mercury widely, ask Nous for a client id of its own.
If the browser sign-in loops on a CAPTCHA, use a Portal API key with the "Nous Portal (API key)" preset.

## Why Nous sign-in isn't carried over from Hermes

Nous Portal refresh tokens are single-use. If Mercury redeemed the token Hermes holds, Hermes's copy
would be dead, and the Portal treats a replayed token as theft and signs the account out everywhere.
So Mercury imports a `NOUS_API_KEY` if the profile has one, and otherwise asks you to sign in once.
