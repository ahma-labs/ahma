// The server's log tail: collapsible, level-coloured, filtered by text here
// (a display filter only; the server's lines are never changed).
import { el, setHidden, setText } from "../dom.js";
const BOTTOM_SLACK_PX = 24;
function createLine() {
    const ts = el("span", { class: "log-ts quiet mono" });
    const level = el("span", { class: "log-level mono" });
    const text = el("span", { class: "log-text mono" });
    const node = el("div", { class: "log-line" }, ts, level, text);
    return {
        node,
        set(line) {
            node.dataset["level"] = line.level;
            setText(ts, line.ts);
            setText(level, line.level);
            setText(text, line.text);
        },
    };
}
export function mount(root) {
    let open = false;
    let log = null;
    let stick = true;
    let needsDraw = true;
    const summary = el("span", { class: "log-summary quiet" });
    const toggle = el("button", { type: "button", class: "log-toggle", "aria-expanded": "false" }, el("span", { class: "caret", "aria-hidden": "true" }, "▸"), el("span", { class: "panel-title" }, "Log"), summary);
    const filter = el("input", { type: "text", class: "input log-filter", placeholder: "Filter", "aria-label": "Filter log lines" });
    const active = el("span", { class: "quiet mono log-active" });
    const lines = el("div", { class: "log-lines", role: "log", "aria-live": "off", tabindex: 0, "aria-label": "Log lines" });
    const body = el("div", { class: "log-body" }, el("div", { class: "log-tools" }, filter, active), lines);
    body.hidden = true;
    root.appendChild(el("section", { class: "panel log", "aria-label": "Log" }, toggle, body));
    const pool = [];
    lines.addEventListener("scroll", () => {
        stick = lines.scrollHeight - lines.scrollTop - lines.clientHeight < BOTTOM_SLACK_PX;
    });
    function draw() {
        needsDraw = false;
        if (log === null)
            return;
        const needle = filter.value.trim().toLowerCase();
        const shown = needle === "" ? log.lines : log.lines.filter((l) => `${l.ts} ${l.level} ${l.text}`.toLowerCase().includes(needle));
        while (pool.length < shown.length) {
            const line = createLine();
            pool.push(line);
            lines.appendChild(line.node);
        }
        while (pool.length > shown.length)
            pool.pop()?.node.remove();
        shown.forEach((l, i) => pool[i]?.set(l));
        setText(active, log.active ?? "");
        if (stick)
            lines.scrollTop = lines.scrollHeight;
    }
    toggle.addEventListener("click", () => {
        open = !open;
        toggle.setAttribute("aria-expanded", String(open));
        body.hidden = !open;
        if (open && needsDraw)
            draw();
        if (open)
            lines.scrollTop = lines.scrollHeight;
    });
    filter.addEventListener("input", () => {
        if (open)
            draw();
        else
            needsDraw = true;
    });
    return {
        update(next) {
            if (next === null)
                return;
            log = next;
            const errors = next.lines.filter((l) => l.level === "error").length;
            const warns = next.lines.filter((l) => l.level === "warn").length;
            const parts = [`${next.lines.length} lines`];
            if (errors > 0)
                parts.push(`${errors} error${errors === 1 ? "" : "s"}`);
            if (warns > 0)
                parts.push(`${warns} warning${warns === 1 ? "" : "s"}`);
            setText(summary, parts.join(" · "));
            summary.dataset["tone"] = errors > 0 ? "error" : warns > 0 ? "warn" : "none";
            if (open)
                draw();
            else
                needsDraw = true;
            setHidden(active, next.active === null);
        },
    };
}
