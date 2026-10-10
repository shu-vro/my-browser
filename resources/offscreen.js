// Added by my-browser. WebKit has no `chrome.offscreen`, which read-aloud
// extensions use to play audio. This stands in for it in the extension's
// background page: the offscreen document is an iframe there, and the browser
// puts the page in a hidden window so it can play sound.
//
// WebKit also ignores declarativeNetRequest rules that set the User-Agent on
// an extension's own requests (read-aloud services want an Edge one), so the
// first such rule in the extension's own rule files is passed to the browser,
// which sets it on this background page.
(() => {
  const api = globalThis.browser ?? globalThis.chrome;
  if (!api || api.offscreen || typeof document === "undefined") return;
  let frame = null;
  const userAgent = async () => {
    const files = api.runtime.getManifest().declarative_net_request?.rule_resources ?? [];
    for (const file of files.filter((f) => f.enabled)) {
      const rules = await fetch(api.runtime.getURL(file.path)).then((r) => r.json()).catch(() => []);
      for (const rule of rules) {
        if (rule.action?.type !== "modifyHeaders") continue;
        const set = rule.action.requestHeaders?.find((h) => h.header?.toLowerCase() === "user-agent" && h.operation === "set");
        if (set) return set.value;
      }
    }
  };
  const offscreen = {
    Reason: new Proxy({}, { get: (_, name) => name }),
    async createDocument({ url }) {
      if (frame) throw new Error("Only a single offscreen document may be created.");
      await api.runtime.sendNativeMessage("my-browser.offscreen", { userAgent: await userAgent() }).catch(() => {});
      frame = document.createElement("iframe");
      frame.src = api.runtime.getURL(url);
      await new Promise((resolve) => {
        frame.onload = resolve;
        document.body.append(frame);
      });
    },
    async hasDocument() {
      return frame !== null;
    },
    async closeDocument() {
      frame?.remove();
      frame = null;
    },
  };
  for (const ns of [globalThis.browser, globalThis.chrome]) if (ns) ns.offscreen = offscreen;
})();
