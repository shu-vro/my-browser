// Added by my-browser. Some services refuse WebKit's own extension
// connections (its Origin, its User-Agent). In this extension's pages,
// WebSocket goes through the browser's network gate instead: blocked until
// you allow the address in Settings → Network.
(() => {
  const api = globalThis.browser ?? globalThis.chrome;
  if (!api?.runtime?.connectNative) return;
  const toBase64 = (bytes) => {
    let s = "";
    for (let i = 0; i < bytes.length; i += 0x8000) s += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
    return btoa(s);
  };
  const fromBase64 = (b64) => Uint8Array.from(atob(b64), (c) => c.charCodeAt(0)).buffer;

  class WebSocket extends EventTarget {
    static CONNECTING = 0;
    static OPEN = 1;
    static CLOSING = 2;
    static CLOSED = 3;
    CONNECTING = 0;
    OPEN = 1;
    CLOSING = 2;
    CLOSED = 3;
    onopen = null;
    onmessage = null;
    onerror = null;
    onclose = null;
    binaryType = "blob";
    bufferedAmount = 0;
    extensions = "";
    protocol = "";
    readyState = 0;
    #port;

    constructor(url, protocols = []) {
      super();
      this.url = new URL(url, location.href).href;
      this.#port = api.runtime.connectNative("my-browser.offscreen");
      this.#port.onMessage.addListener((raw) => this.#receive(typeof raw === "string" ? JSON.parse(raw) : raw));
      this.#port.onDisconnect.addListener(() => this.#closed(1006, ""));
      this.#port.postMessage({ t: "open", url: this.url, protocols: [].concat(protocols).join(", ") });
    }

    #fire(type, init) {
      const event =
        type === "message" ? new MessageEvent("message", init) : type === "close" ? new CloseEvent("close", init) : new Event(type);
      this.dispatchEvent(event);
      this[`on${type}`]?.call(this, event);
    }

    #receive(m) {
      if (m.t === "open") {
        this.readyState = 1;
        this.protocol = m.protocol ?? "";
        this.#fire("open");
      } else if (m.t === "message" && this.readyState === 1) {
        let data = m.text;
        if (m.b64 !== undefined) {
          const buffer = fromBase64(m.b64);
          data = this.binaryType === "arraybuffer" ? buffer : new Blob([buffer]);
        }
        this.#fire("message", { data });
      } else if (m.t === "error") {
        this.#fire("error");
      } else if (m.t === "close") {
        this.#closed(m.code, m.reason);
      }
    }

    #closed(code, reason) {
      if (this.readyState === 3) return;
      this.readyState = 3;
      try {
        this.#port.disconnect();
      } catch {}
      this.#fire("close", { code, reason, wasClean: code !== 1006 });
    }

    send(data) {
      if (this.readyState === 0) throw new DOMException("Still connecting", "InvalidStateError");
      if (this.readyState !== 1) return;
      if (typeof data === "string") return this.#port.postMessage({ t: "send", text: data });
      if (data instanceof Blob) {
        // Blobs are read asynchronously; later sends may overtake this one.
        data.arrayBuffer().then((b) => this.#port.postMessage({ t: "send", b64: toBase64(new Uint8Array(b)) }));
        return;
      }
      const bytes = ArrayBuffer.isView(data) ? new Uint8Array(data.buffer, data.byteOffset, data.byteLength) : new Uint8Array(data);
      this.#port.postMessage({ t: "send", b64: toBase64(bytes) });
    }

    close(code = 1000, reason = "") {
      if (this.readyState >= 2) return;
      // Not open yet: just give up (the browser drops the attempt with the port).
      if (this.readyState === 0) return this.#closed(1006, "");
      this.readyState = 2;
      this.#port.postMessage({ t: "close", code, reason });
    }
  }
  globalThis.WebSocket = WebSocket;
})();
