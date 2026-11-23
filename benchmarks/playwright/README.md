# Live Browser vs. Playwright on loopback

Benchmark for Live Browser v0.4.27 (37fa78300bda5b823c287649b727c16499ad1be3).
The runner uses the existing daemon and CLI unchanged. It launches only
temporary browser profiles, serves its own page, and requires a loopback-only
network namespace.

## Result and recommendation

Keep chromiumoxide for now. Replace the unconditional 1.5 s navigation
sleep with one bounded readiness condition and revisit the 150 ms click
settle. On the same Chrome tab, the fixture finished loading about 1.50 s
before Live Browser returned. CLI, persistent MCP and direct HTTP all
showed that delay. Playwright attached to the tab saved time on navigation
and click by taking a different path through those waits. Live Browser's
persistent MCP and direct HTTP were faster at typing, the matching snapshot
and the matching JPEG than attached Playwright.

### Warm operations

Median / p95 in milliseconds, with 32 verified samples per cell.

| Operation | `lb` CLI | Persistent MCP | Direct HTTP | Playwright CDP, same tab | Playwright native launch |
| --- | ---: | ---: | ---: | ---: | ---: |
| Navigate to load | 1642.1 / 1694.7 | 1628.1 / 1682.8 | 1629.7 / 1691.9 | 237.0 / 291.7 | 61.7 / 79.0 |
| Click CSS | 193.9 / 202.0 | 188.0 / 194.2 | 184.2 / 193.6 | 81.2 / 91.3 | 59.1 / 68.1 |
| Type CSS | 74.4 / 79.5 | 57.8 / 67.3 | 58.7 / 68.8 | 96.3 / 103.8 | 66.9 / 72.0 |
| Live Browser DOM snapshot | 26.2 / 27.8 | 11.4 / 17.3 | 10.0 / 11.9 | 21.4 / 25.3 | 20.4 / 22.4 |
| Matched viewport JPEG | 109.0 / 121.2 | 73.6 / 87.6 | 67.7 / 80.3 | 84.0 / 101.7 | 47.1 / 64.4 |

### Navigation phases

Median / p95 in milliseconds. Browser load comes from Chrome's Navigation Timing.
Post-load is the approximate time from the browser load event ending to the
client receiving the result. These phases use different clock boundaries and
the column medians need not sum to the end-to-end median.

| Path | Client start to fixture request | Browser load | Post-load through response |
| --- | ---: | ---: | ---: |
| CLI | 20.5 / 23.7 | 120.2 / 170.8 | 1504.7 / 1505.3 |
| Persistent MCP | 5.9 / 8.4 | 121.6 / 177.2 | 1503.3 / 1504.5 |
| Direct HTTP | 7.0 / 10.6 | 123.1 / 184.4 | 1503.3 / 1504.3 |
| Playwright CDP, same tab | 7.7 / 13.1 | 232.0 / 276.0 | 1.8 / 5.0 |
| Playwright native launch | 5.2 / 6.3 | 57.9 / 75.3 | 1.0 / 1.4 |
| Raw CDP with load observer | 5.8 / 6.8 | 390.3 / 484.1 | 2.8 / 6.4 |

Raw CDP navigation took 396.0 / 497.1 ms end to end. Chrome's load times
varied across phases, including for the same tab. The post-load interval
isolates the large fixed daemon delay. The native launch's lower total tracks
its lower measured browser load time in this run. A driver-level speed
comparison would need counterbalanced browser phases with equivalent launch
flags.

### Startup

Median / p95 in milliseconds, 20 samples per path.

| Operation | Time |
| --- | ---: |
| CLI process and `lb --version` output | 7.6 / 8.2 |
| Daemon spawn to usable HTTP status | 554.7 / 610.9 |
| MCP process, initialize and list tools | 10.7 / 16.4 |
| Playwright CDP attach and probe an existing tab | 22.6 / 74.9 |
| Playwright persistent launch to usable tab | 245.2 / 268.9 |

Daemon startup includes launch configuration, browser version probing, tab
setup and serving HTTP. Native Playwright starts its own browser and probes
its initial tab. The CDP attach probe starts with the browser already up.

### Waits and response sizes

