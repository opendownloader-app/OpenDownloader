// Minimal CDP client: no Playwright, so blob workers and captcha iframes cannot hang it.
const PORT = 9334;
const list = await (await fetch(`http://127.0.0.1:${PORT}/json/list`)).json();
const pick = (pred) => list.find(pred);

function connect(ws) {
  const sock = new WebSocket(ws);
  let id = 0;
  const pending = new Map();
  const ready = new Promise((res) => (sock.onopen = res));
  sock.onmessage = (e) => {
    const m = JSON.parse(e.data);
    if (m.id && pending.has(m.id)) { pending.get(m.id)(m); pending.delete(m.id); }
  };
  return {
    ready,
    send: (method, params = {}) =>
      new Promise((res) => { const i = ++id; pending.set(i, res); sock.send(JSON.stringify({ id: i, method, params })); }),
    close: () => sock.close(),
  };
}

async function evaluate(target, expression, userGesture = false) {
  const c = connect(target.webSocketDebuggerUrl);
  await c.ready;
  const r = await c.send("Runtime.evaluate", { expression, awaitPromise: true, returnByValue: true, userGesture });
  c.close();
  return r.result?.result?.value ?? r.result?.exceptionDetails?.text ?? null;
}

const mgr = pick((t) => t.url.startsWith("chrome-extension://") && t.url.endsWith("manager.html"));
const dy = pick((t) => t.type === "page" && t.url.includes("douyin.com/jingxuan"));
console.log("manager target:", !!mgr, " douyin target:", !!dy);

// userGesture:true is what makes permissions.request legal here.
const granted = await evaluate(mgr, `chrome.permissions.request({origins:[
  "https://*.douyin.com/*","*://*.zjcdn.com/*","*://*.douyinvod.com/*","*://*.bytecdn.cn/*"]})`, true);
console.log("granted:", granted);
console.log("held   :", JSON.stringify(await evaluate(mgr, `new Promise(r=>chrome.permissions.getAll(p=>r(p.origins)))`)));
