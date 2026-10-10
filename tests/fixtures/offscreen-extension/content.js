chrome.runtime.onMessage.addListener(async (message) => {
  if (message !== "play") return;
  const ready = await chrome.runtime.sendMessage({ cmd: "offscreen" });
  if (ready !== true) return (document.title = `AUDIO error ${ready?.error}`);
  const port = chrome.runtime.connect({ name: "player" });
  port.onMessage.addListener((m) => (document.title = `AUDIO ${m.state} ${m.ua}`));
  port.postMessage({ t: "play" });
});

// The test page posts `{ ws: <url> }`: the offscreen page opens that
// WebSocket (through the browser's network gate) and the outcome goes in the
// title as `WS <events>`.
addEventListener("message", (e) => {
  if (e.source !== window || typeof e.data?.ws !== "string") return;
  const port = chrome.runtime.connect({ name: "socket" });
  port.onMessage.addListener((m) => (document.title = `WS ${m.events}`));
  port.postMessage({ t: "ws", url: e.data.ws });
});
