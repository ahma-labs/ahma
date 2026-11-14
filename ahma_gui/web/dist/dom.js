// The only place nodes are made. Text goes in as text nodes / textContent and
// never as markup, and only an allow-list of attributes can be set, so nothing
// the server (or the model behind it) sends can become an element, an attribute
// or an event handler.
const PLAIN = new Set(["class", "type", "id", "for", "role", "tabindex", "placeholder", "rows"]);
const PREFIXED = /^(?:aria|data)-[a-z0-9-]+$/;
export function el(tag, attrs = {}, ...children) {
    const node = document.createElement(tag);
    for (const [name, value] of Object.entries(attrs)) {
        const prefixed = PREFIXED.test(name);
        if (!prefixed && !PLAIN.has(name) && name !== "disabled" && name !== "value") {
            throw new Error(`attribute not allowed: ${name}`);
        }
        if (value === undefined)
            continue;
        if (name === "class")
            node.className = String(value);
        else if (name === "disabled")
            node.disabled = Boolean(value);
        else if (name === "value")
            node.value = String(value);
        else if (prefixed)
            node.setAttribute(name, String(value)); // aria-expanded="false" matters
        else if (value !== false)
            node.setAttribute(name, value === true ? "" : String(value));
    }
    append(node, children);
    return node;
}
export function append(parent, children) {
    for (const child of children) {
        if (child === null || child === undefined || child === false)
            continue;
        parent.appendChild(typeof child === "string" ? document.createTextNode(child) : child);
    }
}
/** Set an element's text, touching the DOM only when it actually changed. */
export function setText(node, text) {
    if (node.textContent !== text)
        node.textContent = text;
}
export function setHidden(node, hidden) {
    if (node.hidden !== hidden)
        node.hidden = hidden;
}
/**
 * Make `parent`'s children match `items`, keyed: a node whose key is still
 * present is patched in place (and moved only if its position changed), a new key
 * is created, a vanished key is removed. `parent` must hold only these nodes.
 */
export function reconcile(parent, nodes, items, keyOf, create) {
    const seen = new Set();
    items.forEach((item, i) => {
        let key = keyOf(item);
        while (seen.has(key))
            key += "~"; // a duplicated key must not collapse two items
        seen.add(key);
        let entry = nodes.get(key);
        if (entry === undefined) {
            entry = create(item);
            nodes.set(key, entry);
        }
        entry.patch(item);
        const at = parent.children[i];
        if (at !== entry.node)
            parent.insertBefore(entry.node, at ?? null);
    });
    for (const [key, entry] of nodes) {
        if (!seen.has(key)) {
            entry.node.remove();
            nodes.delete(key);
        }
    }
}
