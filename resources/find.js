// Find in page, in the browser's own script world: every match of the term
// highlighted, the one WebKit's find selected (the current one) in another
// colour, and the count for the find bar. Runs only while the bar is open.
window.__mybFind = window.__mybFind || (() => {
  let style = null;
  const SKIP = "script,style,noscript,template,textarea";
  const BLOCKS = "p,div,li,ul,ol,td,th,tr,table,h1,h2,h3,h4,h5,h6,section,article,main,header,footer,nav,aside," +
    "pre,blockquote,dd,dt,dl,figure,figcaption,form,fieldset,button,label,option,body";
  return {
    // `{count, current}` (current is 1-based, 0 if the selection isn't one of them).
    mark(term) {
      if (!window.CSS || !CSS.highlights) return null;
      // The page's visible text as one string (a match may span tags, as in
      // <b>al</b>pha), with a line break where a block ends, as WebKit's find has.
      const t = term.toLowerCase(), nodes = [], starts = [];
      let full = "", lastBlock = null;
      const root = document.body || document.documentElement;
      const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
      while (t && walker.nextNode()) {
        const node = walker.currentNode, p = node.parentElement;
        if (!p || p.closest(SKIP) || (p.checkVisibility && !p.checkVisibility())) continue;
        const block = p.closest(BLOCKS);
        if (lastBlock && block !== lastBlock) full += "\n";
        lastBlock = block;
        nodes.push(node);
        starts.push(full.length);
        full += node.nodeValue;
      }
      // The text node and offset at position `at` of `full`.
      const point = at => {
        let lo = 0, hi = starts.length - 1;
        while (lo < hi) { const mid = (lo + hi + 1) >> 1; if (starts[mid] <= at) lo = mid; else hi = mid - 1; }
        return [nodes[lo], Math.min(at - starts[lo], nodes[lo].nodeValue.length)];
      };
      const ranges = [], lower = full.toLowerCase();
      for (let i = t ? lower.indexOf(t) : -1; i >= 0 && ranges.length < 1000; i = lower.indexOf(t, i + t.length)) {
        const r = new Range();
        r.setStart(...point(i));
        r.setEnd(...point(i + t.length - 1).map((v, k) => k ? v + 1 : v));
        ranges.push(r);
      }
      const sel = getSelection();
      const s = sel.rangeCount ? sel.getRangeAt(0) : null;
      const current = s ? ranges.findIndex(r => r.compareBoundaryPoints(Range.START_TO_START, s) === 0
        && r.compareBoundaryPoints(Range.END_TO_END, s) === 0) : -1;
      if (!style || !style.isConnected) {
        style = document.createElement("style");
        style.textContent = "::highlight(myb-find){background-color:rgba(255,213,0,.45)}" +
          "::highlight(myb-find-current){background-color:#ff9632;color:#000}";
        document.documentElement.append(style);
      }
      CSS.highlights.set("myb-find", new Highlight(...ranges.filter((_, i) => i !== current)));
      CSS.highlights.set("myb-find-current", new Highlight(...(current >= 0 ? [ranges[current]] : [])));
      return JSON.stringify({ count: ranges.length, current: current + 1 });
    },
    clear() {
      if (window.CSS && CSS.highlights) { CSS.highlights.delete("myb-find"); CSS.highlights.delete("myb-find-current"); }
      if (style) style.remove();
      style = null;
    },
  };
})();
