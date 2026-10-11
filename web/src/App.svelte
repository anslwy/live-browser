<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import Icon from './Icon.svelte';

  type Handover = { active: boolean; message: string; claimed?: boolean; requested_at?: string };
  type Cursor = { x: number; y: number; kind: string; by?: string; at: number };
  type Status = { ok: boolean; url: string; title: string; handover: Handover; zoom?: number; cursor?: Cursor };
  type SnapOpt = { value: string; text: string; selected: boolean };
  type SnapEl = { ref: number; tag: string; type: string; name: string; href: string; placeholder: string; value: string; options?: SnapOpt[] };
  type LogEntry = { at: string; action: string; detail: string };

  let status: Status | null = null;
  let connected = false;
  let shotUrl = '/api/screenshot';
  let openUrl = '';
  let snapshot: SnapEl[] = [];
  let snapHint = '';
  let snapLoading = false;
  let log: LogEntry[] = [];
  let busy = '';
  let error = '';
  let viewW = 0; // viewport CSS width, for mapping screenshot clicks → page coords
  let viewH = 0;
  let typingRef: number | null = null;
  let typeText = '';
  let typeErr = '';
  let keysOn = true;
  let keyChain: Promise<void> = Promise.resolve();

  async function fetchMetrics() {
    try {
      const r = await fetch('/api/metrics');
      const j = await r.json();
      if (j.ok && j.metrics) {
        viewW = j.metrics.w ?? 0;
        viewH = j.metrics.h ?? 0;
      }
    } catch {}
  }

  function imgToPage(clientX: number, clientY: number, img: HTMLImageElement) {
    // Maps a point on the <img> to page CSS pixels, accounting for
    // letterboxing: the image box may be taller/wider than the 4:3 frame
    // when max-height caps it, so fit the frame inside the box first.
    const rect = img.getBoundingClientRect();
    if (!rect.width || !rect.height || !viewW) return null;
    const vw = viewW;
    const vh = viewH || viewW * 0.75;
    const scale = Math.min(rect.width / vw, rect.height / vh);
    if (!isFinite(scale) || scale <= 0) return null;
    const fw = vw * scale;
    const fh = vh * scale;
    const ox = rect.left + (rect.width - fw) / 2;
    const oy = rect.top + (rect.height - fh) / 2;
    const x = ((clientX - ox) / fw) * vw;
    const y = ((clientY - oy) / fh) * vh;
    if (x < 0 || y < 0 || x > vw || y > vh) return null; // clicked a letterbox bar
    return { x, y };
  }

  async function clickAtPoint(p: { x: number; y: number }) {
    busy = 'shotclick';
    try {
      await fetch('/api/click-at', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ x: p.x, y: p.y, by: 'human' })
      });
    } finally {
      busy = '';
      await fetchStatus();
      requestShot();
      await loadSnapshot();
    }
  }

  async function shotClick(e: MouseEvent) {
    const img = e.currentTarget as HTMLImageElement;
    const p = imgToPage(e.clientX, e.clientY, img);
    if (!p) return;
    await clickAtPoint(p);
  }

  let dragStart: { x: number; y: number } | null = null;
  let shotDragging = false;
  let dragIsTouch = false;
  let lastHoverSent = 0;
  let lastHoverPos = { x: -1e9, y: -1e9 };

  function maybeHover(e: PointerEvent, img: HTMLImageElement) {
    // Plain mouse motion (no buttons) drives :hover states, tooltips, menus.
    // Throttled + distance-gated so it can't flood the daemon.
    if (e.buttons !== 0) return;
    const p = imgToPage(e.clientX, e.clientY, img);
    if (!p) return;
    const now = Date.now();
    if (now - lastHoverSent < 120) return;
    if (Math.hypot(p.x - lastHoverPos.x, p.y - lastHoverPos.y) < 4) return;
    lastHoverSent = now;
    lastHoverPos = p;
    fetch('/api/hover', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ x: p.x, y: p.y })
    }).catch(() => {});
    requestShot();
  }

  function onShotPointerDown(e: PointerEvent) {
    const img = e.currentTarget as HTMLImageElement;
    const p = imgToPage(e.clientX, e.clientY, img);
    if (!p) return;
    dragStart = p;
    shotDragging = false;
    dragIsTouch = e.pointerType === 'touch';
    try {
      img.setPointerCapture(e.pointerId);
    } catch {}
  }

  function onShotPointerMove(e: PointerEvent) {
    const img = e.currentTarget as HTMLImageElement;
    if (!dragStart) {
      maybeHover(e, img);
      return;
    }
    const p = imgToPage(e.clientX, e.clientY, img);
    if (!p) return;
    if (Math.hypot(p.x - dragStart.x, p.y - dragStart.y) > 8) shotDragging = true;
  }

  async function onShotPointerUp(e: PointerEvent) {
    const img = e.currentTarget as HTMLImageElement;
    const p = imgToPage(e.clientX, e.clientY, img);
    const start = dragStart;
    const wasTouch = dragIsTouch;
    dragStart = null;
    dragIsTouch = false;
    if (!p || !start) return;
    if (!shotDragging) {
      await clickAtPoint(p);
      return;
    }
    shotDragging = false;
    if (wasTouch) {
      // Touch drag = scroll the container under the finger (natural direction:
      // content follows the finger). Mouse drags keep mouse-drag semantics.
      busy = 'shotscroll';
      try {
        await fetch('/api/wheel', {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({ x: start.x, y: start.y, dx: start.x - p.x, dy: start.y - p.y })
        });
      } finally {
        busy = '';
        await fetchStatus();
        requestShot();
      }
      return;
    }
    busy = 'shotdrag';
    try {
      await fetch('/api/drag', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ x1: start.x, y1: start.y, x2: p.x, y2: p.y, by: 'human' })
      });
    } finally {
      busy = '';
      await fetchStatus();
      requestShot();
      await loadSnapshot();
    }
  }

  function onShotWheel(e: WheelEvent) {
    e.preventDefault();
    const img = e.currentTarget as HTMLImageElement;
    const p = imgToPage(e.clientX, e.clientY, img);
    if (!p) return;
    // deltaMode 1 = lines → px approximation
    const scale = e.deltaMode === 1 ? 16 : 1;
    fetch('/api/wheel', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ x: p.x, y: p.y, dx: e.deltaX * scale, dy: e.deltaY * scale })
    }).catch(() => {});
    requestShot();
  }

  async function zoomPage(action: 'in' | 'out' | 'reset') {
    busy = 'zoom';
    try {
      await fetch('/api/zoom', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ action })
      });
    } finally {
      busy = '';
      await fetchStatus();
      requestShot();
      await loadSnapshot();
    }
  }

  async function elSelect(ref: number, value: string) {
    busy = `select${ref}`;
    try {
      const r = await fetch('/api/select', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ target: String(ref), value })
      });
      const j = await r.json();
      if (!j.ok) typeErr = j.error ?? 'select failed';
    } catch {
      typeErr = 'daemon unreachable';
    } finally {
      busy = '';
    }
    refreshShot();
    await fetchStatus();
    await loadSnapshot();
  }

  async function elClick(ref: number) {
    busy = `click${ref}`;
    await fetch('/api/click', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ target: String(ref), by: 'human' })
    });
    busy = '';
    await fetchStatus();
    requestShot();
    await loadSnapshot();
  }

  async function elType(ref: number, submit: boolean) {
    if (!typeText && !submit) return;
    busy = `type${ref}`;
    typeErr = '';
    try {
      const r = await fetch('/api/type', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ target: String(ref), text: typeText, submit })
      });
      const j = await r.json();
      if (!j.ok) {
        typeErr = j.error ?? 'type failed';
        return;
      }
    } catch (e) {
      typeErr = 'daemon unreachable';
      return;
    } finally {
      busy = '';
    }
    typeText = '';
    typingRef = null;
    requestShot();
    await loadSnapshot();
  }

  function keyTargetOk(): boolean {
    if (!keysOn) return false;
    const t = document.activeElement as HTMLElement | null;
    if (t && (t.tagName === 'INPUT' || t.tagName === 'TEXTAREA' || t.tagName === 'SELECT' || t.isContentEditable)) {
      return false; // typing in the monitor UI itself — don't hijack
    }
    return true;
  }

  function forwardKey(e: KeyboardEvent, up: boolean) {
    if (!keyTargetOk()) return;
    if (e.ctrlKey || e.metaKey || e.altKey) return; // leave shortcuts to the browser
    if (e.key === 'Unidentified' || e.keyCode === 229) return; // IME / dead keys
    e.preventDefault();
    const payload = {
      event: up ? 'up' : 'down',
      key: e.key,
      code: e.code,
      key_code: e.keyCode || 0,
      text: !up && e.key.length === 1 ? e.key : '',
      repeat: e.repeat,
      modifiers: (e.shiftKey ? 8 : 0)
    };
    // Chain to preserve key order; fire-and-forget per keystroke.
    // Every completed keystroke queues a fresh frame so typing feels live.
    keyChain = keyChain
      .then(() => fetch('/api/key', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(payload)
      }).then(() => {
        if (up) {
          requestShot();
          if (e.key === 'Enter' || e.key === 'Tab') {
            fetchStatus();
            loadSnapshot();
          }
        }
      }))
      .catch(() => {});
  }

  function onKeyDown(e: KeyboardEvent) { forwardKey(e, false); }
  function onKeyUp(e: KeyboardEvent) { forwardKey(e, true); }

  function attachShotWheel() {
    // Non-passive so we can preventDefault: the wheel scrolls the page under
    // the cursor, never the monitor UI itself.
    const img = document.getElementById('live-shot');
    img?.addEventListener('wheel', onShotWheel as EventListener, { passive: false });
  }

  type TabInfo = { id: number; active: boolean; title: string; url: string; handover: boolean };
  let tabs: TabInfo[] = [];

  async function fetchTabs() {
    try {
      const r = await fetch('/api/tabs');
      const j = await r.json();
      if (j.ok) tabs = j.tabs ?? [];
    } catch {}
  }

  async function activateTab(id: number) {
    busy = `tab${id}`;
    try {
      await fetch('/api/activate', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ tab: id })
      });
    } finally {
      busy = '';
      await refreshAll();
    }
  }

  async function closeTab(id: number) {
    busy = `closetab${id}`;
    try {
      await fetch('/api/close', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ tab: id })
      });
    } finally {
      busy = '';
      await refreshAll();
    }
  }

  async function newTab() {
    busy = 'newtab';
    try {
      await fetch('/api/open', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ url: openUrl.trim() || 'about:blank' })
      });
      openUrl = '';
    } finally {
      busy = '';
      await refreshAll();
    }
  }

  async function refreshAll() {
    await fetchStatus();
    await fetchMetrics();
    await fetchTabs();
    requestShot();
    await loadSnapshot();
    await fetchLog();
  }

  let timer: ReturnType<typeof setInterval>;

  async function fetchStatus() {
    try {
      const r = await fetch('/api/status');
      status = await r.json();
      connected = true;
      error = '';
    } catch (e) {
      connected = false;
      error = 'daemon unreachable — is `lb daemon` running?';
    }
  }

  async function fetchLog() {
    try {
      const r = await fetch('/api/log');
      const j = await r.json();
      if (j.ok) log = j.entries ?? [];
    } catch {}
  }

  let shotLoading = false;
  let shotPending = false;
  let shotImg: HTMLImageElement | null = null;
  let imgBox = { w: 0, h: 0 };

  function measureShot() {
    if (!shotImg) return;
    const r = shotImg.getBoundingClientRect();
    imgBox = { w: r.width, h: r.height };
  }

  // Fake cursor position as % of the <img> box (mirrors imgToPage's fit math).
  // Null when there is no recent pointer action.
  $: cursorPct = (() => {
    const c = status?.cursor;
    if (!c || !c.at || c.by === 'human' || !imgBox.w || !imgBox.h || !viewW) return null;
    if (Date.now() - c.at > 8000) return null;
    const vw = viewW;
    const vh = viewH || viewW * 0.75;
    const scale = Math.min(imgBox.w / vw, imgBox.h / vh);
    if (!isFinite(scale) || scale <= 0) return null;
    const fw = vw * scale;
    const fh = vh * scale;
    const ox = (imgBox.w - fw) / 2;
    const oy = (imgBox.h - fh) / 2;
    return {
      x: ((ox + c.x * scale) / imgBox.w) * 100,
      y: ((oy + c.y * scale) / imgBox.h) * 100,
      kind: c.kind || 'click',
      key: c.at
    };
  })();

  // Coalescing refresh: at most one screenshot request in flight; every
  // interaction marks a frame pending and it fires ASAP after the current one
  // lands. Frames preload off-DOM and swap on decode (no blink, no jump).
  function requestShot() {
    shotPending = true;
    pumpShot();
  }

  function pumpShot() {
    if (shotLoading || !shotPending) return;
    shotPending = false;
    shotLoading = true;
    const url = `/api/screenshot?t=${Date.now()}`;
    const pre = new Image();
    pre.onload = () => {
      shotUrl = url;
      shotLoading = false;
      pumpShot();
    };
    pre.onerror = () => {
      shotLoading = false;
      pumpShot();
    };
    pre.src = url;
  }

  // Theme (persisted): applied pre-paint by index.html, mirrored here.
  let theme: 'dark' | 'light' =
    typeof document !== 'undefined' && document.documentElement.dataset.theme === 'light'
      ? 'light'
      : 'dark';

  function setTheme(t: 'dark' | 'light') {
    theme = t;
    document.documentElement.dataset.theme = t;
    try {
      localStorage.setItem('lb-theme', t);
    } catch {}
  }

  async function doOpen() {
    if (!openUrl.trim()) return;
    busy = 'open';
    await fetch('/api/open', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ url: openUrl.trim() })
    });
    busy = '';
    await refreshAll();
  }

  async function loadSnapshot() {
    snapLoading = true;
    try {
      const r = await fetch('/api/snapshot');
      const j = await r.json();
      if (j.ok) {
        snapshot = j.elements ?? [];
        snapHint = j.hint ?? '';
      }
    } finally {
      snapLoading = false;
    }
  }

  async function nav(action: string) {
    busy = action;
    await fetch('/api/nav', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ action })
    });
    busy = '';
    await refreshAll();
  }

  async function handoverDone() {
    busy = 'done';
    await fetch('/api/handover/done', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: '{}' });
    busy = '';
    await refreshAll();
  }

  async function claimControl() {
    busy = 'claim';
    await fetch('/api/handover/claim', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: '{}' });
    busy = '';
    await refreshAll();
  }

  onMount(() => {
    fetchStatus();
    fetchLog();
    fetchMetrics();
    fetchTabs();
    loadSnapshot();
    requestShot();
    window.addEventListener('keydown', onKeyDown);
    window.addEventListener('keyup', onKeyUp);
    window.addEventListener('resize', measureShot);
    attachShotWheel();
    // Liveness tick — always on. Interactions queue their own frames on top.
    timer = setInterval(async () => {
      await fetchStatus();
      measureShot();
      requestShot();
      await fetchLog();
      await fetchTabs();
    }, 2000);
  });

  onDestroy(() => {
    clearInterval(timer);
    window.removeEventListener('keydown', onKeyDown);
    window.removeEventListener('keyup', onKeyUp);
    window.removeEventListener('resize', measureShot);
  });
