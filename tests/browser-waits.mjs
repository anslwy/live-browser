import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { randomBytes } from 'node:crypto';
import { mkdir, mkdtemp, rm, writeFile } from 'node:fs/promises';
import { createServer } from 'node:http';
import { networkInterfaces, tmpdir } from 'node:os';
import { isAbsolute, join } from 'node:path';
import { setTimeout as delay } from 'node:timers/promises';

const [lb, chrome] = process.argv.slice(2);
assert(lb && chrome && isAbsolute(lb) && isAbsolute(chrome),
  'Usage: node tests/browser-waits.mjs /absolute/path/to/lb /absolute/path/to/chrome');
assert(Object.values(networkInterfaces()).flat().every(address => address.internal),
  'Run in a loopback-only network namespace');

const html = `<!doctype html><title>Wait fixture</title>
<button id="click">Click</button><output id="clicks">0</output>
<script>
  addEventListener('load', () => { document.body.dataset.loaded = 'yes'; });
  document.querySelector('#click').onclick = (event) => {
    document.querySelector('#clicks').value = event.isTrusted ? '1' : 'untrusted';
    setTimeout(() => {
      const element = document.createElement('div');
      element.id = 'delayed';
      document.body.append(element);
    }, 120);
  };
</script>`;
const requests = new Map();
const fixture = createServer(async (req, res) => {
  const path = new URL(req.url, 'http://127.0.0.1').pathname;
  requests.set(path, (requests.get(path) ?? 0) + 1);
  if (path === '/redirect') {
    res.writeHead(302, { Location: '/fast' }).end();
  } else if (path === '/failed') {
    req.socket.destroy();
  } else if (path === '/slow.js') {
    await delay(300);
    res.writeHead(200, { 'Content-Type': 'text/javascript' }).end('window.slowScriptRan = true;');
  } else {
    res.writeHead(200, { 'Content-Type': 'text/html', 'Cache-Control': 'no-store' });
    res.end(path === '/slow' ? html + '<script defer src="/slow.js"></script>' : html);
  }
});

async function listen(server) {
  await new Promise((resolve, reject) => {
    server.once('error', reject).listen(0, '127.0.0.1', resolve);
  });
  return server.address().port;
}

