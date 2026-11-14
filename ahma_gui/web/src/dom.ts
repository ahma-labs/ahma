// The only place nodes are made. Text goes in as text nodes / textContent and
// never as markup, and only an allow-list of attributes can be set, so nothing
// the server (or the model behind it) sends can become an element, an attribute
// or an event handler.

export type AttrValue = string | number | boolean | undefined;
export type Attrs = { readonly [name: string]: AttrValue };
export type Child = Node | string | null | undefined | false;

/** What every region view is: it mounts once and is told its data on each change. */
export interface Mounted<T> {
  update(data: T): void;
  destroy?: () => void;
}

const PLAIN = new Set(["class", "type", "id", "for", "role", "tabindex", "placeholder", "rows"]);
const PREFIXED = /^(?:aria|data)-[a-z0-9-]+$/;

export function el<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  attrs: Attrs = {},
  ...children: Child[]
): HTMLElementTagNameMap[K] {
  const node = document.createElement(tag);
  for (const [name, value] of Object.entries(attrs)) {
    const prefixed = PREFIXED.test(name);
    if (!prefixed && !PLAIN.has(name) && name !== "disabled" && name !== "value") {
      throw new Error(`attribute not allowed: ${name}`);
    }
    if (value === undefined) continue;
    if (name === "class") node.className = String(value);
    else if (name === "disabled") (node as HTMLButtonElement).disabled = Boolean(value);
    else if (name === "value") (node as HTMLInputElement).value = String(value);
    else if (prefixed) node.setAttribute(name, String(value)); // aria-expanded="false" matters
    else if (value !== false) node.setAttribute(name, value === true ? "" : String(value));
  }
  append(node, children);
  return node;
}

export function append(parent: Node, children: readonly Child[]): void {
  for (const child of children) {
    if (child === null || child === undefined || child === false) continue;
    parent.appendChild(typeof child === "string" ? document.createTextNode(child) : child);
  }
}

/** Set an element's text, touching the DOM only when it actually changed. */
export function setText(node: Node, text: string): void {
  if (node.textContent !== text) node.textContent = text;
}

export function setHidden(node: HTMLElement, hidden: boolean): void {
  if (node.hidden !== hidden) node.hidden = hidden;
}

export interface Keyed<T> {
  node: HTMLElement;
  patch(item: T): void;
}

/**
 * Make `parent`'s children match `items`, keyed: a node whose key is still
 * present is patched in place (and moved only if its position changed), a new key
 * is created, a vanished key is removed. `parent` must hold only these nodes.
 */
export function reconcile<T>(
  parent: HTMLElement,
  nodes: Map<string, Keyed<T>>,
  items: readonly T[],
  keyOf: (item: T) => string,
  create: (item: T) => Keyed<T>,
): void {
  const seen = new Set<string>();
  items.forEach((item, i) => {
    let key = keyOf(item);
    while (seen.has(key)) key += "~"; // a duplicated key must not collapse two items
    seen.add(key);
    let entry = nodes.get(key);
    if (entry === undefined) {
      entry = create(item);
      nodes.set(key, entry);
    }
    entry.patch(item);
    const at = parent.children[i];
    if (at !== entry.node) parent.insertBefore(entry.node, at ?? null);
  });
  for (const [key, entry] of nodes) {
    if (!seen.has(key)) {
      entry.node.remove();
      nodes.delete(key);
    }
  }
}
