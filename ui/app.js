/* AI 用量面板 — UI logic. The Rust side owns all data and scheduling; this file only renders. */
(async () => {
  'use strict';

  // Dev preview in a plain browser: open index.html?mock=live or ?mock=states
  if (new URLSearchParams(location.search).has('mock')) {
    await import('./dev/mock.js');
  }

  const tauri = window.__TAURI__;
  if (!tauri) {
    document.body.innerHTML = '<p class="fatal">找不到桌面程式的執行環境,請用 AI Usage Panel 開啟這個畫面。</p>';
    return;
  }
  const invoke = (cmd, args) => tauri.core.invoke(cmd, args);

  const $ = (id) => document.getElementById(id);
  const els = {
    grid: $('grid'),
    toast: $('toast'),
    txtLast: $('txt-last'),
    txtNext: $('txt-next'),
    pillNext: $('pill-next'),
    pillAttn: $('pill-attn'),
    txtAttn: $('txt-attn'),
    btnRefresh: $('btn-refresh'),
    lblRefresh: $('lbl-refresh'),
    btnSettings: $('btn-settings'),
    settings: $('settings'),
    selInterval: $('sel-interval'),
    chkToken: $('chk-token'),
  };

  const PROVIDERS = {
    claude: { name: 'Claude', icon: 'i-claude' },
    codex: { name: 'Codex', icon: 'i-codex' },
    gemini: { name: 'Gemini', icon: 'i-gemini' },
  };
  const GROUP_NAMES = {
    'Gemini Models': 'Gemini 模型',
    'Claude and GPT models': 'Claude 與 GPT 模型',
  };
  const BADGES = {
    ok: { cls: 'ok', icon: 'i-check', text: '已更新' },
    loading: { cls: 'idle', icon: 'i-spin', text: '載入中' },
    stale: { cls: 'warn', icon: 'i-clock', text: '舊資料' },
    waiting: { cls: 'warn', icon: 'i-clock', text: '等待重試' },
    token_expired: { cls: 'warn', icon: 'i-key', text: 'token 過期' },
    login_required: { cls: 'bad', icon: 'i-login', text: '需要登入' },
    error: { cls: 'bad', icon: 'i-alert', text: '取得失敗' },
  };
  const NEEDS_ATTENTION = new Set(['token_expired', 'login_required', 'error']);

  let snap = null;
  const cards = new Map(); // account key -> { el, sig }
  const lastWidths = new Map(); // meter id -> last rendered percent (for bar animation)
  let toastTimer = 0;
  let tickTimer = 0;

  // ── formatting ────────────────────────────────────────────────────────────
  const esc = (s) =>
    String(s ?? '').replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[c]);
  const icon = (id) => `<svg class="ic" aria-hidden="true"><use href="#${id}"/></svg>`;

  const fmtClock = new Intl.DateTimeFormat('zh-TW', { hour: '2-digit', minute: '2-digit', hour12: false });
  const fmtClockSec = new Intl.DateTimeFormat('zh-TW', { hour: '2-digit', minute: '2-digit', second: '2-digit', hour12: false });

  const WEEKDAYS = ['日', '一', '二', '三', '四', '五', '六'];
  const pad2 = (n) => String(n).padStart(2, '0');
  const startOfDay = (d) => new Date(d.getFullYear(), d.getMonth(), d.getDate()).getTime();

  /** "今天 13:22" / "明天 09:00" / "10/6(週二) 12:01": a weekly reset must show its date. */
  function fmtAt(ms, now) {
    const d = new Date(ms);
    const clock = `${pad2(d.getHours())}:${pad2(d.getMinutes())}`;
    const days = Math.round((startOfDay(d) - startOfDay(new Date(now))) / 86400000);
    if (days === 0) return `今天 ${clock}`;
    if (days === 1) return `明天 ${clock}`;
    return `${d.getMonth() + 1}/${d.getDate()}(週${WEEKDAYS[d.getDay()]}) ${clock}`;
  }

  function fmtDur(ms) {
    const m = Math.round(ms / 60000);
    if (m < 1) return '不到 1 分鐘';
    if (m < 60) return `${m} 分鐘`;
    const h = Math.floor(m / 60);
    const mm = m % 60;
    if (h < 24) return mm ? `${h} 小時 ${mm} 分` : `${h} 小時`;
    const d = Math.floor(h / 24);
    const hh = h % 24;
    return hh ? `${d} 天 ${hh} 小時` : `${d} 天`;
  }

  function fmtAge(ms) {
    const s = Math.max(0, Math.round(ms / 1000));
    if (s < 10) return '剛剛';
    if (s < 60) return `${s} 秒前`;
    const m = Math.round(s / 60);
    if (m < 60) return `${m} 分鐘前`;
    const h = Math.round(m / 60);
    if (h < 48) return `${h} 小時前`;
    return `${Math.round(h / 24)} 天前`;
  }

  const spanLabel = (min) => {
    if (!min) return '其他';
    if (min % 1440 === 0) return `${min / 1440} 天`;
    if (min % 60 === 0) return `${min / 60} 小時`;
    return `${min} 分鐘`;
  };

  function windowLabel(w) {
    const base = w.kind === 'session' ? '5 小時' : w.kind === 'weekly' ? '每週' : spanLabel(w.window_minutes);
    return w.scope ? `${w.scope} ${base}` : base;
  }

  function levelOf(p) {
    if (p >= 100) return { id: 'full', text: '已用完' };
    if (p >= 90) return { id: 'high', text: '快用完' };
    if (p >= 70) return { id: 'warn', text: '偏高' };
    return { id: 'ok', text: '正常' };
  }

  const pctText = (p) => {
    const r = Math.round(p);
    return p > 0 && r === 0 ? '<1' : String(r);
  };

  // ── card rendering ────────────────────────────────────────────────────────
  const windowRank = (w) => (w.kind === 'session' ? 0 : w.kind === 'weekly' ? (w.scope ? 2 : 1) : 3);

  function groupWindows(windows) {
    const order = [];
    const map = new Map();
    for (const w of windows) {
      const g = w.group || '';
      if (!map.has(g)) {
        map.set(g, []);
        order.push(g);
      }
      map.get(g).push(w);
    }
    return order.map((g) => ({ group: g, windows: map.get(g).slice().sort((a, b) => windowRank(a) - windowRank(b)) }));
  }

  function meterHtml(a, w) {
    const id = `${a.key}|${w.group || ''}|${w.kind}|${w.scope || ''}|${w.window_minutes || ''}`;
    const p = Math.max(0, Math.min(100, w.used_percent));
    const lv = levelOf(p);
    const from = lastWidths.has(id) ? lastWidths.get(id) : 0;
    lastWidths.set(id, p);
    const label = windowLabel(w);
    const rounded = Math.round(p);
    if (w.scope) {
      // slim variant for per-model windows; the reset time lives in the tooltip
      return `
      <div class="meter compact" data-level="${lv.id}" data-resets="${w.resets_at_ms ?? ''}">
        <div class="meter-top">
          <span class="meter-label">${esc(label)}</span>
          <span class="meter-pct">${pctText(p)}<small>%</small></span>
        </div>
        <div class="bar" role="progressbar" aria-label="${esc(a.label)} ${esc(label)}用量" aria-valuemin="0" aria-valuemax="100" aria-valuenow="${rounded}" aria-valuetext="已用 ${rounded}%,${lv.text}">
          <div class="bar-fill" style="width:${from}%" data-to="${p}"></div><i class="tick t70"></i><i class="tick t90"></i>
        </div>
      </div>`;
    }
    return `
      <div class="meter" data-level="${lv.id}">
        <div class="meter-top">
          <span class="meter-label">${esc(label)}</span>
          <span class="meter-pct">${pctText(p)}<small>%</small></span>
        </div>
        <div class="bar" role="progressbar" aria-label="${esc(a.label)} ${esc(label)}用量" aria-valuemin="0" aria-valuemax="100" aria-valuenow="${rounded}" aria-valuetext="已用 ${rounded}%,${lv.text}">
          <div class="bar-fill" style="width:${from}%" data-to="${p}"></div><i class="tick t70"></i><i class="tick t90"></i>
        </div>
        <div class="meter-bot">
          <span class="level">${lv.text}</span>
          <span class="reset" data-resets="${w.resets_at_ms ?? ''}"></span>
        </div>
      </div>`;
  }

  function metersHtml(a) {
    const groups = groupWindows(a.windows);
    const titled = groups.length > 1 || (groups.length === 1 && groups[0].group);
    const parts = groups.map((g) => {
      const title = titled && g.group ? `<div class="group-title">${esc(GROUP_NAMES[g.group] || g.group)}</div>` : '';
      return title + g.windows.map((w) => meterHtml(a, w)).join('');
    });
    return `<div class="meters">${parts.join('')}</div>`;
  }

  function skeletonHtml() {
    const one = `<div>
      <div class="meter-top"><span class="sk sk-line" style="width:64px"></span><span class="sk sk-num"></span></div>
      <div class="sk sk-bar"></div>
      <div class="meter-bot"><span class="sk sk-line" style="width:40px"></span><span class="sk sk-line" style="width:120px"></span></div>
    </div>`;
    return `<div class="meters" aria-hidden="true">${one}${one}</div>`;
  }

  function calloutHtml(a) {
    if (!a.message) return '';
    let cls = '';
    let ic = 'i-alert';
    if (a.state === 'login_required') { cls = 'bad'; ic = 'i-login'; }
    else if (a.state === 'error') { cls = 'bad'; ic = 'i-alert'; }
    else if (a.state === 'token_expired') { cls = 'warn'; ic = 'i-key'; }
    else if (a.state === 'stale' || a.state === 'waiting') { cls = 'warn'; ic = 'i-clock'; }
    else { cls = 'warn'; ic = 'i-alert'; }
    const cmd = a.fix_command
      ? `<div class="cmd"><code title="${esc(a.fix_command)}">${esc(a.fix_command)}</code><button class="icon-btn" type="button" data-copy="${esc(a.fix_command)}" aria-label="複製指令 ${esc(a.fix_command)}" title="複製指令">${icon('i-copy')}</button></div>`
      : '';
    return `<div class="callout ${cls}" role="${cls === 'bad' ? 'alert' : 'note'}">${icon(ic)}<div class="callout-body"><span>${esc(a.message)}</span>${cmd}</div></div>`;
  }

  function cardInner(a) {
    const pv = PROVIDERS[a.provider] || PROVIDERS.claude;
    const b = BADGES[a.state] || BADGES.error;
    const chips = [];
    if (a.plan) chips.push(`<span class="chip">${esc(a.plan)}</span>`);
    if (a.detail) chips.push(`<span class="chip dim" title="${esc(a.detail)}">${esc(a.detail)}</span>`);
    for (const e of a.extras || []) {
      if (e.key === 'reset_credits') chips.push(`<span class="chip" title="可用的額度重置券數量">${icon('i-ticket')}重置券 ×${esc(e.value)}</span>`);
    }
    let body = '';
    if (a.state === 'loading') body = skeletonHtml();
    else if (a.windows.length) body = metersHtml(a);

    return `
      <div class="card-head">
        <span class="pv"><svg aria-hidden="true"><use href="#${pv.icon}"/></svg>${pv.name}</span>
        <span class="badge ${b.cls}">${icon(b.icon)}${b.text}</span>
      </div>
      <h2 class="acct" title="${esc(a.label)}">${esc(a.label)}</h2>
      ${chips.length ? `<div class="chips">${chips.join('')}</div>` : ''}
      ${body}
      ${a.state === 'loading' ? '' : calloutHtml(a)}
      <div class="card-foot">
        <span class="when" data-fetched="${a.fetched_at_ms ?? ''}">${icon('i-clock')}<span class="when-text"></span></span>
        ${a.token_refreshed ? '<span class="note">token 已自動更新</span>' : ''}
      </div>`;
  }

  function renderCards() {
    const accounts = snap.accounts;
    if (!accounts.length) {
      cards.clear();
      els.grid.innerHTML =
        '<div class="empty"><strong>找不到任何帳號</strong>請先用 <code>orca account add</code> 登入 Claude / Codex,或安裝並登入 <code>agy</code>(Gemini)。</div>';
      return;
    }
    const seen = new Set();
    const ordered = [];
    const changed = [];
    for (const a of accounts) {
      seen.add(a.key);
      let c = cards.get(a.key);
      if (!c) {
        c = { el: document.createElement('article'), sig: '' };
        c.el.className = 'card';
        cards.set(a.key, c);
      }
      c.el.dataset.provider = a.provider;
      c.el.dataset.state = a.state;
      const sig = JSON.stringify(a);
      if (c.sig !== sig) {
        c.el.innerHTML = cardInner(a);
        c.sig = sig;
        changed.push(c.el);
      }
      ordered.push(c.el);
    }
    for (const [key, c] of cards) {
      if (!seen.has(key)) {
        c.el.remove();
        cards.delete(key);
      }
    }
    const current = Array.from(els.grid.children);
    if (current.length !== ordered.length || current.some((n, i) => n !== ordered[i])) {
      els.grid.replaceChildren(...ordered);
    }
    // Start bar animations once the cards are in the document.
    void els.grid.offsetWidth;
    for (const el of changed) {
      el.querySelectorAll('.bar-fill').forEach((f) => {
        f.style.width = `${f.dataset.to}%`;
      });
    }
  }

  // ── header, settings, ticking texts ───────────────────────────────────────
  function renderSettings() {
    if (!els.selInterval.options.length) {
      for (const s of snap.allowed_intervals_secs) {
        const o = document.createElement('option');
        o.value = String(s);
        o.textContent = `每 ${s / 60} 分鐘`;
        els.selInterval.append(o);
      }
    }
    if (document.activeElement !== els.selInterval) els.selInterval.value = String(snap.interval_secs);
    if (document.activeElement !== els.chkToken) els.chkToken.checked = snap.auto_refresh_tokens;
  }

  function setText(node, text) {
    if (node.textContent !== text) node.textContent = text;
  }

  function tick() {
    if (!snap) return;
    const now = Date.now();

    const resetText = (t) => (t <= now ? '已重置,等下次更新' : `${fmtDur(t - now)}後重置 · ${fmtAt(t, now)}`);
    els.grid.querySelectorAll('.reset').forEach((n) => {
      if (!n.dataset.resets) return setText(n, '');
      const t = Number(n.dataset.resets);
      setText(n, resetText(t));
      n.title = new Date(t).toLocaleString('zh-TW');
    });
    els.grid.querySelectorAll('.meter.compact[data-resets]').forEach((n) => {
      if (n.dataset.resets) n.title = resetText(Number(n.dataset.resets));
    });

    const staleAfter = snap.interval_secs * 2000 + 60000;
    els.grid.querySelectorAll('.when').forEach((n) => {
      const text = n.querySelector('.when-text');
      if (!n.dataset.fetched) {
        setText(text, '尚未取得資料');
        return;
      }
      const age = now - Number(n.dataset.fetched);
      setText(text, `更新於 ${fmtAge(age)}`);
      n.classList.toggle('old', age > staleAfter);
    });

    const last = snap.cycle_started_ms;
    setText(els.txtLast, last ? `上次更新 ${fmtClock.format(last)} · ${fmtAge(now - last)}` : '尚未更新');
    els.txtLast.parentElement.title = last ? `上次更新:${fmtClockSec.format(last)}` : '';
    els.pillNext.classList.toggle('running', snap.cycle_running);
    if (snap.cycle_running) setText(els.txtNext, '更新中…');
    else {
      const left = snap.next_cycle_ms - now;
      setText(els.txtNext, left > 0 ? `下次更新:${fmtDur(left)}後` : '即將更新');
    }

    const attention = snap.accounts.filter((a) => NEEDS_ATTENTION.has(a.state)).length;
    els.pillAttn.hidden = attention === 0;
    setText(els.txtAttn, `${attention} 個帳號需要處理`);

    const wait = snap.manual_allowed_at_ms - now;
    els.btnRefresh.classList.toggle('busy', snap.cycle_running);
    if (snap.cycle_running) {
      els.btnRefresh.disabled = true;
      setText(els.lblRefresh, '更新中…');
    } else if (wait > 0) {
      els.btnRefresh.disabled = true;
      setText(els.lblRefresh, `${Math.ceil(wait / 1000)} 秒後可更新`);
    } else {
      els.btnRefresh.disabled = false;
      setText(els.lblRefresh, '立即更新');
    }
  }

  function render() {
    renderSettings();
    renderCards();
    tick();
  }

  function startTicking() {
    clearInterval(tickTimer);
    tickTimer = setInterval(tick, 1000);
    tick();
  }

  // ── actions ───────────────────────────────────────────────────────────────
  function toast(message) {
    els.toast.textContent = message;
    els.toast.hidden = false;
    clearTimeout(toastTimer);
    toastTimer = setTimeout(() => { els.toast.hidden = true; }, 4500);
  }

  els.btnRefresh.addEventListener('click', async () => {
    try {
      snap = await invoke('refresh_now');
      render();
    } catch (e) {
      toast(String(e));
    }
  });

  els.btnSettings.addEventListener('click', () => {
    const open = els.settings.hidden;
    els.settings.hidden = !open;
    els.btnSettings.setAttribute('aria-expanded', String(open));
  });

  async function saveSettings() {
    try {
      snap = await invoke('set_settings', {
        intervalSecs: Number(els.selInterval.value),
        autoRefreshTokens: els.chkToken.checked,
      });
      render();
      toast('設定已儲存');
    } catch (e) {
      toast(String(e));
      render();
    }
  }
  els.selInterval.addEventListener('change', saveSettings);
  els.chkToken.addEventListener('change', saveSettings);

  els.grid.addEventListener('click', async (ev) => {
    const btn = ev.target.closest('[data-copy]');
    if (!btn) return;
    try {
      await navigator.clipboard.writeText(btn.dataset.copy);
      toast(`已複製:${btn.dataset.copy}`);
    } catch {
      toast('複製失敗,請手動選取指令');
    }
  });

  document.addEventListener('keydown', (ev) => {
    if (ev.key === 'Escape' && !els.settings.hidden) {
      els.settings.hidden = true;
      els.btnSettings.setAttribute('aria-expanded', 'false');
      els.btnSettings.focus();
    }
  });

  // Stop the clock while the window is hidden (nothing to see, nothing to redraw).
  document.addEventListener('visibilitychange', () => {
    if (document.hidden) clearInterval(tickTimer);
    else startTicking();
  });

  // ── go ────────────────────────────────────────────────────────────────────
  try {
    snap = await invoke('get_state');
    render();
    startTicking();
    await tauri.event.listen('usage-snapshot', (e) => {
      snap = e.payload;
      render();
    });
  } catch (e) {
    document.body.innerHTML = `<p class="fatal">啟動失敗:${esc(String(e))}</p>`;
  }
})();
