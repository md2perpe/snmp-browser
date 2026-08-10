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
