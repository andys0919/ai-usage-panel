/* Dev-only preview: fakes the Tauri bridge so the UI can be opened in any browser.
   index.html?mock=live    -> every account healthy (numbers like the real ones)
   index.html?mock=states  -> one card per state, to check every visual state */
const params = new URLSearchParams(location.search);
const variant = params.get('mock') === 'states' ? 'states' : 'live';

const now = Date.now();
const min = (m) => now + m * 60_000;
const win = (kind, used, resetsIn, extra = {}) => ({
  kind,
  group: null,
  scope: null,
  used_percent: used,
  resets_at_ms: min(resetsIn),
  window_minutes: kind === 'session' ? 300 : 10080,
  ...extra,
});
const account = (key, provider, label, extra = {}) => ({
  key,
  provider,
  label,
  detail: null,
  plan: null,
  windows: [],
  extras: [],
  state: 'ok',
  message: null,
  fix_command: null,
  fetched_at_ms: now - 40_000,
  token_refreshed: false,
  ...extra,
});

const ORG = 'Example Corp';
const gemini = (extra = {}) =>
  account('gemini:antigravity', 'gemini', 'personal.max@example.com', {
    detail: 'Antigravity CLI',
    windows: [
      win('weekly', 61, 33, { group: 'Gemini Models' }),
      win('session', 2, 190, { group: 'Gemini Models' }),
      win('weekly', 34, 7300, { group: 'Claude and GPT models' }),
      win('session', 0, 300, { group: 'Claude and GPT models' }),
    ],
    ...extra,
  });

const LIVE = [
  account('claude:work', 'claude', 'work.account@example.com', {
    plan: 'Team · Max 5x',
    detail: ORG,
    token_refreshed: true,
    windows: [win('session', 23, 168), win('weekly', 41, 4600), win('weekly', 0, 4600, { scope: 'Fable' })],
  }),
  account('claude:personal', 'claude', 'personal.max@example.com', {
    plan: 'Max 20x',
    detail: 'Personal workspace',
    windows: [win('session', 8, 205), win('weekly', 17, 7400), win('weekly', 0, 7400, { scope: 'Fable' })],
  }),
  account('claude:team', 'claude', 'team.member@example.com', {
    plan: 'Team · Max 5x',
    detail: ORG,
    windows: [win('session', 12, 138), win('weekly', 22, 7920), win('weekly', 0, 7920, { scope: 'Fable' })],
  }),
  account('codex:work', 'codex', 'work.account@example.com', {
    plan: 'Team',
    extras: [{ key: 'reset_credits', value: '3' }],
    windows: [win('session', 2, 277), win('weekly', 11, 3400)],
  }),
  account('codex:system-default', 'codex', 'personal.max@example.com', {
    plan: 'Pro',
    extras: [{ key: 'reset_credits', value: '2' }],
    windows: [win('weekly', 60, 7390)],
  }),
  gemini(),
];

const STATES = [
  account('claude:a', 'claude', 'busy.account@example.com', {
    plan: 'Max 20x',
    detail: 'Personal',
    windows: [win('session', 93, 41), win('weekly', 74, 2900), win('weekly', 100, 2900, { scope: 'Fable' })],
  }),
  account('claude:b', 'claude', 'expired.token@example.com', {
    plan: 'Team · Max 5x',
    detail: ORG,
    state: 'token_expired',
    fetched_at_ms: null,
    message: 'access token 已過期,而且「自動更新 token」目前是關閉的',
  }),
  account('claude:c', 'claude', 'stale.numbers@example.com', {
    plan: 'Pro',
    state: 'stale',
    fetched_at_ms: now - 25 * 60_000,
    message: 'token 伺服器暫時限流(HTTP 429)(約 5 分鐘後自動再試)',
    windows: [win('session', 40, 120), win('weekly', 55, 5000)],
  }),
  account('codex:d', 'codex', 'login.needed@example.com', {
    plan: 'Team',
    state: 'login_required',
    fetched_at_ms: null,
    message: 'refresh token 被拒絕(HTTP 400 invalid_grant)。請用 login.needed@example.com 重新登入',
    fix_command: 'orca account add --agent codex',
  }),
  account('codex:e', 'codex', 'rate.limited@example.com', {
    plan: 'Plus',
    state: 'waiting',
    fetched_at_ms: null,
    message: 'token 伺服器暫時限流(HTTP 429)(約 5 分鐘後自動再試)',
  }),
  account('codex:h', 'codex', 'broken@example.com', {
    plan: 'Plus',
    state: 'error',
    fetched_at_ms: null,
    message: '回應不像用量資料(沒有 plan_type)',
  }),
  account('gemini:f', 'gemini', 'loading@example.com', { detail: 'Antigravity CLI', state: 'loading', fetched_at_ms: null }),
  account('claude:g', 'claude', 'warning.writeback@example.com', {
    plan: 'Max 5x',
    token_refreshed: true,
    message: 'token 已更新,但寫回憑證檔失敗(PermissionDenied);新 token 已另存到 C:\\Users\\me\\AppData\\Roaming\\ai-usage-panel\\recovery\\x.json',
    windows: [win('session', 5, 250), win('weekly', 12, 8000)],
  }),
];

const state = {
  accounts: variant === 'states' ? STATES : LIVE,
  cycle_running: false,
  cycle_started_ms: now - 40_000,
  next_cycle_ms: now - 40_000 + 300_000,
  manual_allowed_at_ms: now - 40_000 + 60_000,
  interval_secs: 300,
  auto_refresh_tokens: variant !== 'states',
  allowed_intervals_secs: [300, 600, 900, 1800],
};

const listeners = [];
const emit = () => listeners.forEach((cb) => cb({ payload: structuredClone(state) }));

window.__TAURI__ = {
  core: {
    invoke: async (cmd, args) => {
      if (cmd === 'get_state') return structuredClone(state);
      if (cmd === 'refresh_now') {
        if (Date.now() < state.manual_allowed_at_ms) throw new Error('官方會限流,請稍後再按');
        state.cycle_running = true;
        state.cycle_started_ms = Date.now();
        setTimeout(() => {
          state.cycle_running = false;
          state.accounts.forEach((a) => { if (a.state === 'ok') a.fetched_at_ms = Date.now(); });
          state.next_cycle_ms = state.cycle_started_ms + state.interval_secs * 1000;
          state.manual_allowed_at_ms = state.cycle_started_ms + 60_000;
          emit();
        }, 1500);
        return structuredClone(state);
      }
      if (cmd === 'set_settings') {
        state.interval_secs = args.intervalSecs;
        state.auto_refresh_tokens = args.autoRefreshTokens;
        state.next_cycle_ms = state.cycle_started_ms + state.interval_secs * 1000;
        return structuredClone(state);
      }
      throw new Error(`unknown command ${cmd}`);
    },
  },
  event: {
    listen: async (_name, cb) => {
      listeners.push(cb);
      return () => {};
    },
  },
};
