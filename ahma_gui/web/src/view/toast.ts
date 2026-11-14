// Small, unobtrusive messages: what the server said when it refused something.

import { el } from "../dom.ts";
import type { Mounted } from "../dom.ts";

const SHOW_MS = 6000;
const MAX_SHOWN = 3;

let sink: ((message: string) => void) | null = null;

/** Show a message, if a toast area is mounted. */
export function notify(message: string): void {
  sink?.(message);
}

export function mount(root: HTMLElement): Mounted<{ message: string }> {
  const area = el("div", { class: "toasts", role: "status", "aria-live": "polite" });
  root.appendChild(area);

  function show(message: string): void {
    const toast = el("button", { type: "button", class: "toast" }, message);
    const dismiss = (): void => toast.remove();
    toast.addEventListener("click", dismiss);
    setTimeout(dismiss, SHOW_MS);
    area.appendChild(toast);
    while (area.children.length > MAX_SHOWN) area.firstElementChild?.remove();
  }

  sink = show;
  return {
    update: (data) => show(data.message),
    destroy: () => {
      if (sink === show) sink = null;
      area.remove();
    },
  };
}
