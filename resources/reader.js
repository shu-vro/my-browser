// Reader mode: find the article in the page and return it as clean HTML.
// Runs only when the user asks (View → Reader), in the browser's own script
// world, so the page can't see or change it. Returns "" when there's no
// article worth showing, else JSON {title, site, html}.
(() => {
  const textOf = el => el.textContent.replace(/\s+/g, " ").trim();
  // The element whose own paragraphs hold the most text wins; <article> and
  // <main> get a head start.
  let best = null, bestScore = 0;
  for (const el of document.querySelectorAll("article, main, [role=main], div, section, td")) {
    let score = 0;
    for (const child of el.children) {
      if (child.tagName === "P" || child.tagName === "PRE" || child.tagName === "BLOCKQUOTE") {
        const t = textOf(child);
        if (t.length > 25) score += t.length + 50 * (t.split(",").length - 1);
      }
    }
    if (el.tagName === "ARTICLE" || el.tagName === "MAIN") score *= 1.5;
    if (score > bestScore) { bestScore = score; best = el; }
  }
  if (!best || bestScore < 300) return "";

  const keep = new Set(["P", "H1", "H2", "H3", "H4", "H5", "H6", "A", "IMG", "FIGURE", "FIGCAPTION", "BLOCKQUOTE",
    "PRE", "CODE", "UL", "OL", "LI", "EM", "STRONG", "B", "I", "U", "S", "BR", "HR", "TABLE", "THEAD", "TBODY", "TR",
    "TD", "TH", "CAPTION", "SPAN", "DIV", "SECTION", "ARTICLE", "PICTURE", "SOURCE", "SUP", "SUB", "DL", "DT", "DD",
    "MARK", "SMALL", "TIME", "ABBR", "Q", "CITE", "KBD", "VAR", "SAMP", "DEL", "INS"]);
  const junk = /(^|[\s_-])(ad|ads|advert|banner|comment|comments|footer|header|menu|nav|newsletter|promo|related|share|sharing|social|sidebar|sponsor|subscribe)([\s_-]|$)/i;
  const article = best.cloneNode(true);
  const clean = el => {
    for (const c of [...el.children]) {
      const label = (c.className && c.className.baseVal === undefined ? c.className : "") + " " + c.id;
      if (!keep.has(c.tagName) || c.hidden || c.getAttribute("aria-hidden") === "true" || junk.test(label)) {
        c.remove();
        continue;
      }
      // Lazy-loaded images keep their real address in data-src.
      if (c.tagName === "IMG" && (!c.getAttribute("src") || c.getAttribute("src").startsWith("data:"))) {
        const lazy = c.getAttribute("data-src") || c.getAttribute("data-lazy-src") || c.getAttribute("data-original");
        if (lazy) c.setAttribute("src", lazy);
      }
      for (const a of [...c.attributes]) {
        const media = c.tagName === "IMG" || c.tagName === "SOURCE";
        const ok = (a.name === "href" && c.tagName === "A") || (media && ["src", "srcset", "alt"].includes(a.name));
        if (!ok) c.removeAttribute(a.name);
      }
      // Absolute addresses, and no javascript: links.
      if (c.tagName === "A") {
        if (/^\s*javascript:/i.test(c.getAttribute("href") || "")) c.removeAttribute("href");
        else if (c.hasAttribute("href")) c.setAttribute("href", c.href);
      }
      if (c.tagName === "IMG" && c.hasAttribute("src")) c.setAttribute("src", c.src);
      clean(c);
    }
  };
  clean(article);
  const heading = best.querySelector("h1") || document.querySelector("h1");
  const title = (heading && textOf(heading)) || document.title;
  // The heading is shown once, above the text.
  const first = article.querySelector("h1");
  if (first && textOf(first) === title) first.remove();
  return JSON.stringify({ title, site: location.hostname, html: article.innerHTML });
})()
