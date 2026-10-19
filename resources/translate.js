// Translate Page, in the browser's own script world (the page can't see or
// change it). Runs only when the user asks. collect() returns the page's
// text, one entry per text node; apply() puts translations in their place,
// keeping the spacing around them; restore() puts the page's own text back.
window.__mybTr = window.__mybTr || (() => {
  const SKIP = "script,style,noscript,textarea,code,pre,kbd,samp,var,svg,math,template," +
    "[translate=no],.notranslate,[contenteditable=''],[contenteditable=true]";
  let nodes = [], orig = [];
  return {
    collect() {
      this.restore();
      const root = document.body || document.documentElement;
      const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT, {
        acceptNode: n => /\p{L}/u.test(n.nodeValue) && !(n.parentElement && n.parentElement.closest(SKIP))
          ? NodeFilter.FILTER_ACCEPT : NodeFilter.FILTER_REJECT,
      });
      while (nodes.length < 5000 && walker.nextNode()) nodes.push(walker.currentNode);
      orig = nodes.map(n => n.nodeValue);
      return orig.map(t => t.trim());
    },
    apply(at, texts) {
      texts.forEach((t, i) => {
        const n = nodes[at + i], o = orig[at + i];
        if (n && t) n.nodeValue = o.match(/^\s*/)[0] + t + o.match(/\s*$/)[0];
      });
      return texts.length;
    },
    restore() {
      nodes.forEach((n, i) => { n.nodeValue = orig[i]; });
      nodes = []; orig = [];
    },
  };
})();
