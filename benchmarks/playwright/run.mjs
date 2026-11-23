import assert from 'node:assert/strict';
import { execFile, execFileSync, spawn } from 'node:child_process';
import { createHash, randomBytes } from 'node:crypto';
import { mkdtemp, mkdir, readFile, rm, writeFile } from 'node:fs/promises';
import { createServer } from 'node:http';
import { createRequire } from 'node:module';
import { arch, cpus, networkInterfaces, platform, release, tmpdir } from 'node:os';
import { isAbsolute, join } from 'node:path';
import { createInterface } from 'node:readline';
import { setTimeout as delay } from 'node:timers/promises';
import { promisify } from 'node:util';

const [lb, chrome, dependencies, output] = process.argv.slice(2);
assert([lb, chrome, dependencies, output].every(path => path && isAbsolute(path)),
  'Usage: node run.mjs /abs/lb /abs/chrome /abs/npm-prefix /abs/results.json');
assert(Object.values(networkInterfaces()).flat().every(address => address.internal),
  'Run inside a loopback-only network namespace; see README.md');
const require = createRequire(join(dependencies, 'package.json'));
const { chromium } = require('playwright');
const { decode } = require('jpeg-js');
const count = Number(process.env.BENCH_SAMPLES ?? 32);
const startupCount = Number(process.env.BENCH_STARTUPS ?? 20);
assert(Number.isInteger(count) && count > 0 && Number.isInteger(startupCount) && startupCount > 0);
const viewport = { width: 1280, height: 900 };
const typing = 'local benchmark';
const javascript = await readFile(new URL('../../src/daemon.rs', import.meta.url), 'utf8');
const snapshotJS = javascript.match(/const SNAPSHOT_JS: &str = r#"([\s\S]+?)"#;/)[1];
const exec = promisify(execFile);
const scratch = await mkdtemp(join(tmpdir(), 'lb-bench-run-'));
const rows = {};
const requestTimes = new Map();
const active = new Set();
let interrupted = false;

for (const signal of ['SIGINT', 'SIGTERM']) {
  process.on(signal, () => { interrupted = true; });
}

function record(name, sample, warm) {
  if (!warm) (rows[name] ??= []).push(sample);
}

async function measure(name, operation, warm = false, verify = () => ({})) {
  assert(!interrupted, 'Interrupted');
  const start = performance.now();
  const result = await operation();
  const ms = performance.now() - start;
  const extra = await verify(result, start + ms);
  record(name, { ms, ...extra }, warm);
  return result;
}

const cards = Array.from({ length: 48 }, (_, index) =>
  '<article><h2>Project ' + (index + 1) + '</h2><p>Assigned · Updated today</p>' +
  '<a href="#detail">Details</a> <button>Review</button> <button>Archive</button></article>'
).join('');
const html = `<!doctype html><html lang="en"><meta charset="utf-8"><title>Benchmark dashboard</title>
<style>
*{box-sizing:border-box}body{margin:0;background:#eef2f7;color:#17243b;font:16px system-ui}
header{background:#213d68;color:white;padding:24px 48px}h1{margin:0}main{margin:24px 48px}
section,article{background:white;border:1px solid #c3cee0;border-radius:8px;padding:16px}
#grid{display:grid;grid-template-columns:repeat(4,1fr);gap:14px;margin-top:16px}
h2{margin:0;font-size:18px}button,input,select{font:inherit;padding:6px 10px}
a{color:#174c91}iframe{width:300px;height:55px;border:0}output{padding:0 18px}
input{caret-color:transparent}
</style>
<header><h1>Project dashboard</h1><p>Activity and assignments</p></header>
<main><section><label>Search <input id="query" value="initial"></label>
<button id="test">Apply filters</button><output id="clicks">0</output>
<label>Team <select><option>All teams</option><option>Design</option></select></label>
<iframe title="Team tools" src="/frame"></iframe></section><div id="grid">${cards}</div></main>
<script>
window.stats = { clicks: 0, keys: 0, inputs: 0, trusted: 0 };
const query = document.getElementById('query');
query.onkeydown = () => ++stats.keys;
query.oninput = event => { ++stats.inputs; if (event.isTrusted) ++stats.trusted; };
document.getElementById('test').onclick = () => document.getElementById('clicks').value = ++stats.clicks;
window.addEventListener('load', () => { document.body.dataset.ready = 'yes'; });
</script>`;
const fixture = createServer((req, res) => {
  const url = new URL(req.url, 'http://loopback');
  if (url.pathname === '/dashboard') requestTimes.set(url.search, performance.now());
  res.writeHead(200, { 'Content-Type': 'text/html; charset=utf-8', 'Cache-Control': 'no-store' });
  res.end(url.pathname === '/dashboard' ? html : '<button>Frame tool</button>');
});

async function listen(server) {
  await new Promise((resolve, reject) => {
    server.once('error', reject);
    server.listen(0, '127.0.0.1', resolve);
  });
  return server.address().port;
}

async function freePort() {
  const server = createServer();
  const port = await listen(server);
  await new Promise(resolve => server.close(resolve));
  return port;
}

const passcode = randomBytes(24).toString('hex');
const env = Object.fromEntries(Object.entries(process.env).filter(([name]) =>
  !/^(LB_|SB_|.*_proxy$|xdg_)/i.test(name)));
Object.assign(env, {
  HOME: scratch,
  TMPDIR: scratch,
  XDG_CACHE_HOME: join(scratch, 'cache'),
  XDG_CONFIG_HOME: join(scratch, 'config'),
  XDG_DATA_HOME: join(scratch, 'data'),
  LB_PASSCODE: passcode,
  LB_NO_UPDATE_CHECK: '1',
  NO_PROXY: '127.0.0.1,localhost',
});

function start(args) {
  const child = spawn(lb, args, { env, detached: true, stdio: ['pipe', 'pipe', 'pipe'] });
  active.add(child);
  child.logs = '';
  const log = chunk => { child.logs = (child.logs + chunk).slice(-16000); };
  child.stdout.on('data', log);
  child.stderr.on('data', log);
  child.stdin.on('error', log);
  child.done = new Promise(resolve => {
    child.once('exit', resolve);
    child.once('error', error => { log(error.message); resolve(null); });
  }).finally(() => active.delete(child));
  return child;
}

async function stop(child) {
  if (!active.has(child)) return;
  child.stdin.end();
  child.kill('SIGTERM');
  const timeout = new AbortController();
  try {
    await Promise.race([child.done, delay(22000, undefined, { signal: timeout.signal }).then(() => {
      if (active.has(child)) process.kill(-child.pid, 'SIGKILL');
      return child.done;
    })]);
  } finally {
    timeout.abort();
  }
}

function check(result) {
  assert.equal(result.ok, true, JSON.stringify(result));
  return result;
}

let fixtureURL;
let serial = 0;
const evalState = `(() => ({
  ready: document.body?.dataset.ready,
  url: location.href, state: document.readyState,
  width: innerWidth, height: innerHeight, dpr: devicePixelRatio,
  agent: navigator.userAgent,
  origin: performance.timeOrigin,
  load: performance.getEntriesByType('navigation')[0].loadEventEnd,
  value: document.querySelector('#query')?.value,
  ...window.stats,
}))()`;

function navigationResult(url, start, end, state) {
  assert.equal(state.url, url);
  assert.equal(state.ready, 'yes');
  assert.equal(state.state, 'complete');
  assert(state.load > 0);
  assert.deepEqual([state.width, state.height, state.dpr], [viewport.width, viewport.height, 1]);
  const key = new URL(url).search;
  assert(requestTimes.has(key));
  const requestMs = requestTimes.get(key) - start;
  requestTimes.delete(key);
  const afterLoadMs = performance.timeOrigin + end - state.origin - state.load;
  return { browserLoadMs: state.load, requestMs, afterLoadMs, agent: state.agent };
}

async function daemon(profile, wrapper) {
  const port = await freePort();
  const server = 'http://127.0.0.1:' + port;
  const flags = ['--server', server, '--tab', '1'];
  const processStart = performance.now();
  const child = start(['web', '--foreground', '--headless', '--no-sandbox', '--no-open',
    '--port', String(port), '--profile', profile, '--chrome', wrapper, '--url', 'about:blank']);

  async function http(path, body, query = 'tab=1', headers = {}) {
    const response = await fetch(server + '/api/' + path + (query ? '?' + query : ''), {
      method: body ? 'POST' : 'GET',
      headers: { 'x-lb-passcode': passcode, 'Content-Type': 'application/json', ...headers },
      body: body ? JSON.stringify({ ...body, tab: 1 }) : undefined,
      signal: AbortSignal.timeout(65000),
    });
    const bytes = Buffer.from(await response.arrayBuffer());
    if (response.status === 304) return { bytes, status: 304 };
    assert(response.ok, response.status + ': ' + bytes.toString());
    const json = path === 'screenshot' ? null : check(JSON.parse(bytes));
    return { bytes, json, hash: response.headers.get('x-image-hash') };
  }

  try {
    const deadline = performance.now() + 25000;
    let lastError;
    while (true) {
      assert(active.has(child) && !interrupted && performance.now() < deadline,
        child.logs + '\nLast status probe: ' + lastError);
      try {
        const status = (await http('status')).json;
        assert.equal(status.tab, 1);
        assert.match(status.url, /^(about:blank|chrome(?:-search)?:\/\/)/);
        break;
      } catch (error) {
        lastError = error;
        await delay(10);
      }
    }
    const ms = performance.now() - processStart;
    const [debugPort, socketPath] = (await readFile(join(profile, 'DevToolsActivePort'), 'utf8')).trim().split('\n');
    const cdpURL = 'ws://127.0.0.1:' + debugPort + socketPath;
    const cli = async args => (await exec(lb, [...flags, ...args], {
      env, encoding: 'buffer', maxBuffer: 4 * 1024 * 1024, timeout: 65000,
    })).stdout;
    return { child, ms, flags, cdpURL, cli, http };
  } catch (error) {
    await stop(child);
    throw error;
  }
}

async function mcp(flags, warm) {
  const startTime = performance.now();
  const child = start([...flags, 'mcp']);
  const pending = new Map();
  let id = 0;
  const lines = createInterface({ input: child.stdout });
  lines.on('line', line => {
    const result = JSON.parse(line);
    pending.get(result.id)?.({ ...result, bytes: Buffer.byteLength(line) + 1 });
  });
  async function send(method, params, notification = false) {
    const seq = ++id;
    const message = { jsonrpc: '2.0', method, params };
    if (notification) {
      child.stdin.write(JSON.stringify(message) + '\n');
      return;
    }
    message.id = seq;
    let timeout;
    try {
      return await new Promise((resolve, reject) => {
        pending.set(seq, response => response.error ? reject(new Error(JSON.stringify(response.error))) : resolve(response));
        timeout = setTimeout(() => reject(new Error(method + ' timed out: ' + child.logs)), 65000);
        child.stdin.write(JSON.stringify(message) + '\n');
      });
    } finally {
      clearTimeout(timeout);
      pending.delete(seq);
    }
  }
  try {
    const init = await send('initialize', {
      protocolVersion: '2025-06-18', capabilities: {}, clientInfo: { name: 'lb-bench', version: '1' },
    });
    assert.equal(init.result.serverInfo.name.length > 0, true);
    await send('notifications/initialized', {}, true);
    assert((await send('tools/list', {})).result.tools.some(tool => tool.name === 'snapshot'));
    record('startup: MCP spawn + initialize + tools/list', { ms: performance.now() - startTime }, warm);
    return {
      child,
      async call(name, args = {}) {
        const { result, bytes } = await send('tools/call', { name, arguments: { ...args, tab: 1 } });
        assert(!result.isError, JSON.stringify(result));
        return { content: result.content, bytes };
      },
    };
  } catch (error) {
    await stop(child);
    throw error;
  }
}

function screenshot(bytes, requireBottom = true) {
  const image = decode(bytes, { useTArray: true });
  assert(image.data[0] < 100 && image.data[2] > image.data[0], 'Expected the blue dashboard header');
  const bottomLeft = 4 * (880 * image.width + 8);
  const bottomRGB = Array.from(image.data.subarray(bottomLeft, bottomLeft + 3));
  if (requireBottom) {
    assert.deepEqual([image.width, image.height], [viewport.width, viewport.height]);
    assert(bottomRGB.every((value, index) => Math.abs(value - [238, 242, 247][index]) < 7),
      'Expected the dashboard background near the bottom, got RGB ' + bottomRGB.join(','));
  }
  return {
    imageBytes: bytes.length, imageWidth: image.width, imageHeight: image.height,
    bottomRGB, imageHash: createHash('sha256').update(bytes).digest('hex'),
  };
}

function snapshot(elements) {
  assert.equal(elements.length, 148);
  assert.deepEqual(elements.slice(0, 3).map(el => [el.ref, el.tag]),
    [[1, 'input'], [2, 'button'], [3, 'select']]);
  assert.equal(elements[0].value, typing);
  assert.equal(elements[1].name, 'Apply filters');
  assert.deepEqual(elements[2].options.map(option => option.text), ['All teams', 'Design']);
  assert.equal(elements.at(-1).name, 'Frame tool');
  assert.equal(elements.at(-1).frame, 'iframe[0]');
  return { elements: elements.length };
}

function textResult(response) {
  return check(JSON.parse(response.content[0].text));
}

async function startupNative(profile, wrapper) {
  const context = await chromium.launchPersistentContext(profile, {
    executablePath: wrapper, headless: true, chromiumSandbox: false,
    viewport, deviceScaleFactor: 1, timeout: 25000, env,
  });
  try {
    const page = context.pages()[0];
    await page.waitForLoadState('load');
    assert.equal(page.url(), 'about:blank');
    await page.evaluate('document.readyState');
    return { context, page };
  } catch (error) {
    await context.close();
    throw error;
  }
}

async function attach(cdpURL) {
  const browser = await chromium.connectOverCDP(cdpURL, { noDefaults: true, timeout: 25000 });
  try {
    const page = browser.contexts()[0].pages()[0];
    assert.equal(await page.evaluate('innerWidth'), viewport.width);
    return { browser, page };
  } catch (error) {
    await browser.close();
    throw error;
  }
}

async function operations(label, lane, warm) {
  const url = fixtureURL + '/dashboard?n=' + ++serial;
  const name = op => label + ': ' + op;
  const start = performance.now();
  await measure(name('navigate'), () => lane.open(url), warm,
    async (_, end) => navigationResult(url, start, end, await lane.verify(evalState)));
  await measure(name('click CSS'), lane.click, warm, async () => {
    assert.equal((await lane.verify(evalState)).clicks, 1);
    return {};
  });
  await measure(name('type CSS'), lane.type, warm, async () => {
    const state = await lane.verify(evalState);
    assert.deepEqual([state.value, state.keys, state.trusted, state.inputs],
      [typing, typing.length, typing.length, typing.length + 1]);
    return {};
  });
  await measure(name('snapshot'), lane.snap, warm, lane.checkSnap);
  await measure(name('JPEG'), lane.shot, warm, lane.checkShot);
  if (lane.extra) await lane.extra(warm);
}

function lbLanes(server, bridge) {
  const { cli, http } = server;
  const { call } = bridge;
  const verify = async js => (await http('eval', { js })).json.result;
  const checkCli = async args => check(JSON.parse(await cli([...args, '--json'])));
  return [
    ['CLI', {
      verify, open: url => checkCli(['open', url]),
      click: () => checkCli(['click', 'css=#test']),
      type: () => checkCli(['type', 'css=#query', typing]),
      snap: () => cli(['snapshot', '--json']),
      checkSnap: bytes => ({ ...snapshot(check(JSON.parse(bytes)).elements), responseBytes: bytes.length }),
      shot: () => cli(['shot', '--out', '-']),
      checkShot: bytes => ({ ...screenshot(Buffer.from(bytes.toString(), 'base64')), responseBytes: bytes.length }),
      extra: warm => measure('CLI: compact snapshot', () => cli(['snapshot']), warm, bytes => {
        assert(bytes.toString().includes('Frame tool'));
        return { responseBytes: bytes.length };
      }),
    }],
    ['MCP', {
      verify, open: async url => textResult(await call('open', { url })),
      click: async () => textResult(await call('click', { target: 'css=#test' })),
      type: async () => textResult(await call('type_text', { target: 'css=#query', text: typing })),
      snap: () => call('snapshot'),
      checkSnap: result => ({ ...snapshot(textResult(result).elements), responseBytes: result.bytes }),
      shot: () => call('screenshot'),
      checkShot: result => {
        const { mimeType, data } = result.content[0];
        assert.equal(mimeType, 'image/jpeg');
        return { ...screenshot(Buffer.from(data, 'base64')), responseBytes: result.bytes };
      },
    }],
    ['HTTP', {
      verify, open: url => http('open', { url }),
      click: () => http('click', { target: 'css=#test', kind: 'single' }),
      type: () => http('type', { target: 'css=#query', text: typing }),
      snap: () => http('snapshot'),
      checkSnap: result => ({ ...snapshot(result.json.elements), responseBytes: result.bytes.length }),
      shot: () => http('screenshot'),
      checkShot: result => ({ ...screenshot(result.bytes), responseBytes: result.bytes.length }),
      extra: async warm => {
        const { hash } = await http('screenshot');
        assert.match(hash, /^[a-f0-9]{16}$/);
        await measure('HTTP: unchanged JPEG', () => http('screenshot', null, 'tab=1&have=' + hash), warm, result => {
          assert.equal(result.status, 304);
          return { responseBytes: result.bytes.length };
        });
      },
    }],
  ];
}

function playwrightLane(label, page) {
  page.setDefaultTimeout(10000);
  const screenshotOptions = { type: 'jpeg', quality: 70 };
  return {
    verify: js => page.evaluate(js),
    open: async url => assert.equal((await page.goto(url, { waitUntil: 'load' })).status(), 200),
    click: () => page.locator('#test').click(),
    type: async () => {
      await page.locator('#query').click();
      await page.evaluate(`(() => {
        const el = document.querySelector('#query');
        el.focus();
        el.value = '';
        el.dispatchEvent(new Event('input', { bubbles: true }));
      })()`);
      await page.locator('#query').pressSequentially(typing);
    },
    snap: () => page.evaluate('({url: location.href, title: document.title, elements: ' + snapshotJS + '})'),
    checkSnap: result => {
      assert.equal(result.title, 'Benchmark dashboard');
      assert(result.url.startsWith(fixtureURL));
      return { ...snapshot(result.elements), responseBytes: Buffer.byteLength(JSON.stringify(result)) };
    },
    shot: () => page.screenshot(label === 'PW CDP'
      ? { ...screenshotOptions, fullPage: true, clip: { x: 0, y: 0, ...viewport } }
      : screenshotOptions),
    checkShot: bytes => ({ ...screenshot(bytes), responseBytes: bytes.length }),
    extra: async warm => {
      await measure(label + ': ARIA snapshot', () => page.locator('body').ariaSnapshot(), warm, aria => {
        assert(aria.includes('Apply filters') && aria.includes('Search') && aria.includes('Project dashboard'));
        return { responseBytes: Buffer.byteLength(aria) };
      });
      if (label === 'PW CDP') {
        await measure(label + ': default JPEG', () => page.screenshot(screenshotOptions), warm,
          bytes => ({ ...screenshot(bytes, false), responseBytes: bytes.length }));
      }
    },
  };
}

async function probes(server, page) {
  const session = await page.context().newCDPSession(page);
  const lane = playwrightLane('PW CDP', page);
  for (let i = -2; i < count; ++i) {
    const warm = i < 0;
    const url = fixtureURL + '/dashboard?n=' + ++serial;
    const start = performance.now();
    await measure('raw CDP: navigate', async () => {
      const loaded = page.waitForURL(url, { waitUntil: 'load' });
      const [, result] = await Promise.all([loaded, session.send('Page.navigate', { url })]);
      assert(!result.errorText);
    }, warm, async (_, end) => navigationResult(url, start, end, await page.evaluate(evalState)));
    await lane.click();
    await lane.type();
    await measure('raw CDP: JPEG', () => session.send('Page.captureScreenshot', {
      format: 'jpeg', quality: 70, fromSurface: true, captureBeyondViewport: true,
      clip: { x: 0, y: 0, ...viewport, scale: 1 },
    }), warm, result => ({
      ...screenshot(Buffer.from(result.data, 'base64')),
      responseBytes: Buffer.byteLength(result.data),
    }));
    for (const [label, wait] of [
      ['HTTP', () => server.http('wait', { target: 'css=#delayed', timeout: 3 })],
      ['PW CDP', () => page.locator('#delayed').waitFor({ state: 'visible' })],
    ]) {
      await page.evaluate(`(() => {
        document.querySelector('#delayed')?.remove();
        setTimeout(() => {
          const el = document.createElement('button');
          el.id = 'delayed'; el.textContent = 'Ready'; document.body.prepend(el);
        }, 120);
      })()`);
      await measure(label + ': delayed selector', wait, warm,
        async () => { assert(await page.locator('#delayed').isVisible()); return {}; });
    }
  }
  await session.detach();
}

function stats(values) {
  const sorted = [...values].sort((a, b) => a - b);
  const mid = Math.floor(sorted.length / 2);
  return {
    n: sorted.length,
    median: sorted.length % 2 ? sorted[mid] : (sorted[mid - 1] + sorted[mid]) / 2,
    p95: sorted[Math.ceil(sorted.length * 0.95) - 1],
  };
}

async function main() {
  const bin = join(scratch, 'bin');
  await mkdir(bin);
  // Suppress Live Browser's interface discovery so the benchmark binds only to loopback.
  for (const program of ['tailscale', 'ip', 'ifconfig', 'hostname']) {
    await writeFile(join(bin, program), '#!/bin/sh\nexit 1\n', { mode: 0o700 });
  }
  const wrapper = join(bin, 'benchmark-chrome');
  const quotedChrome = "'" + chrome.replaceAll("'", "'\\''") + "'";
  await writeFile(wrapper, '#!/bin/sh\nexec ' + quotedChrome +
    ' --no-proxy-server --disable-background-networking "$@"\n',
  { mode: 0o700 });
  env.PATH = bin + ':' + process.env.PATH;
  const fixturePort = await listen(fixture);
  fixtureURL = 'http://127.0.0.1:' + fixturePort;

  const metadata = {
    revision: execFileSync('git', ['rev-parse', 'HEAD'], { encoding: 'utf8' }).trim(),
    lb: execFileSync(lb, ['--version'], { env, encoding: 'utf8' }).trim(),
    chrome: execFileSync(wrapper, ['--version'], { env, encoding: 'utf8' }).trim(),
    playwright: require('playwright/package.json').version,
    node: process.version, os: [platform(), release(), arch()], cpu: cpus()[0].model, viewport,
    count, startupCount,
  };
  console.log(JSON.stringify(metadata));
  for (let i = -1; i < startupCount; ++i) {
    const warm = i < 0;
    await measure('startup: lb --version', () => exec(lb, ['--version'], { env }), warm,
      result => { assert.equal(result.stdout.trim(), metadata.lb); return {}; });
    const service = await daemon(join(scratch, 'lb-start-' + i), wrapper);
    try {
      record('startup: lb daemon to status', { ms: service.ms }, warm);
      const bridge = await mcp(service.flags, warm);
      await stop(bridge.child);
      const cdp = await measure('startup: PW CDP attach + tab probe', () => attach(service.cdpURL), warm);
      await cdp.browser.close();
    } finally {
      await stop(service.child);
    }
    const native = await measure('startup: PW persistent launch to tab probe',
      () => startupNative(join(scratch, 'pw-start-' + i), wrapper), warm);
    await native.context.close();
  }

  const service = await daemon(join(scratch, 'lb-warm'), wrapper);
  try {
    const bridge = await mcp(service.flags, true);
    try {
      const lanes = lbLanes(service, bridge);
      for (let i = -2; i < count; ++i) {
        for (let offset = 0; offset < lanes.length; ++offset) {
          const [label, lane] = lanes[(Math.max(i, 0) + offset) % lanes.length];
          await operations(label, lane, i < 0);
        }
        if (i >= 0 && (i === 0 || (i + 1) % 8 === 0)) {
          console.log('Live Browser rounds: ' + (i + 1), Object.fromEntries(lanes.map(([label]) =>
            [label, Math.round(stats(rows[label + ': navigate'].map(sample => sample.ms)).median)])));
        }
      }
    } finally {
      await stop(bridge.child);
    }
    const cdp = await attach(service.cdpURL);
    try {
      const lane = playwrightLane('PW CDP', cdp.page);
      for (let i = -2; i < count; ++i) await operations('PW CDP', lane, i < 0);
      await probes(service, cdp.page);
    } finally {
      await cdp.browser.close();
    }
  } finally {
    await stop(service.child);
  }

  const native = await startupNative(join(scratch, 'pw-warm'), wrapper);
  try {
    const lane = playwrightLane('PW native', native.page);
    for (let i = -2; i < count; ++i) await operations('PW native', lane, i < 0);
  } finally {
    await native.context.close();
  }
  const summary = Object.fromEntries(Object.entries(rows).map(([name, samples]) => [
    name, Object.fromEntries(Object.keys(samples[0]).filter(key => typeof samples[0][key] === 'number')
      .map(key => [key, stats(samples.map(sample => sample[key]))])),
  ]));
  await writeFile(output, JSON.stringify({ metadata, summary, rows }, null, 2) + '\n', { flag: 'wx' });
  console.table(Object.fromEntries(Object.entries(summary).map(([name, result]) => [name, result.ms])));
}

try {
  await main();
} finally {
  await Promise.all([...active].map(stop));
  fixture.closeAllConnections();
  await new Promise(resolve => fixture.close(resolve));
  await rm(scratch, { recursive: true });
}
