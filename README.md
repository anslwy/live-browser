# 🌐 Live Browser

**One browser, two drivers — you and your agent, in the same tab, at the same time.**

Drive any page from the CLI (`lb click`, `lb type`, `lb snapshot`, …) while
watching the live view — and grab control yourself for logins, captchas, and
judgment calls, then hand back. Neither side steps on the other.

![Live Browser UI](docs/ui.png)

```
agent drives via CLI  ──►  lb daemon (Chrome via CDP + JSON API)
                                    │
user watches + takes over ──►  web UI (live screenshot, handover banner)
```
## Install (Linux x86_64, macOS arm64/x86_64, Windows x86_64)

One line — installs a browser (Chromium/Chrome) plus the latest prebuilt
`lb` release into `~/.local` (Windows: `%LOCALAPPDATA%\live-browser`):

Linux / macOS:

```sh
curl -fsSL https://raw.githubusercontent.com/anslwy/live-browser/main/install.sh | bash
```

Windows (PowerShell):

```powershell
irm https://raw.githubusercontent.com/anslwy/live-browser/main/install.ps1 | iex
```

Then:

```sh
export PATH="$HOME/.local/bin:$PATH"  # if not already set
lb daemon --port 8765
```

Prefer building from source (installs Rust + Node toolchains, clones and
compiles — takes a few minutes)?

```sh
curl -fsSL https://raw.githubusercontent.com/anslwy/live-browser/main/install.sh | bash -s -- --from-source
```

## The core loop (why this exists)

```sh
# 1. Agent opens a page
lb open https://example.com

# 2. User watches the running page live at http://127.0.0.1:8765/

# 3. Agent reads + acts
lb snapshot            # [ref] tag "name" list of interactive elements
lb shot                # PNG screenshot
lb click 3
lb type 1 "ada@example.com" --submit

# 4. Agent hits a login wall → asks the human to take over.
#    This blocks until the human clicks "Done" in the web UI.
lb handover "please log in, then click Done in the UI"

# 5. Human logs in using the real browser window, clicks Done.

# 6. Agent re-reads the (now logged-in) tab and continues
lb snapshot
lb click 7
```

The browser profile persists (`~/.cache/live-browser/profile`), so logins/cookies survive
daemon restarts.

## Setup (developing from a checkout)

Requires Chrome/Chromium and Node (for the UI build).

```sh
cargo build                      # builds the `lb` binary (target/debug/lb)
cd web && npm install && npm run build   # builds the monitor UI into web/dist
```

## Run

```sh
# Headed (default): visible browser window so a human can take over.
lb daemon --port 8765

# Headless (servers, CI — no human takeover possible):
lb daemon --port 8765 --headless --no-sandbox

# Useful daemon flags:
#   --chrome /path/to/chrome   (auto-detects /usr/bin/google-chrome-stable)
#   --profile DIR              (persistent profile; keeps logins)
#   --url URL                  (page to open on start)
#   --passcode CODE             (optional API+UI guard; env LB_PASSCODE)
```

Optional passcode (like `opencode web` + `OPENCODE_SERVER_PASSWORD`):
without one the daemon prints `! LB_PASSCODE is not set; server is unsecured.`
and anyone on the machine can drive the browser.

```sh
LB_PASSCODE=secret lb daemon --port 8765          # set it (env preferred over --passcode flag, which leaks via `ps`)
LB_PASSCODE=secret lb status                      # every CLI command needs the same value
# Web UI asks for the passcode once and remembers it in localStorage.
# Raw HTTP also works:  curl -H "x-lb-passcode: secret" http://127.0.0.1:8765/api/status
# (Bearer token, Basic-auth password, or ?passcode=secret are accepted too.)
```

Set a default server once and drop the flag: `export LB_SERVER=http://127.0.0.1:8765`.

## CLI reference

| Command | What it does |
|---|---|
| `lb open <URL\|text>` | Navigate the tab (plain text searches Google) |
| `lb status` | URL, title, handover state |
| `lb snapshot` | Interactive elements as `[ref] tag "name"` (`--json` for full JSON) |
| `lb shot [--out f.png]` | Save screenshot (`--out -` prints base64) |
| `lb click <ref\|css=…>` | Click by snapshot ref or CSS selector |
| `lb click-at <x> <y>` | Click at viewport coordinates (CSS pixels) |
| `lb hover <x> <y>` | Move mouse (hover states, tooltips, menus) |
| `lb type <ref> <text> [--submit]` | Type into an input (Enter with `--submit`) |
| `lb press <key>` | Key press (`Enter`, `Tab`, `Escape`, …) |
| `lb scroll [dx] [dy]` | Scroll the page (default: down 600px) |
| `lb wheel <x> <y> [--dy N] [--dx N]` | Trackpad-style scroll of whatever is under the point (inner containers too) |
| `lb drag <x1> <y1> <x2> <y2>` | Mouse drag — text selection, sliders, resize handles, DnD |
| `lb insert <text>` | Fast paste at the cursor (focus target first) |
| `lb zoom [in\|out\|reset]` | Page zoom for small viewports (persists across navigations) |
| `lb eval <js>` | Run JS, print JSON-ified result |
| `lb back / forward / reload` | History navigation |
| `lb handover "msg" [--timeout s]` | Request human takeover, block until Done |
| `lb claim ["msg"]` | Claim control as human (same as UI "Take control") |
| `lb handover-done` | End handover (human scripts/tests) |
| `lb log` | Recent action log |

Targets accept a snapshot ref (`lb click 12`) or a raw CSS selector
(`lb click css=.login-button`).

## Web UI (http://127.0.0.1:8765/)

- **Live page view** — always-on coalesced polling (no blink, no scroll jump).
  **Click** the screenshot to click · **drag** on it to drag (select text, sliders, resize panes) ·
  **move the mouse** over it for hover states · **scroll wheel** over it to scroll the container under the cursor.
  A fake cursor + ripple marks the agent's latest click so you can follow along.
- **Snapshot table** (collapsed) — every row has a **click** button; `input`/`textarea` rows also get a
  **type** box (Send / Send + Enter). No separate browser window needed.
- **Handover banner** — appears when the agent runs `lb handover`; human takes over the
  browser window, then clicks **Done — return to agent**
- **Open / back / reload** controls and **action log**

Dev mode: `cd web && npm run dev` (proxies `/api` to the daemon on :8765).

## API (for scripts)

`GET /api/status`, `POST /api/open`, `GET /api/snapshot`, `GET /api/screenshot` (JPEG),
`GET /api/metrics`, `POST /api/click|click-at|hover|type|type-raw|insert|press|key|eval|nav|scroll|wheel|drag|zoom`,
`GET /api/handover`, `POST /api/handover/request|claim|done`,
`GET /api/log`. All POSTs take/return JSON with `{ok: …}`.

## Layout

```
src/main.rs    CLI entry (clap)
src/cli.rs     command definitions
src/client.rs  thin HTTP client (each command → daemon API)
src/daemon.rs  Chrome (chromiumoxide/CDP) + Axum API + static UI server
web/           Svelte 5 + TS + Vite monitor app (build → web/dist)
```

## Minimal on purpose

Single tab, single browser, polling screenshots — no multi-tab orchestration, no WebSocket
streaming, no remote input injection in the UI (the human uses the real browser window).
Those are the obvious next steps if this workflow proves useful.
