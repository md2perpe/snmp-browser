import { renderPreservingFocus } from "./dom";
import { renderApp, updateAutoRefreshRings } from "./render";
import { Store } from "./state";

const store = new Store();
const root = document.getElementById("app");
if (!root) throw new Error("#app root element missing");

function render() {
  renderPreservingFocus(root!, () => renderApp(store));
}

store.onChange(render);
store.onTick(() => updateAutoRefreshRings(store, root!));
render();
void store.init();

// Elements with a custom context menu already call preventDefault() themselves;
// this suppresses the native browser menu everywhere else, except on editable
// controls (inputs, textareas, contenteditable) where the native menu provides
// cut/copy/paste/spellcheck that the app doesn't otherwise offer.
window.addEventListener("contextmenu", (e) => {
  const target = e.target as HTMLElement | null;
  if (target?.closest("input, textarea, [contenteditable='true']")) return;
  e.preventDefault();
});

// Cycle the active pane's tabs with the usual tab-switching shortcuts:
// Ctrl+Tab / Ctrl+Shift+Tab (Windows/Linux convention, also common on Mac),
// and Cmd+Shift+] / Cmd+Shift+[ (Safari/Chrome's Mac convention).
window.addEventListener("keydown", (e) => {
  let direction: 1 | -1 | null = null;
  if (e.ctrlKey && e.key === "Tab") direction = e.shiftKey ? -1 : 1;
  else if (e.metaKey && e.shiftKey && e.key === "]") direction = 1;
  else if (e.metaKey && e.shiftKey && e.key === "[") direction = -1;
  if (direction === null) return;
  e.preventDefault();
  store.cycleActiveTab(direction);
});
