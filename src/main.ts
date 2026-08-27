import { renderPreservingFocus } from "./dom";
import { renderApp } from "./render";
import { Store } from "./state";

const store = new Store();
const root = document.getElementById("app");
if (!root) throw new Error("#app root element missing");

function render() {
  renderPreservingFocus(root!, () => renderApp(store));
}

store.onChange(render);
render();
void store.init();

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
