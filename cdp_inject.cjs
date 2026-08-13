const CDP_PORT = process.env.CDP_PORT || '9333';
const SLUGS = ['deepseek-v4-flash', 'deepseek-v4-pro'];
const PATCH = `(() => {
  const __slugs = ${JSON.stringify(SLUGS)};
  const __orig = Set.prototype.has;
  Set.prototype.has = function (v) {
    if (typeof v === 'string' && __slugs.includes(v)) return true;
    return __orig.call(this, v);
  };
})();`;

async function getTargets() {
  const r = await fetch(`http://127.0.0.1:${CDP_PORT}/json/list`);
  return r.json();
}

function connect(wsUrl) {
  return new Promise((resolve, reject) => {
    const ws = new WebSocket(wsUrl);
    const pending = new Map();
    let id = 0;
    ws.onopen = () => {
      resolve({
        ws,
        send(method, params = {}) {
          return new Promise((res, rej) => {
            const mid = ++id;
            pending.set(mid, { res, rej });
            ws.send(JSON.stringify({ id: mid, method, params }));
          });
        },
        pending,
      });
    };
    ws.onerror = (e) => reject(e);
    ws.onmessage = (ev) => {
      const msg = JSON.parse(ev.data);
      if (msg.id && pending.has(msg.id)) {
        const p = pending.get(msg.id);
        pending.delete(msg.id);
        if (msg.error) p.rej(new Error(JSON.stringify(msg.error)));
        else p.res(msg.result);
      }
    };
  });
}

async function inject(t) {
  const c = await connect(t.webSocketDebuggerUrl);
  await c.send('Page.enable');
  await c.send('Runtime.enable');
  const add = await c.send('Page.addScriptToEvaluateOnNewDocument', { source: PATCH });
  console.log('INJECTED', t.type, JSON.stringify(t.title), t.url, 'identifier', add.identifier);
  await c.send('Page.reload', { ignoreCache: true });
  c.ws.close();
}

async function main() {
  let targets = [];
  for (let i = 0; i < 80; i++) {
    try {
      targets = await getTargets();
      if (targets.length) break;
    } catch {}
    await new Promise((r) => setTimeout(r, 500));
  }
  if (!targets.length) {
    console.error('NO_CDP_TARGETS');
    process.exit(2);
  }
  console.log('TARGETS');
  for (const t of targets) {
    console.log(JSON.stringify({ id: t.id, type: t.type, title: t.title, url: t.url }));
  }
  const candidates = targets.filter(
    (t) =>
      t.type === 'page' || t.type === 'webview' || /index\.html|codex|chatgpt/i.test(t.url || '')
  );
  if (!candidates.length) {
    console.error('NO_CODEX_PAGE_TARGET');
    process.exit(3);
  }
  for (const t of candidates) {
    try {
      await inject(t);
    } catch (e) {
      console.log('INJECT_FAIL', t.title, e.message);
    }
  }
  console.log('DONE');
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
