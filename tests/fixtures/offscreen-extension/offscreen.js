const ctx = new AudioContext();
chrome.runtime.onConnect.addListener((port) => {
  port.onMessage.addListener(async (m) => {
    if (m.t === "ws") return socket(port, m.url);
    const osc = ctx.createOscillator();
    const gain = ctx.createGain();
    gain.gain.value = 0.0001; // inaudible
    osc.connect(gain).connect(ctx.destination);
    osc.start();
    osc.stop(ctx.currentTime + 0.2);
    await Promise.race([ctx.resume(), new Promise((r) => setTimeout(r, 1000))]);
    port.postMessage({ state: ctx.state, ua: navigator.userAgent });
  });
});

function socket(port, url) {
  const events = [];
  const ws = new WebSocket(url);
  ws.onopen = () => events.push("open");
  ws.onerror = () => events.push("error");
  ws.onclose = (e) => {
    events.push(`close ${e.code} ${e.reason}`);
    port.postMessage({ events: events.join(" ") });
  };
}
