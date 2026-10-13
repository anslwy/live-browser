# Agent setup — live-browser (no clone needed)

You are a coding agent. Set up live-browser on this machine WITHOUT cloning
this repo unless `--from-source` is explicitly required. Follow these steps,
then report back to the user.

## 1. Install the `lb` CLI (if missing)

First check: `lb --version` (or `which lb` / `Get-Command lb`). If it works,
skip to step 2.

**Linux / macOS:**

```sh
curl -fsSL https://raw.githubusercontent.com/anslwy/live-browser/main/install.sh | bash
export PATH="$HOME/.local/bin:$PATH"
lb --version
```

- Installs a browser (Chromium/Chrome) + the latest prebuilt `lb` into
  `~/.local` (`~/.local/bin/lb`).
- If on Linux arm64 (no prebuilt binary), re-run with `--from-source`:
  `curl -fsSL https://raw.githubusercontent.com/anslwy/live-browser/main/install.sh | bash -s -- --from-source`
- If `~/.local/bin` is not on PATH, add
  `export PATH="$HOME/.local/bin:$PATH"` to the shell profile.

**Windows (PowerShell):**

```powershell
irm https://raw.githubusercontent.com/anslwy/live-browser/main/install.ps1 | iex
lb --version
```

- Installs Chrome (via winget if missing) + `lb.exe` into
  `%LOCALAPPDATA%\live-browser` and adds it to the user PATH
  (restart the terminal if `lb` is not found).

Do NOT `git clone` the repo for a normal install. Only build from source
if the prebuilt binary is unsupported on this platform.

## 2. Start the server (`lb web`)

```sh
lb web --port 8765
```

- It backgrounds by default (survives terminal close, logs to
  `daemon-<port>.log`) — no background-process mechanism needed.
- If `lb status` already works, a server is already running — `lb web`
  reuses it and just shows the URL, it does not start a second one.
- Only use `--foreground` when you need the logs in the terminal
  (debugging, or under a supervisor like systemd `Type=simple`).
- Flags you may need:
  - Containers / servers without a GUI: add `--no-sandbox --no-open`
  - To show a visible browser window for the human: add `--headed`
  - Custom page on start: add `--url https://example.com`
- Verify: `lb status` and `lb list` must succeed. Default server URL is
  `http://127.0.0.1:8765` (override with `LB_SERVER` env var).

## 3. Add `lb mcp` as an MCP server

The daemon must be running from step 2. `lb mcp` is a stateless stdio
bridge to it — same actions as the CLI.

- Command: `lb` (use the absolute path from `which lb` / `Get-Command lb`
  if the agent host may not inherit PATH), args: `["mcp"]`.
- Env: `LB_SERVER=http://127.0.0.1:8765` (match the port from step 2;
  also set `LB_PASSCODE` if the server was started with one).

Detect the agent config in the current project and edit the matching file
(create it if missing). Most common shapes:

**opencode (`opencode.json`):**

```json
"mcp": {
  "live-browser": {
    "type": "local",
    "command": ["lb", "mcp"],
    "environment": { "LB_SERVER": "http://127.0.0.1:8765" },
    "enabled": true
  }
}
```

**Claude Code / generic `.mcp.json`:**

```json
{
  "mcpServers": {
    "live-browser": {
      "command": "lb",
      "args": ["mcp"],
      "env": { "LB_SERVER": "http://127.0.0.1:8765" }
    }
  }
}
```

For other hosts (Cursor, Windsurf, etc.) use the same
command + args + env in that host's MCP settings. After writing the config,
tell the user to restart/reload the agent if MCP tools don't appear.

Quick check (without MCP): `lb snapshot` or `lb open --here https://example.com`
should work against the running server.

## 4. Report back

Reply with:

1. `lb --version` output and install path.
2. The live web UI URL (default `http://127.0.0.1:8765/` — plus
   `lb web --port 8765 link` output if available, e.g. `/?tabId=N`).
3. Which agent config file was added/edited for MCP.
4. `lb status` output proving the daemon is reachable.

Tell the user to open the web UI URL in their browser to watch the live
page, take over for logins/captchas (`lb handover` / handover banner), and
drive the browser either from the UI or via the `live-browser` MCP tools.
