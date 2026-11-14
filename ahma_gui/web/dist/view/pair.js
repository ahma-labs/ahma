// The pairing screen: the human types the short single-use code their terminal
// shows. The server decides whether it is right; this shows what it said.
import { ApiError } from "../api.js";
import { el, setText } from "../dom.js";
export function mount(root, deps) {
    const input = el("input", {
        type: "text",
        id: "pair-code",
        class: "input mono pair-code",
        placeholder: "ABCD-EFGH",
        "aria-describedby": "pair-message",
    });
    input.autocomplete = "off";
    input.spellcheck = false;
    input.autocapitalize = "characters";
    input.maxLength = 32;
    const button = el("button", { type: "submit", class: "btn btn-primary" }, "Pair");
    const message = el("p", { id: "pair-message", class: "pair-message", role: "alert" });
    const form = el("form", { class: "pair-form" }, el("label", { for: "pair-code", class: "pair-label" }, "Pairing code"), el("div", { class: "pair-row" }, input, button), message);
    let busy = false;
    form.addEventListener("submit", (event) => {
        event.preventDefault(); // never a navigation; the code goes to /api/pair by fetch
        const code = input.value.trim();
        if (busy || code === "")
            return;
        busy = true;
        button.disabled = true;
        setText(message, "");
        deps
            .submit(code)
            .catch((err) => {
            setText(message, err instanceof ApiError ? err.message : "Could not pair.");
            input.select();
        })
            .finally(() => {
            busy = false;
            button.disabled = false;
        });
    });
    const card = el("main", { class: "pair" }, el("div", { class: "pair-card" }, el("h1", { class: "pair-title" }, "ahma"), el("p", { class: "pair-help" }, "Enter the code shown in your terminal to watch and answer from this browser."), form));
    root.appendChild(card);
    input.focus();
    return {
        update: ({ notice }) => {
            if (notice !== null)
                setText(message, notice);
        },
        destroy: () => {
            input.value = "";
            card.remove();
        },
    };
}
