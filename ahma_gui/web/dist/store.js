// The page's state is the latest copy of each region the server has sent. The
// reducer is pure: a View merges in region by region, whole regions replacing
// whole regions (never a deep merge), and a region the View leaves out keeps its
// previous object, so "did this region change" is a reference comparison.
import { PROTOCOL } from "./wire.js";
export const REGIONS = ["header", "work", "chat", "log", "gates"];
export const initialState = {
    rev: 0,
    header: null,
    work: null,
    chat: null,
    log: null,
    gates: null,
    protocolError: null,
};
function isRecord(x) {
    return typeof x === "object" && x !== null && !Array.isArray(x);
}
/**
 * Merge one server message into the state. Anything that is not a View is
 * ignored; a View of another protocol changes nothing but `protocolError`.
 */
export function reduce(state, message) {
    if (!isRecord(message) || typeof message["protocol"] !== "number")
        return state;
    if (message["protocol"] !== PROTOCOL) {
        return state.protocolError === message["protocol"]
            ? state
            : { ...state, protocolError: message["protocol"] };
    }
    const view = message;
    const next = { ...state, rev: typeof view.rev === "number" ? view.rev : state.rev };
    for (const region of REGIONS) {
        const incoming = view[region];
        if (isRecord(incoming))
            next[region] = incoming;
    }
    return next;
}
/** The regions whose object differs between two states. */
export function changedRegions(a, b) {
    return REGIONS.filter((r) => a[r] !== b[r]);
}