const scratch = await mkdtemp(join(tmpdir(), 'lb-browser-waits-'));
const passcode = randomBytes(24).toString('hex');
let daemon;
let exited;
let logs = '';
try {
  const base = 'http://127.0.0.1:' + await listen(fixture);
  const reserve = createServer();
  const port = await listen(reserve);
  await new Promise(resolve => reserve.close(resolve));
  const bin = join(scratch, 'bin');
  await mkdir(bin);
  for (const name of ['tailscale', 'ip', 'ifconfig', 'hostname']) {
    await writeFile(join(bin, name), '#!/bin/sh\nexit 1\n', { mode: 0o700 });
  }
  const env = Object.fromEntries(Object.entries(process.env).filter(([name]) =>
    !/^(LB_|SB_|XDG_|.*_PROXY$)/i.test(name)));
  Object.assign(env, {
    HOME: scratch, TMPDIR: scratch, PATH: bin + ':' + process.env.PATH,
    LB_PASSCODE: passcode, LB_NO_UPDATE_CHECK: '1',
  });
  daemon = spawn(lb, [
    'web', '--foreground', '--headless', '--no-sandbox', '--no-open',
    '--port', String(port), '--profile', join(scratch, 'profile'),
    '--chrome', chrome, '--url', 'about:blank',
  ], { env, detached: true, stdio: ['ignore', 'pipe', 'pipe'] });
  exited = new Promise(resolve => {
    daemon.once('exit', resolve);
    daemon.once('error', error => { logs += error; resolve(); });
  });
  for (const stream of [daemon.stdout, daemon.stderr]) {
    stream.on('data', chunk => { logs = (logs + chunk).slice(-16000); });
  }

  async function api(path, body, headers = { 'x-lb-client': 'ui' }) {
    const res = await fetch(`http://127.0.0.1:${port}/api/${path}`, {
      method: body ? 'POST' : 'GET',
      headers: { ...headers, 'x-lb-passcode': passcode, 'content-type': 'application/json' },
      body: body && JSON.stringify(body),
      signal: AbortSignal.timeout(95000),
    });
    assert.equal(res.status, 200);
    return res.json();
  }
  function ok(result) {
    assert.equal(result.ok, true, JSON.stringify(result));
    return result;
  }
  async function evaluate(js, tab = 1) {
    return ok(await api('eval', { js, tab })).result;
  }
  const open = (url, extra = {}) => api('open', { tab: 1, url, ...extra });
  const status = tab => api('status?tab=' + tab);

  let ready = false;
  for (let attempt = 0; attempt < 200 && daemon.exitCode === null; attempt++) {
    if (await status(1).then(result => result.ok, () => false)) {
      ready = true;
      break;
    }
    await delay(100);
  }
  assert(ready, 'daemon did not start');

  const fastStart = performance.now();
  assert.equal(ok(await open(base + '/fast')).tab, 1);
  const fastMs = performance.now() - fastStart;
  assert.equal(await evaluate('document.body.dataset.loaded'), 'yes');
  assert(fastMs < 1200, `fast open took ${fastMs.toFixed(0)} ms`);

  const zoom = ok(await api('zoom', { tab: 1, action: 'in' })).zoom;
  assert(zoom > 1);
  ok(await open(base + '/slow'));
  assert.deepEqual(await evaluate('([document.readyState, window.slowScriptRan, Number(document.body.style.zoom)])'),
    ['complete', true, zoom]);
  assert.match(ok(await open(base + '/redirect')).message, /now at .*\/fast/);

  const visits = requests.get('/fast');
  for (const fragment of ['#one', '#two', '#two']) {
    assert.match(ok(await open(base + '/fast' + fragment)).message, /same-document/);
    assert.equal(await evaluate('location.hash'), fragment);
    assert.equal(await evaluate('Number(document.body.style.zoom)'), zoom);
  }
  assert.equal(requests.get('/fast'), visits);
  ok(await open('about:blank'));
  assert.equal(ok(await status(1)).url, base + '/fast#two');
  const blank = ok(await open('about:blank', { tab: null, new_tab: true }));
  assert.notEqual(blank.tab, 1);
  assert.equal(ok(await status(blank.tab)).url, 'about:blank');
  const data = 'data:text/html,<p>fixture</p>';
  ok(await open(data, { tab: blank.tab }));
  assert.equal(await evaluate('document.body.textContent', blank.tab), 'fixture');
  assert.match(ok(await open(data + '#marker', { tab: blank.tab })).message, /same-document/);
  assert.equal(await evaluate('location.hash', blank.tab), '#marker');

  const failed = await open(base + '/failed');
  assert.equal(failed.ok, false);
  assert.match(failed.error, /goto failed after 3 attempts/);
  assert.match(ok(await status(1)).url, /^chrome-error:/);
  assert.match(ok(await api('nav', { tab: 1, action: 'reload' })).message, /recovered to .*\/fast#two/);
  assert.equal(await evaluate('Number(document.body.style.zoom)'), zoom);

  const firstOwner = { 'x-lb-session': 'wait-test-a', 'x-lb-isolated': 'true' };
  const secondOwner = { 'x-lb-session': 'wait-test-b' };
  const owned = ok(await api('open', { url: base + '/fast' }, firstOwner)).tab;
  assert.notEqual(owned, 1);
  assert.equal(ok(await api('open', { url: base + '/slow' }, firstOwner)).tab, owned);
  assert.match((await api('open', { url: base + '/fast', tab: owned }, secondOwner)).error,
    /belongs to another agent session/);

  // CSS clicks resolve element geometry in the foreground renderer.
  ok(await api('activate', { tab: 1 }));
  ok(await api('click', { tab: 1, target: 'css=#click' }));
  assert.equal(await evaluate('document.querySelector("#clicks").value'), '1');
  const waitStart = performance.now();
  ok(await api('wait', { tab: 1, target: 'css=#delayed', timeout: 1 }));
  const waitMs = performance.now() - waitStart;
  await evaluate('setTimeout(() => document.querySelector("#delayed").remove(), 120)');
  ok(await api('wait', { tab: 1, target: 'css=#delayed', gone: true, timeout: 1 }));
  assert.match((await api('wait', { tab: 1, target: 'css=[', gone: true, timeout: 1 })).error,
    /wait timed out after 1s/);
  console.log(`Browser wait regressions passed (fast open ${fastMs.toFixed(0)} ms, delayed selector ${waitMs.toFixed(0)} ms)`);
} catch (error) {
  console.error(logs);
  throw error;
} finally {
  if (daemon && daemon.exitCode === null && daemon.signalCode === null && daemon.pid) {
    daemon.kill('SIGTERM');
    const timer = new AbortController();
    try {
      await Promise.race([exited, delay(22000, undefined, { signal: timer.signal }).then(() => {
        process.kill(-daemon.pid, 'SIGKILL');
        return exited;
      })]);
    } finally {
      timer.abort();
    }
  }
  fixture.closeAllConnections();
  fixture.close();
  await rm(scratch, { recursive: true, force: true });
}
