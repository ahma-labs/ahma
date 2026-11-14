// Small, unobtrusive messages: what the server said when it refused something.
import { el } from "../dom.js";
const SHOW_MS = 6000;
const MAX_SHOWN = 3;
let sink = null;
/** Show a message, if a toast area is mounted. */
export function notify(message) {
    sink?.(message);
}
export function mount(root) {
    const area = el("div", { class: "toasts", role: "status", "aria-live": "polite" });
    root.appendChild(area);
    function show(message) {
        const toast = el("button", { type: "button", class: "toast" }, message);
        const dismiss = () => toast.remove();
        toast.addEventListener("click", dismiss);
        setTimeout(dismiss, SHOW_MS);
        area.appendChild(toast);
        while (area.children.length > MAX_SHOWN)
            area.firstElementChild?.remove();
    }
    sink = show;
    return {
        update: (data) => show(data.message),
        destroy: () => {
            if (sink === show)
                sink = null;
            area.remove();
        },
    };
}
