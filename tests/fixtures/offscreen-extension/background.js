// Like Edge Read Aloud: the content script asks for the offscreen player,
// then talks to it over a port.
chrome.runtime.onMessage.addListener((message, _sender, reply) => {
  if (message?.cmd !== "offscreen") return;
  chrome.offscreen.hasDocument()
    .then((has) => has || chrome.offscreen.createDocument({ url: "/offscreen.html", reasons: ["AUDIO_PLAYBACK"], justification: "test" }))
    .then(() => reply(true), (e) => reply({ error: String(e) }));
  return true;
});
chrome.action.onClicked.addListener((tab) => chrome.tabs.sendMessage(tab.id, "play"));