| Additional operation, 32 samples each | Median / p95 (ms) |
| --- | ---: |
| CLI compact snapshot | 33.3 / 39.1 |
| Playwright CDP ARIA snapshot | 40.1 / 51.0 |
| Playwright native ARIA snapshot | 38.9 / 55.1 |
| HTTP unchanged screenshot, 304 | 63.7 / 67.0 |
| Raw CDP clipped JPEG | 91.9 / 110.6 |
| Playwright CDP default JPEG, 1280×708 | 74.9 / 95.9 |
| HTTP wait for a selector added after 120 ms | 514.6 / 517.0 |
| Playwright CDP wait for the same selector to become visible | 182.7 / 185.4 |

- CLI starts a new runtime and HTTP connection on each action. It took
  about 14 ms longer to reach the fixture than persistent MCP. The
  `--version` probe provides a process baseline. The remaining client-path
  difference includes connection setup, argument and output handling, and
  scheduling. Loopback request time includes browser dispatch.
- Median snapshot result sizes were 3,622 bytes for CLI compact output,
  34,900 for CLI JSON, 42,299 for MCP JSON-RPC, and 20,252 for HTTP JSON.
  The Playwright result serialized to 20,129 bytes for the reused DOM scan
  and 9,125 bytes for ARIA. CLI compact text took longer than CLI JSON
  despite its smaller result on this workload.
- Comparable JPEGs decoded to 1280×900 and matched the expected content
  checks. Their median was 110,083 JPEG bytes. Some frames differed at the
  byte level. CLI and MCP carried 146,781 and 146,960 bytes of base64
  output or JSON-RPC respectively. An unchanged HTTP frame carried zero body
  bytes and still cost 63.7 ms to capture, validate and hash on this fixture.
  The attached Playwright default capture returned 1280×708 in all 32
  measured samples. The explicit clip preserved the full requested frame.
- The 120 ms selector probe exercises Live Browser's 500 ms polling cadence.
  Both waits returned with the same visible element. Their conditions
  differ in general because Live Browser checks presence for this selector
  and the Playwright probe requests visibility.

## Reproduce on Linux

Use Node 24, a local Chrome executable, and Linux user/network namespaces
with `unshare` and `ip`. The commands use the published x86-64 release binary.
On Linux ARM64, use the corresponding release archive and checksum. A local
release build of the base revision also works.

```bash
bench_dir=$(mktemp -d -t lb-playwright.XXXXXXXX)
gh release download v0.4.27 --repo anslwy/live-browser \
  --pattern 'live-browser-v0.4.27-linux-x86_64.tar.gz' --dir "$bench_dir"
(
  cd "$bench_dir"
  printf '%s\n' \
    'ca4609b8ba4cbb928b766f9dbc5d34b1e02d56808bc7e09fad24a0e96ac40789  live-browser-v0.4.27-linux-x86_64.tar.gz' |
    sha256sum -c - &&
    tar -xzf live-browser-v0.4.27-linux-x86_64.tar.gz
)
npm install --prefix "$bench_dir" --no-audit --no-fund --ignore-scripts \
  playwright@1.65.1 jpeg-js@0.4.4

chrome_path=$(command -v chromium-browser) # Or use an absolute Chrome executable path.
unshare --user --map-root-user --net sh -c 'ip link set lo up && exec "$@"' sh \
  node benchmarks/playwright/run.mjs \
  "$bench_dir/live-browser-v0.4.27-linux-x86_64/lb" "$chrome_path" \
  "$bench_dir" "$bench_dir/results.json"
```

The result file contains every measured sample, machine and version metadata,
and summaries. Set `BENCH_SAMPLES=1 BENCH_STARTUPS=1` for a smoke test and use a
fresh output filename. The runner shuts down its own daemon, MCP process and
browser and removes their temporary profiles. Dependencies, the release
archive and results stay in the caller's temporary directory.

