// A Server-Sent Events parser, pure and incremental: feed it text as it arrives
// in whatever chunks the network chose, get back the events completed so far.
// Handles \n, \r\n and lone \r line ends (even when \r and \n land in different
// chunks), comment lines (":..."), multi-line data, and a leading BOM.
/** A line longer than this is a broken stream, not a message: fail and reconnect. */
const MAX_PENDING = 16 * 1024 * 1024;
export function createSseParser() {
    let pending = "";
    let started = false;
    let skipLf = false; // the last chunk ended in \r; a leading \n now belongs to it
    let eventName = "";
    let dataLines = [];
    function line(text, out) {
        if (text === "") {
            if (dataLines.length > 0) {
                out.push({ event: eventName === "" ? "message" : eventName, data: dataLines.join("\n") });
            }
            eventName = "";
            dataLines = [];
            return;
        }
        if (text.startsWith(":"))
            return;
        const colon = text.indexOf(":");
        const field = colon === -1 ? text : text.slice(0, colon);
        let value = colon === -1 ? "" : text.slice(colon + 1);
        if (value.startsWith(" "))
            value = value.slice(1);
        if (field === "event")
            eventName = value;
        else if (field === "data")
            dataLines.push(value);
        // `id` and `retry` are not used by this page.
    }
    return {
        feed(chunk) {
            let text = chunk;
            if (!started && text.length > 0) {
                started = true;
                if (text.charCodeAt(0) === 0xfeff)
                    text = text.slice(1);
            }
            if (skipLf && text.length > 0) {
                if (text.startsWith("\n"))
                    text = text.slice(1);
                skipLf = false;
            }
            pending += text;
            const out = [];
            const ends = /[\r\n]/g;
            let start = 0;
            for (let m = ends.exec(pending); m !== null; m = ends.exec(pending)) {
                const at = m.index;
                line(pending.slice(start, at), out);
                if (pending[at] === "\r") {
                    if (at + 1 < pending.length) {
                        if (pending[at + 1] === "\n")
                            ends.lastIndex = at + 2;
                        start = pending[at + 1] === "\n" ? at + 2 : at + 1;
                    }
                    else {
                        skipLf = true;
                        start = at + 1;
                    }
                }
                else {
                    start = at + 1;
                }
            }
            pending = pending.slice(start);
            if (pending.length > MAX_PENDING)
                throw new Error("event stream line too long");
            return out;
        },
    };
}
