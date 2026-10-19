// Saved passwords: offer to save on sign-in, fill when the user picks an
// account. Also addresses and cards (autofill): focus on such a field is
// reported, and the form is filled with the entry the user picks. In every frame: a frame's form is its own site's. Runs in its own script world, so the page can't see it or post
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

  // Address and card fields, named by their `autocomplete` token, or guessed
  // from their name, id, placeholder and label.
  const TOKENS = /^(name|given-name|family-name|organization|street-address|address-line[12]|address-level[12]|postal-code|country|country-name|email|tel|cc-name|cc-given-name|cc-family-name|cc-number|cc-exp|cc-exp-month|cc-exp-year)$/;
  const GUESSES = [
    [/card.?num|cc.?num|cardnumber/, "cc-number"], [/(card|cc).?(holder|name)|name.?on.?card/, "cc-name"],
    [/exp.*(month|mm)|cc.?month/, "cc-exp-month"], [/exp.*(year|yy)|cc.?year/, "cc-exp-year"], [/expir|exp.?date/, "cc-exp"],
    [/e.?mail/, "email"], [/phone|mobile|\btel/, "tel"], [/zip|postal|postcode/, "postal-code"],
    [/city|town|locality/, "address-level2"], [/state|province|region|county/, "address-level1"], [/country/, "country"],
    [/address.?(line)?.?2|apartment|suite/, "address-line2"], [/street|address/, "street-address"],
    [/company|organi[sz]ation/, "organization"], [/full.?name|^name$|your.?name/, "name"],
    [/first.?name|given.?name|\bfname/, "given-name"], [/last.?name|surname|family.?name|\blname/, "family-name"],
  ];
  const fieldOf = el => {
    if (!(el instanceof HTMLInputElement || el instanceof HTMLSelectElement)) return null;
    if (["password", "hidden", "checkbox", "radio", "submit", "button", "file", "search"].includes(el.type)) return null;
    const token = (el.autocomplete || "").toLowerCase().split(/\s+/).pop();
    if (TOKENS.test(token)) return token;
    if (token === "off" && !el.name && !el.id) return null;
    const label = el.labels && el.labels[0] ? el.labels[0].textContent : "";
    const text = [el.name, el.id, el.placeholder || "", label].join(" ").toLowerCase().trim();
    if (!text || /user|login|search|captcha|coupon|promo|verif|query/.test(text)) return null;
    const hit = GUESSES.find(([re]) => re.test(text));
    return hit ? hit[1] : null;
  };
  const formOf = el => el.form || el.closest("form") || document;
  // A guessed field counts only in a form with another one (not a lone search box).
  const autofillKind = el => {
    const f = fieldOf(el);
    if (!f) return null;
    const explicit = TOKENS.test((el.autocomplete || "").toLowerCase().split(/\s+/).pop());
    if (!explicit && [...formOf(el).querySelectorAll("input, select")].filter(fieldOf).length < 2) return null;
    return f.startsWith("cc-") ? "card" : "address";
  };

  // Focus on a login field: the browser shows saved accounts, if any. On an
  // address or card field: saved addresses or cards.
  let offered = new WeakSet();
  addEventListener("focusin", e => {
    const el = e.target;
    const login = isPassword(el) || (isUser(el) && scope(el).querySelector("input[type=password]"));
    const kind = login ? null : autofillKind(el);
    if ((!login && !kind) || offered.has(el)) return;
    offered.add(el);
    const r = el.getBoundingClientRect();
    post(login ? { op: "focus", x: r.left, y: r.bottom, width: r.width } : { op: "autofill", kind, x: r.left, y: r.bottom });
  }, true);

  // Called by the browser with the picked entry's values, by field name.
  // Cards only on HTTPS (or this Mac), whatever the browser was told.
  window.__mybAutofill = values => {
    const card = Object.keys(values).some(k => k.startsWith("cc-"));
    const local = ["localhost", "127.0.0.1", "[::1]"].includes(location.hostname);
    if (card && location.protocol !== "https:" && !(local && location.protocol === "http:")) return 0;
    const el = document.activeElement;
    const root = el && (el.tagName === "INPUT" || el.tagName === "SELECT") ? formOf(el) : document;
    let filled = 0;
    for (const input of root.querySelectorAll("input, select")) {
      const value = values[fieldOf(input)];
      if (!value || input.disabled || input.readOnly) continue;
      if (input instanceof HTMLSelectElement) {
        const v = value.toLowerCase();
        const o = [...input.options].find(o => o.value.toLowerCase() === v || o.text.trim().toLowerCase() === v
          || (/^\d+$/.test(v) && /^\d+$/.test(o.value) && +o.value === +v));
        if (!o) continue;
        input.value = o.value;
      } else {
        Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value").set.call(input, value);
        input.dispatchEvent(new Event("input", { bubbles: true }));
      }
      input.dispatchEvent(new Event("change", { bubbles: true }));
      filled++;
    }
    return filled;
  };

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
    // Same rule as `site_for`: HTTPS, or plain HTTP only on this Mac.
    const local = ["localhost", "127.0.0.1", "[::1]"].includes(site);
    if (location.protocol !== "https:" && !(local && location.protocol === "http:")) return;
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
