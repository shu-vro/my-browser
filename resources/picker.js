// Hide Element…: the user points at an element and clicks it; this returns a
// CSS selector for it (null for Esc). Runs in the browser's own script world,
// only when asked, and removes itself when done.
const part = el => {
  if (el.id && /^[A-Za-z][\w-]*$/.test(el.id) && !/\d{3}/.test(el.id)) return "#" + CSS.escape(el.id);
  // Generated names (with long numbers) change between visits; skip them.
  const classes = [...el.classList].filter(c => /^-?[A-Za-z_][\w-]*$/.test(c) && !/\d{3}/.test(c)).slice(0, 3);
  let s = el.localName + classes.map(c => "." + CSS.escape(c)).join("");
  const same = el.parentElement ? [...el.parentElement.children].filter(c => c.localName === el.localName) : [];
  if (!classes.length && same.length > 1) s += ":nth-of-type(" + (same.indexOf(el) + 1) + ")";
  return s;
};
// The shortest chain of parts, from the element up, that matches it alone.
const selector = el => {
  const path = [];
  for (let e = el; e && e !== document.body && e !== document.documentElement; e = e.parentElement) {
    path.unshift(part(e));
    const s = path.join(" > ");
    if (path[0][0] === "#" || document.querySelectorAll(s).length === 1) return s;
  }
  return path.length ? path.join(" > ") : null;
};
return await new Promise(resolve => {
  const box = document.createElement("div");
  box.style.cssText = "position:fixed;z-index:2147483647;pointer-events:none;background:rgba(10,102,216,.22);" +
    "outline:2px solid #0a66d8;border-radius:2px;left:0;top:0;width:0;height:0";
  const move = e => {
    const r = e.target.getBoundingClientRect();
    Object.assign(box.style, { left: r.left + "px", top: r.top + "px", width: r.width + "px", height: r.height + "px" });
  };
  const stop = e => { e.preventDefault(); e.stopImmediatePropagation(); };
  const finish = value => {
    removeEventListener("mousemove", move, true);
    for (const t of ["mousedown", "mouseup"]) removeEventListener(t, stop, true);
    removeEventListener("click", click, true);
    removeEventListener("keydown", key, true);
    box.remove();
    resolve(value);
  };
  const click = e => { stop(e); finish(selector(e.target)); };
  const key = e => { if (e.key === "Escape") { stop(e); finish(null); } };
  addEventListener("mousemove", move, true);
  for (const t of ["mousedown", "mouseup"]) addEventListener(t, stop, true);
  addEventListener("click", click, true);
  addEventListener("keydown", key, true);
  document.documentElement.append(box);
});
