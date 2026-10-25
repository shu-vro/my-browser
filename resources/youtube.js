// YouTube's in-video ads. The player gets its ad schedule in the same JSON as
// the video (ytInitialPlayerResponse on the first page, /youtubei/v1/player
// after that), from youtube.com itself, so the network blocker can't stop it.
// This drops the ad fields before the player reads them, as uBlock Origin's
// and Brave's json-prune filters do. Page world (it wraps the page's own
// JSON.parse and Response.json), at document start, in every frame (embedded
// players); on other sites, and under Shields down, it does nothing.
(() => {
  const EXEMPT = EXEMPT_SITES;
  const host = location.hostname;
  if (!/(^|\.)youtube(-nocookie)?\.com$/.test(host)) return;
  const origins = location.ancestorOrigins;
  const top = origins && origins.length ? new URL(origins[origins.length - 1]).hostname : host;
  if (EXEMPT.some(s => top === s || top.endsWith("." + s))) return;

  const AD = ["adPlacements", "playerAds", "adSlots"];
  const prune = o => {
    for (const r of [o, o && o.playerResponse]) if (r && typeof r === "object") for (const k of AD) delete r[k];
    return o;
  };
  let initial;
  Object.defineProperty(window, "ytInitialPlayerResponse", {
    configurable: true,
    get: () => initial,
    set: v => { initial = prune(v); },
  });
  const parse = JSON.parse;
  JSON.parse = function () { return prune(parse.apply(this, arguments)); };
  const json = Response.prototype.json;
  Response.prototype.json = function () { return json.call(this).then(prune); };
})();
