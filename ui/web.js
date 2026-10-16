/* Browser bridge for the web server (ai-usage-web): stands in for the Tauri API that app.js uses.
   The desktop app pushes every change; here the page asks again every few seconds while visible. */
const POLL_MS = 5000;

async function call(cmd, args) {
  const res = await fetch(`api/${cmd}`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json', 'X-Requested-With': 'panel' },
    body: JSON.stringify(args ?? {}),
  });
  const text = await res.text();
  if (!res.ok) throw text || `HTTP ${res.status}`;
  return JSON.parse(text);
}

window.__TAURI__ = {
  core: { invoke: call },
  event: {
    listen: async (_event, handler) => {
      let last = '';
      setInterval(async () => {
        if (document.hidden) return;
        try {
          const snap = await call('get_state');
          const json = JSON.stringify(snap);
          if (json !== last) {
            last = json;
            handler({ payload: snap });
          }
        } catch {
          /* keep showing the last numbers; the next poll tries again */
        }
      }, POLL_MS);
    },
  },
};
