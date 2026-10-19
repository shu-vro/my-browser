chrome.commands.onCommand.addListener(async (name, tab) => {
  const [t] = tab ? [tab] : await chrome.tabs.query({ active: true, currentWindow: true });
  chrome.scripting.executeScript({ target: { tabId: t.id }, func: n => { document.title = "CMD " + n; }, args: [name] });
});