The reported run used a Steam Deck (AMD Custom APU 0405, Linux x86-64
7.2.7-valve2-2-neptune-72-g31504582a073), Chrome for Testing 143.0.7499.4,
Node 24.11.1, Playwright 1.65.1 and the released musl `lb` 0.4.27 binary
with chromiumoxide 0.9.1. The local Rust build hit a temporary filesystem
quota, so the run used the release archive after verifying its digest and
tag against the benchmark base. An independent pass recomputed sample counts,
medians and nearest-rank p95 values for all 1,188 samples. [Release archive](https://github.com/anslwy/live-browser/releases/tag/v0.4.27)

## Method

- The 1280×900, DPR 1 headless fixture has a dashboard with 48 cards, an input,
  buttons, links, a select and a same-origin iframe. All assets and requests are
  local. Each navigation uses a unique query string and `Cache-Control:
  no-store`; the browser and tab stay warm.
- CLI starts a fresh `lb` process for every action. MCP reuses one initialized
  stdio process and its HTTP client. Direct HTTP reuses a loopback client. The
  three lanes use exactly the same tab and rotate order on each round.
  Playwright then attaches to that browser and tab using `connectOverCDP` with
  `noDefaults`. The native path uses `launchPersistentContext` with a separate
  fresh profile and the exact same Chrome executable, with Playwright's usual
  launch defaults. The timed loop exercises one local caller at a time in
  headless Linux.
- Navigation requires the intended document URL, load marker and complete
  document. Separate measurements record the time until the fixture receives
  the main request, the browser's Navigation Timing load duration, and the
  approximate time from browser load-event end to the client response. The
  latter maps monotonic timestamps to epoch time on the same host and is
  approximate at sub-millisecond resolution.
- Click uses a visible CSS target and verifies exactly one handler invocation.
  Typing focuses and clears a populated input, then dispatches per-character
  keys without submitting. The runner verifies the exact value, key count,
  trusted input count and the single synthetic clear event. Playwright uses
  locator actionability for the click; it can wait on visibility, stability and
  occlusion. Live Browser attempts the click and then applies a fixed settle.
  Those are different guarantees on a dynamic page. [Playwright actionability](https://playwright.dev/docs/actionability)
- The matching snapshot runs the shipped `SNAPSHOT_JS` verbatim. Live Browser
  asks Chrome for URL and title separately and returns its API envelope.
  Playwright gets URL, title and elements in one evaluation. Each result is
  checked for 148 elements, refs, form values, select options, and an iframe
  control. CLI compact text and Playwright ARIA YAML are measured separately.
  ARIA describes roles and accessible names and includes structural content.
  Live Browser callers also rely on numeric refs, coordinates and frame
  metadata.
- Screenshot is a 1280×900 viewport JPEG at quality 70. CLI uses
  `lb shot --out -` (base64 stdout, no file write), MCP carries a base64 image
  in JSON-RPC, HTTP returns raw JPEG, and Playwright returns a buffer. The
  attached Playwright comparison uses `fullPage: true` with an explicit
  1280×900 clip because normal viewport capture on the attached page can
  omit some content. The runner also records the default attached capture
  as a diagnostic. A raw `Page.captureScreenshot` probe uses the same
  browser with an explicit clip and records its CDP base64 payload size.
  The runner decodes every image outside the timed interval and checks its
  dimensions, top header and bottom background for the comparable paths.
  It also records JPEG hashes. The HTTP 304 probe verifies that an unchanged
  frame has an empty response body.
- Each warm operation discards two warmups. Startup discards one warmup and
  gets a new profile per browser launch. Timing includes client-side response
  receipt and the lane's normal serialization or parsing, with additional
  outcome checks outside it. Median averages the two middle values when
  needed. p95 is nearest rank. Startup measures a usable daemon status versus
  a usable direct browser tab, plus separate MCP initialization, CDP attach
  and CLI `--version` probes. The Playwright module is already loaded for
  its startup probes. All timings use a warm OS and a fresh profile per
  browser launch. The daemon's startup command requests `about:blank` and the
  status probe accepts Chrome's internal startup tab. Raw CDP navigation
  sends `Page.navigate` and uses a
  separate Playwright load observer.

## Control path and likely changes

- The CLI starts with a per-process Reqwest client that sends HTTP to the
  already-running daemon. The stdio MCP bridge keeps its client alive, adds
  session headers and forwards to the same
  Axum handlers. Its snapshot response pretty-prints daemon JSON as text in
  JSON-RPC. For screenshots the daemon decodes/validates JPEG before hashing
  it, and CLI/MCP decode/validate again before encoding to base64.
  [CLI client](../../src/client.rs), [MCP bridge](../../src/mcp.rs)
- Navigation and input handlers use chromiumoxide over CDP. Normal HTTP
  navigation runs `goto` with a 25-second bound, sleeps
  1.5 seconds, reads the URL and calls `wait_for_navigation` with another
  10-second bound, then reads the final URL and reapplies zoom. Timeout/retry
  paths have further waits. Hash-only and about:blank paths are special cases.
  Chromiumoxide's `goto` already tracks load completion.
  CSS click adds a 150 ms settle after the click and updates shared cursor
  metadata. Main-frame typing without submit skips the settle. Numeric
  click/type refs rescan and tag the DOM first. `wait` also runs the snapshot
  scan up front, then polls its predicate every 500 ms. Snapshot itself makes
  several browser calls and walks interactive nodes with layout/style reads.
  [Daemon handlers](../../src/daemon.rs) ·
  [Chromiumoxide navigation implementation](https://docs.rs/chromiumoxide/latest/src/chromiumoxide/page.rs.html)
- The web UI coalesces JPEG refreshes with at most
  one in flight, fetches status and snapshot around click-at, swaps the shown
  image after decode, and uses a two-second liveness tick. Hover is throttled.
  Coordinate click has a server-side 120 ms settle. Even an HTTP 304 comes
  after browser capture, JPEG validation and hashing. This viewport path
  pulls JPEGs over HTTP. It forwards keystrokes to `/api/key` in sequence
  and maps pointer coordinates to viewport coordinates before sending
  click, wheel or drag input. The recording loop also captures JPEGs and has
  its own scheduling and encoding work. [Web UI](../../web/src/App.svelte),
  [Recording loop](../../src/record.rs)
- First replace the navigation sleep and second wait with one bounded wait
  for a defined navigation readiness event, then verify successful
  navigation and preserve the existing retry, same-document and zoom
  behavior. Cover redirects, failed loads, same-document changes, SPA
  hydration and old Chrome before changing defaults. For client workflows
  that need application readiness, let the caller name a predicate or
  selector. Similarly, separate click completion from an optional named
  postcondition and measure the impact of a tighter wait cadence. Reuse
  persistent MCP for multiple calls where process overhead matters.
- For interactive latency, measure input-send, CDP dispatch, capture,
  validation/hash, JPEG transfer and client decode/display under one trace
  ID, with and without the viewport and recording active. Explore reducing
  redundant captures and validating JPEG bytes once per trusted boundary.
  Test on the actual Mac↔Deck/Tailscale route and under the actual client
  harness before making claims about remote or visible latency.

## Migration tradeoffs

Playwright could improve locator actionability, waiting for SPA state, and
frame handling. A private CDP sidecar is the lowest-risk way to prototype it
for selected operations. Keep one browser owner and the daemon's HTTP/MCP
contracts, passcode and user lock. Keep the debugger loopback-only and map
the sidecar's targets to Live Browser tab IDs. Test for interference with
input, dialogs, downloads, recording, tab discovery and browser recovery
when two clients attach. Playwright documents lower fidelity over CDP
compared with its own protocol. [Playwright BrowserType](https://playwright.dev/docs/api/class-browsertype)

A full migration must replace or carry forward shared versus isolated
contexts, reuse of a dedicated profile after login, tab ownership and
reconciliation, session disconnect behavior, handover and cursor state.
`launchPersistentContext` provides one persistent context. Closing it closes
Chrome. Chrome grants one browser process exclusive use of its user-data
directory. Exercise the existing system Chrome/CloakBrowser choice and
profile separation, popup and cross-origin tabs, crash or sleep recovery,
and macOS headed operation. A native launch still needs the JPEG viewport,
input mapping and public APIs. [Playwright BrowserType](https://playwright.dev/docs/api/class-browsertype)

The current release workflow packages a Rust executable and static UI for
Linux x86-64/aarch64, signed/notarized
macOS arm64/x86-64, and Windows x86-64. A Node sidecar or replacement adds a
runtime, package lifecycle, and platform support/signing work. Bundled
Playwright browsers also add downloads and Linux runtime dependencies.
Using a preinstalled executable avoids browser downloads but requires a
compatibility matrix for existing Chrome and CloakBrowser builds.
[Release workflow](../../.github/workflows/release.yml) ·
[Playwright BrowserType](https://playwright.dev/docs/api/class-browsertype) ·
[Playwright Docker support](https://playwright.dev/docs/docker)
