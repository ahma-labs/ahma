// A Server-Sent Events parser, pure and incremental: feed it text as it arrives
// in whatever chunks the network chose, get back the events completed so far.
// Handles \n, \r\n and lone \r line ends (even when \r and \n land in different
// chunks), comment lines (":..."), multi-line data, and a leading BOM.
/** A line longer than this is a broken stream, not a message: fail and reconnect. */
const MAX_PENDING = 16 * 1024 * 1024;
/** A blank line: dispatch the event gathered so far, if it has any data. */
function dispatch(s, out) {
    if (s.dataLines.length > 0) {
        out.push({ event: s.eventName === "" ? "message" : s.eventName, data: s.dataLines.join("\n") });
    }
    s.eventName = "";
    s.dataLines = [];
}
/** `field: value`, `field:value` or a bare `field` (an empty value); one leading space is dropped. */
function splitField(text) {
    const colon = text.indexOf(":");
    if (colon === -1)
        return { field: text, value: "" };
    const value = text.slice(colon + 1);
    return { field: text.slice(0, colon), value: value.startsWith(" ") ? value.slice(1) : value };
}
function line(s, text, out) {
    if (text === "")
        return dispatch(s, out);
    if (text.startsWith(":"))
        return;
    const { field, value } = splitField(text);
    if (field === "event")
        s.eventName = value;
    else if (field === "data")
        s.dataLines.push(value);
    // `id` and `retry` are not used by this page.
}
/** Drop what is not content: a leading BOM, and the \n that completes a \r from the last chunk. */
function trimChunk(s, chunk) {
    if (chunk.length === 0)
        return chunk;
    let text = chunk;
    if (!s.started) {
        s.started = true;
        if (text.charCodeAt(0) === 0xfeff)
            text = text.slice(1);
    }
    if (s.skipLf) {
        s.skipLf = false;
        if (text.startsWith("\n"))
            text = text.slice(1);
    }
    return text;
}
/** Where the next line starts after the terminator at `at`; steers `ends` past a \r\n pair. */
function nextLineStart(s, at, ends) {
    if (s.pending[at] !== "\r")
        return at + 1;
    if (at + 1 >= s.pending.length) {
        s.skipLf = true; // the \n, if any, arrives in the next chunk
        return at + 1;
    }
    if (s.pending[at + 1] !== "\n")
        return at + 1;
    ends.lastIndex = at + 2;
    return at + 2;
}
/** Hand every complete line in `pending` to `line`, keeping only the unfinished tail. */
function drainLines(s, out) {
    const ends = /[\r\n]/g;
    let start = 0;
    for (let m = ends.exec(s.pending); m !== null; m = ends.exec(s.pending)) {
        line(s, s.pending.slice(start, m.index), out);
        start = nextLineStart(s, m.index, ends);
    }
    s.pending = s.pending.slice(start);
}
export function createSseParser() {
    const s = { pending: "", started: false, skipLf: false, eventName: "", dataLines: [] };
    return {
        feed(chunk) {
            s.pending += trimChunk(s, chunk);
            const out = [];
            drainLines(s, out);
            if (s.pending.length > MAX_PENDING)
                throw new Error("event stream line too long");
            return out;
        },
    };
}
