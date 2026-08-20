export type Child = Node | string | null | undefined | false;

type Props = Record<string, unknown>;

/** Minimal hyperscript-style element builder for vanilla DOM construction. */
export function el<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  props: Props = {},
  children: Child[] = [],
): HTMLElementTagNameMap[K] {
  const node = document.createElement(tag);
  let deferredValue: unknown;
  for (const [key, value] of Object.entries(props)) {
    if (value == null || value === false) continue;
    if (key === "value") {
      // Deferred until after children are appended: a <select>'s value can
      // only "stick" once its <option>s exist.
      deferredValue = value;
    } else if (key === "class") {
      node.className = String(value);
    } else if (key === "style" && typeof value === "object") {
      Object.assign(node.style, value as Record<string, string>);
    } else if (key.startsWith("on") && typeof value === "function") {
      node.addEventListener(key.slice(2).toLowerCase(), value as EventListener);
    } else if (key in node) {
      (node as unknown as Record<string, unknown>)[key] = value;
    } else {
      node.setAttribute(key, String(value));
    }
  }
  for (const child of children) {
    if (child == null || child === false) continue;
    node.appendChild(typeof child === "string" ? document.createTextNode(child) : child);
  }
  if (deferredValue !== undefined) {
    (node as unknown as Record<string, unknown>).value = deferredValue;
  }
  return node;
}

/** A small stroke-based (Lucide/Feather-style) icon, sized to sit inside a 24px icon button. */
export function svgIcon(inner: string, viewBox = "0 0 24 24"): SVGSVGElement {
  const node = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  node.setAttribute("viewBox", viewBox);
  node.setAttribute("width", "16");
  node.setAttribute("height", "16");
  node.setAttribute("fill", "none");
  node.setAttribute("stroke", "currentColor");
  node.setAttribute("stroke-width", "2");
  node.setAttribute("stroke-linecap", "round");
  node.setAttribute("stroke-linejoin", "round");
  node.innerHTML = inner;
  return node;
}

export function cssEscape(s: string): string {
  return typeof CSS !== "undefined" && CSS.escape ? CSS.escape(s) : s.replace(/[^a-zA-Z0-9_-]/g, "\\$&");
}

/**
 * Rebuilds `root`'s contents from `build()`, restoring focus (and text
 * selection) afterwards if the previously-focused element carried a
 * `data-focus-key` attribute, and restoring scroll position for any element
 * carrying a `data-preserve-scroll` key. The app re-renders its whole tree on
 * every state change - even ones unrelated to a given panel, like dragging
 * the sidebar splitter - so without this, every scrollable panel would jump
 * back to the top on each unrelated state change.
 */
export function renderPreservingFocus(root: HTMLElement, build: () => Node) {
  const active = document.activeElement;
  let focusKey: string | null = null;
  let selStart: number | null = null;
  let selEnd: number | null = null;
  let reusableInput: HTMLInputElement | null = null;
  if (active instanceof HTMLElement && root.contains(active)) {
    focusKey = active.getAttribute("data-focus-key");
    if (active instanceof HTMLInputElement) {
      reusableInput = active;
      try {
        selStart = active.selectionStart;
        selEnd = active.selectionEnd;
      } catch {
        // Some input types (e.g. color) don't support selection ranges.
      }
    }
  }

  const scrollPositions = new Map<string, number>();
  root.querySelectorAll<HTMLElement>("[data-preserve-scroll]").forEach((el) => {
    scrollPositions.set(el.getAttribute("data-preserve-scroll")!, el.scrollTop);
  });

  const next = build();

  // The app re-renders its whole tree on every keystroke (each oninput handler
  // writes to the store, which synchronously rebuilds everything), which would
  // normally replace the focused <input> with a freshly-built one and wipe its
  // native undo/redo history. Splice the still-live, already-focused DOM node
  // back in for its freshly-built counterpart so that history survives - its
  // value already matches state (it's what the keystroke just wrote there), so
  // nothing else needs to be copied over. Only possible when the input carries
  // a focus key, since that's how its freshly-built counterpart is found.
  let spliced = false;
  if (focusKey && reusableInput && next instanceof Element) {
    const fresh = next.querySelector(`[data-focus-key="${cssEscape(focusKey)}"]`);
    if (fresh instanceof HTMLInputElement && fresh.type === reusableInput.type) {
      fresh.replaceWith(reusableInput);
      spliced = true;
    }
  }

  root.replaceChildren(next);

  if (spliced && reusableInput) {
    reusableInput.focus();
    if (selStart != null) {
      try {
        reusableInput.setSelectionRange(selStart, selEnd ?? selStart);
      } catch {
        // ignore
      }
    }
  } else if (focusKey) {
    const found = root.querySelector(`[data-focus-key="${cssEscape(focusKey)}"]`);
    if (found instanceof HTMLElement) {
      found.focus();
      if (found instanceof HTMLInputElement && selStart != null) {
        try {
          found.setSelectionRange(selStart, selEnd ?? selStart);
        } catch {
          // ignore
        }
      }
    }
  }

  scrollPositions.forEach((scrollTop, key) => {
    const found = root.querySelector(`[data-preserve-scroll="${cssEscape(key)}"]`);
    if (found instanceof HTMLElement) found.scrollTop = scrollTop;
  });
}

/** Drag helper shared by all splitters and column resize handles. */
export function startDrag(e: MouseEvent, onDelta: (dx: number) => void) {
  e.preventDefault();
  e.stopPropagation();
  const startX = e.clientX;
  const onMove = (ev: MouseEvent) => onDelta(ev.clientX - startX);
  const onUp = () => {
    window.removeEventListener("mousemove", onMove);
    window.removeEventListener("mouseup", onUp);
  };
  window.addEventListener("mousemove", onMove);
  window.addEventListener("mouseup", onUp);
}
