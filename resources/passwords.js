// Saved passwords: offer to save on sign-in, fill when the user picks an
// account. In every frame: a frame's form is its own site's. Runs in its own script world, so the page can't see it or post
// to its handler. Two event listeners and nothing else: no polling, no DOM
// observers. The browser decides everything; this only reports and fills.
(() => {
  const post = m => webkit.messageHandlers.passwords.postMessage(m);
  const isPassword = el => el instanceof HTMLInputElement && el.type === "password";
  const isUser = el => el instanceof HTMLInputElement && ["text", "email", "tel", ""].includes(el.type)
    && /user|login|email|mail|name|account|id/i.test(el.name + el.id + el.autocomplete + el.placeholder);
  // The login form around a field: its <form>, else the nearest ancestor
  // holding a password field.
  const scope = el => {
    if (el.form) return el.form;
    for (let p = el.parentElement; p; p = p.parentElement) if (p.querySelector("input[type=password]")) return p;
    return document;
  };
  const fields = root => {
    const password = [...root.querySelectorAll("input[type=password]")].find(p => p.offsetParent !== null)
      || root.querySelector("input[type=password]");
    const inputs = [...root.querySelectorAll("input")];
    const before = password ? inputs.slice(0, inputs.indexOf(password)) : inputs;
    const user = before.reverse().find(i => isUser(i) || (i.type === "email")) || before.find(i => i.type === "text");
    return { user, password };
  };

  // Focus on a login field: the browser shows saved accounts, if any.
  let offered = new WeakSet();
  addEventListener("focusin", e => {
    const el = e.target;
    if (!(isPassword(el) || (isUser(el) && scope(el).querySelector("input[type=password]")))) return;
    if (offered.has(el)) return;
    offered.add(el);
    const r = el.getBoundingClientRect();
    post({ op: "focus", x: r.left, y: r.bottom, width: r.width });
  }, true);

  // Sign-in: a submitted form, or Enter or a button click next to a filled
  // password field (pages that sign in with fetch).
  let last = "";
  const report = el => {
    const { user, password } = fields(scope(el));
    if (!password || !password.value) return;
    const key = (user ? user.value : "") + "\n" + password.value;
    if (key === last) return;
    last = key;
    post({ op: "submit", username: user ? user.value : "", password: password.value });
  };
  addEventListener("submit", e => report(e.target), true);
  addEventListener("keydown", e => { if (e.key === "Enter" && e.target instanceof HTMLInputElement) report(e.target); }, true);
  addEventListener("click", e => {
    const b = e.target.closest && e.target.closest("button, input[type=submit], [role=button]");
    if (b) report(b);
  }, true);

  // Called by the browser after the user picked an account for `site`;
  // nothing happens if this frame is now somewhere else.
  window.__mybFill = (username, password, site) => {
    const host = location.hostname.toLowerCase();
    if (site && host !== site && "[" + host + "]" !== site) return;
    const el = document.activeElement;
    const { user, password: field } = fields(el && el.tagName === "INPUT" ? scope(el) : document);
    const set = (input, value) => {
      if (!input) return;
      // The native setter, so frameworks that track the value notice.
      Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value").set.call(input, value);
      input.dispatchEvent(new Event("input", { bubbles: true }));
      input.dispatchEvent(new Event("change", { bubbles: true }));
    };
    set(user, username);
    set(field, password);
    last = username + "\n" + password;
  };
  // ⌥⌘A: ask for the focused field's accounts again.
  window.__mybOffer = () => {
    const el = document.activeElement;
    if (!el || el.tagName !== "INPUT") return;
    offered.delete(el);
    el.blur();
    el.focus();
  };
})();
