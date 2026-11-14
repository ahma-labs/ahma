// What is running: one accordion section per client/session, open one at a time,
// each listing its operations. Everything the buttons do is an intent.
import { send } from "../act.js";
import { el, reconcile, setHidden, setText } from "../dom.js";
import { formatDuration } from "../format.js";
const GLYPH = {
    running: "●",
    queued: "○",
    succeeded: "✓",
    failed: "✕",
    cancelled: "–",
};
function countsText(c) {
    const parts = [];
    if (c.running > 0)
        parts.push(`${c.running} running`);
    if (c.queued > 0)
        parts.push(`${c.queued} queued`);
    if (c.failed > 0)
        parts.push(`${c.failed} failed`);
    if (c.succeeded > 0)
        parts.push(`${c.succeeded} ok`);
    return parts.join(" · ");
}
const rowKey = (r) => `${r.id}\u0000${r.instance ?? ""}`;
function createRow(first) {
    let cur = first;
    const glyph = el("span", { class: "glyph", "aria-hidden": "true" });
    const status = el("span", { class: "status" });
    const title = el("span", { class: "row-title mono" });
    const duration = el("span", { class: "meta quiet" });
    const exit = el("span", { class: "meta quiet" });
    const tag = el("span", { class: "tag tag-warn" }, "outside sandbox");
    const cancel = el("button", { type: "button", class: "btn btn-small" }, "Cancel");
    const toggle = el("button", { type: "button", class: "btn btn-small btn-quiet", "aria-expanded": "false" }, "Output");
    const main = el("div", { class: "row-main" }, glyph, status, title, tag, duration, exit);
    const actions = el("div", { class: "row-actions" }, cancel, toggle);
    const denialText = el("span", { class: "denial-text" });
    const reraise = el("button", { type: "button", class: "btn btn-small" }, "Ask again");
    const denial = el("div", { class: "denial" }, denialText, reraise);
    const output = el("pre", { class: "output mono", tabindex: 0, "aria-label": "Output tail" });
    const node = el("div", { class: "row" }, el("div", { class: "row-line" }, main, actions), denial, output);
    const ref = () => ({ id: cur.id, instance: cur.instance });
    cancel.addEventListener("click", () => void send({ type: "cancel_op", ...ref() }));
    reraise.addEventListener("click", () => void send({ type: "re_raise_grant", ...ref() }));
    toggle.addEventListener("click", () => void send({ type: "toggle_op_output", ...ref() }));
    return {
        node,
        patch(row) {
            cur = row;
            node.dataset["status"] = row.status;
            setText(glyph, GLYPH[row.status] ?? "·");
            setText(status, row.status);
            setText(title, row.title);
            setText(duration, row.duration_ms === null ? "" : formatDuration(row.duration_ms));
            setText(exit, row.exit_code === null ? "" : `exit ${row.exit_code}`);
            setHidden(tag, !row.unsandboxed);
            setHidden(cancel, !row.cancellable);
            toggle.setAttribute("aria-expanded", String(row.expanded));
            setHidden(denial, row.denial === null);
            if (row.denial !== null)
                setText(denialText, `Denied ${row.denial.access} access to ${row.denial.path}`);
            setHidden(output, !row.expanded);
            if (row.expanded)
                setText(output, row.output.length === 0 ? "(no output yet)" : row.output.join("\n"));
        },
    };
}
function createSection(first) {
    let cur = first;
    const caret = el("span", { class: "caret", "aria-hidden": "true" }, "▸");
    const pulse = el("span", { class: "pulse", "aria-hidden": "true" });
    const title = el("span", { class: "sec-title" });
    const scope = el("span", { class: "sec-scope mono quiet" });
    const counts = el("span", { class: "sec-counts quiet" });
    const latest = el("span", { class: "sec-latest quiet" });
    const head = el("button", { type: "button", class: "sec-head", "aria-expanded": "false" }, caret, pulse, title, scope, counts, latest);
    head.addEventListener("click", () => void send({ type: "open_section", key: cur.open ? null : cur.key }));
    const chatWith = el("button", { type: "button", class: "btn btn-small" }, "Chat with this window");
    chatWith.addEventListener("click", () => void send({ type: "chat_with", key: cur.key }));
    const rowsEl = el("div", { class: "rows" });
    const empty = el("p", { class: "empty" }, "No operations.");
    const body = el("div", { class: "sec-body" }, el("div", { class: "sec-tools" }, chatWith), empty, rowsEl);
    const node = el("div", { class: "section" }, head, body);
    const rows = new Map();
    return {
        node,
        patch(section) {
            cur = section;
            node.dataset["open"] = String(section.open);
            node.dataset["kind"] = section.kind;
            head.setAttribute("aria-expanded", String(section.open));
            setHidden(pulse, !section.running);
            setText(title, section.title);
            setText(scope, section.scope);
            setText(counts, countsText(section.counts));
            setText(latest, !section.open && section.latest !== null ? `${section.latest.status} · ${section.latest.title}` : "");
            setHidden(body, !section.open);
            if (section.open) {
                setHidden(empty, section.rows.length > 0);
                reconcile(rowsEl, rows, section.rows, rowKey, createRow);
            }
        },
    };
}
export function mount(root) {
    const project = el("span", { class: "project mono quiet" });
    const filter = el("button", { type: "button", class: "btn btn-small btn-quiet", "aria-pressed": "false" }, "All projects");
    filter.addEventListener("click", () => void send({ type: "toggle_project_filter" }));
    const empty = el("p", { class: "empty" }, "No activity yet.");
    const list = el("div", { class: "sections" });
    root.appendChild(el("section", { class: "panel work", "aria-label": "Work" }, el("div", { class: "panel-head" }, el("h2", { class: "panel-title" }, "Work"), project, el("span", { class: "hdr-spacer" }), filter), empty, list));
    const sections = new Map();
    return {
        update(work) {
            if (work === null)
                return;
            setText(project, work.all_projects ? "" : (work.project ?? ""));
            filter.setAttribute("aria-pressed", String(work.all_projects));
            setHidden(empty, work.sections.length > 0);
            reconcile(list, sections, work.sections, (s) => s.key, createSection);
        },
    };
}