</script>

<div class="wrap">
  <header class="constrain">
    <span class="dot {connected ? '' : 'off'}"></span>
    <h2 style="margin:0"><span class="brand"><Icon name="globe" /></span> Live Browser</h2>
    <span class="muted mono status">{connected ? 'connected' : 'offline'}</span>
    <span style="flex:1"></span>
    <button class="ghost iconbtn" on:click={() => setTheme(theme === 'dark' ? 'light' : 'dark')} title={theme === 'dark' ? 'Switch to light mode' : 'Switch to dark mode'} aria-label="Toggle light / dark mode">
      <Icon name={theme === 'dark' ? 'sun' : 'moon'} />
    </button>
  </header>

  {#if error}
    <div class="card constrain">{error}</div>
  {/if}

  {#if tabs.length > 0}
    <div class="tabstrip constrain" role="tablist" aria-label="Browser tabs">
      {#each tabs as t}
        <button
          role="tab"
          aria-selected={t.active}
          class="tabpill"
          class:active={t.active}
          class:needs-human={t.handover}
          title={t.url}
          on:click={() => !t.active && activateTab(t.id)}>
          <span class="tabtitle">{t.title || t.url || `Tab ${t.id}`}</span>
          {#if t.handover}<span class="tabflag" title="Needs human">!</span>{/if}
          <span
            class="tabclose"
            role="button"
            aria-label="Close tab {t.id}"
            title="Close tab (agents lose control of it)"
            on:click|stopPropagation={() => closeTab(t.id)}>×</span>
        </button>
      {/each}
      <button class="tabnew" on:click={newTab} disabled={busy === 'newtab'} title="New tab (uses the address bar, or blank)" aria-label="New tab">+</button>
    </div>
  {/if}

  {#if status?.handover?.active}
    <div class="banner constrain">
      {#if status.handover.claimed}
        <strong><Icon name="hand" /> You're in control</strong>
      {:else}
        <strong><Icon name="bell" /> Agent needs you — human takeover</strong>
      {/if}
      <p>{status.handover.message}</p>
      <p class="muted mono">Steps: click / type directly below — no separate browser window needed — then:</p>
      <div class="row">
        <button on:click={handoverDone} disabled={busy === 'done'}>
          {busy === 'done' ? 'Returning…' : 'Done — return to agent'}
        </button>
        <button class="ghost" on:click={requestShot}>Refresh view</button>
      </div>
    </div>
  {/if}

  <div class="card constrain">
    <div class="row browserbar">
      <button class="iconplain" on:click={() => nav('back')} title="Back"><Icon name="arrow-left" /></button>
      <button class="iconplain" on:click={() => nav('reload')} title="Reload"><Icon name="reload" /></button>
      <button class="iconplain" on:click={() => nav('forward')} title="Forward"><Icon name="arrow-right" /></button>
      <input class="omnibox" type="text" placeholder="Type a URL or search…" bind:value={openUrl} on:keydown={(e) => e.key === 'Enter' && doOpen()} />
    </div>
    {#if status}
      <p class="mono page-meta" style="margin:8px 0 0">
        <span class="title"><Icon name="doc" /> {status.title || '(no title)'}</span>
        <a class="url" href={status.url}>{status.url}</a>
      </p>
    {/if}
  </div>

  <div class="card constrain">
    <div class="row" style="justify-content:flex-end">
      <span class="livepill" title="Streaming screenshots"><span class="livedot"></span>LIVE</span>
    </div>
        <!-- Single persistent <img>: src swaps only after the new frame decodes. -->
        <div class="shotwrap">
          <img
            id="live-shot"
            bind:this={shotImg}
            class="shot actionable"
            src={shotUrl}
            alt="live page view"
            draggable="false"
            on:dragstart={(e) => e.preventDefault()}
            on:pointerdown={onShotPointerDown}
            on:pointermove={onShotPointerMove}
            on:pointerup={onShotPointerUp}
            on:load={measureShot} />
          {#if cursorPct}
            {#key cursorPct.key}
              <div class="cursorlayer" aria-hidden="true">
                <div class="fakecursor" style="left:{cursorPct.x}%;top:{cursorPct.y}%">
                  <svg viewBox="0 0 24 24" width="22" height="22"><path d="M5 3l14 7.5-6.2 1.8L9.5 18.5z" fill="#111" stroke="#fff" stroke-width="1.6" stroke-linejoin="round" /></svg>
                  <span class="ripple {cursorPct.kind === 'drag' ? 'drag' : 'click'}"></span>
                </div>
              </div>
            {/key}
          {/if}
        </div>
        <div class="row" style="margin-top:8px">
          <span class="zoomgroup" role="group" aria-label="Page zoom">
            <button class="ghost" on:click={() => zoomPage('out')} disabled={busy === 'zoom'} title="Zoom out (fit more)">−</button>
            <button class="ghost zoompct" on:click={() => zoomPage('reset')} disabled={busy === 'zoom'} title="Reset zoom to 100%">{Math.round((status?.zoom ?? 1) * 100)}%</button>
            <button class="ghost" on:click={() => zoomPage('in')} disabled={busy === 'zoom'} title="Zoom in">+</button>
          </span>
      <button class:ghost={!keysOn} on:click={() => (keysOn = !keysOn)} title={keysOn ? 'Keyboard capture is ON — your keystrokes go to the page. Click to turn off.' : 'Turn on keyboard capture to type into the page.'}>
        <Icon name="keyboard" /> {keysOn ? 'Keys: ON' : 'Keys: off'}
      </button>
    </div>
  </div>

  {#if status && !status.handover.active}
    <div class="banner done slim constrain">
      <div class="row banner-head" style="justify-content:space-between">
        <span class="url-label"><Icon name="check-circle" /> Agent in control — <span class="mono">{status.url}</span></span>
        <button class="ghost" on:click={claimControl} disabled={busy === 'claim'} title="Pause the agent and drive the page yourself — click or drag the screenshot, scroll with your wheel, use the snapshot buttons — then click Done to return control. The agent can also hand over explicitly when it needs you.">
          <Icon name="hand" /> {busy === 'claim' ? 'Claiming…' : 'Take control'}
        </button>
      </div>
    </div>
  {/if}

  <details class="card collapsible constrain">
    <summary>
      <strong>Snapshot (agent view)</strong>
      <span class="muted mono">{snapshot.length ? `${snapshot.length} elements` : ''}</span>
    </summary>
    <div class="row" style="justify-content:flex-end;margin-bottom:8px">
      <button class="ghost" on:click={loadSnapshot} disabled={snapLoading}>{snapLoading ? '…' : 'Refresh'}</button>
    </div>
    {#if snapshot.length === 0}
      <p class="muted">No elements yet — open a page, then refresh.</p>
    {:else}
      <table class="snap">
        <thead><tr><th>ref</th><th>element</th><th></th></tr></thead>
        <tbody>
          {#each snapshot.slice(0, 60) as el}
            <tr>
              <td class="mono">[{el.ref}]</td>
              <td>{el.tag} “{el.name}”</td>
                  <td class="mono" style="white-space:nowrap">
                    <button class="mini" on:click={() => elClick(el.ref)} disabled={busy === `click${el.ref}`}>click</button>
                    {#if ['input', 'textarea'].includes(el.tag)}
                      <button class="mini ghost" on:click={() => { typingRef = typingRef === el.ref ? null : el.ref; typeText = ''; }}>type</button>
                    {/if}
                    {#if el.tag === 'select' && el.options?.length}
                      <select
                        class="rowsel"
                        value={el.options.find((o) => o.selected)?.value ?? ''}
                        disabled={busy === `select${el.ref}`}
                        on:change={(e) => elSelect(el.ref, (e.target as HTMLSelectElement).value)}>
                        {#each el.options as o}
                          <option value={o.value} selected={o.selected}>{o.text || o.value}</option>
                        {/each}
                      </select>
                    {/if}
                  </td>
            </tr>
            {#if typingRef === el.ref}
              <tr>
                <td></td>
                <td colspan="2">
                  <div class="row">
                    <input
                      type="text"
                      placeholder="type into [{el.ref}]…"
                      bind:value={typeText}
                      on:keydown={(e) => { if (e.key === 'Enter') elType(el.ref, false); }} />
                  </div>
                  <div class="row" style="margin-top:6px">
                    <button class="mini" on:click={() => elType(el.ref, false)} disabled={busy === `type${el.ref}`}>Send</button>
                    <button class="mini ghost" on:click={() => elType(el.ref, true)} disabled={busy === `type${el.ref}`}>Send + Enter ⏎</button>
                  </div>
                      {#if typeErr}<p class="mono" style="color:var(--danger);margin:6px 0 0"><Icon name="warn" /> {typeErr}</p>{/if}
                </td>
              </tr>
            {/if}
          {/each}
        </tbody>
      </table>
      {#if snapshot.length > 60}<p class="muted">… +{snapshot.length - 60} more (CLI shows all)</p>{/if}
    {/if}
    {#if snapHint}<p class="muted mono">{snapHint}</p>{/if}
  </details>

  <details class="card collapsible constrain">
    <summary><strong>Action log</strong></summary>
    <ul class="log mono">
      {#each [...log].reverse().slice(0, 15) as e}
        <li>{e.at} · {e.action} {e.detail}</li>
      {/each}
    </ul>
  </details>

  <details class="card collapsible constrain">
    <summary><strong>Agent CLI cheatsheet</strong></summary>
    <p class="muted mono" style="margin:0">
      lb open &lt;url&gt; · lb snapshot · lb click &lt;ref&gt; · lb type &lt;ref&gt; &lt;text&gt; · lb handover "msg"
    </p>
  </details>
</div>
