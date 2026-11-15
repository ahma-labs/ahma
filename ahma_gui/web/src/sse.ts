// A Server-Sent Events parser, pure and incremental: feed it text as it arrives
// in whatever chunks the network chose, get back the events completed so far.
// Handles \n, \r\n and lone \r line ends (even when \r and \n land in different
// chunks), comment lines (":..."), multi-line data, and a leading BOM.

export interface SseEvent {
  /** The `event:` field, or "message" when the event named none. */
  event: string;
  /** The `data:` lines joined with "\n". */
  data: string;
}

export interface SseParser {
  feed(chunk: string): SseEvent[];
}

/** A line longer than this is a broken stream, not a message: fail and reconnect. */
const MAX_PENDING = 16 * 1024 * 1024;

export function createSseParser(): SseParser {
  let pending = "";
  let started = false;
  let skipLf = false; // the last chunk ended in \r; a leading \n now belongs to it
  let eventName = "";
  let dataLines: string[] = [];

  /** A blank line: dispatch the event gathered so far, if it has any data. */
  function dispatch(out: SseEvent[]): void {
    if (dataLines.length > 0) {
      out.push({ event: eventName === "" ? "message" : eventName, data: dataLines.join("\n") });
    }
    eventName = "";
    dataLines = [];
  }

  function line(text: string, out: SseEvent[]): void {
    if (text === "") return dispatch(out);
    if (text.startsWith(":")) return;
    const colon = text.indexOf(":");
    const field = colon === -1 ? text : text.slice(0, colon);
    let value = colon === -1 ? "" : text.slice(colon + 1);
    if (value.startsWith(" ")) value = value.slice(1);
    if (field === "event") eventName = value;
    else if (field === "data") dataLines.push(value);
    // `id` and `retry` are not used by this page.
  }

  /** Drop what is not content: a leading BOM, and the \n that completes a \r from the last chunk. */
  function trimChunk(chunk: string): string {
    let text = chunk;
    if (text.length === 0) return text;
    if (!started) {
      started = true;
      if (text.charCodeAt(0) === 0xfeff) text = text.slice(1);
    }
    if (skipLf) {
      if (text.startsWith("\n")) text = text.slice(1);
      skipLf = false;
    }
    return text;
  }

  /** Where the next line starts after the terminator at `at`; steers `ends` past a \r\n pair. */
  function nextLineStart(at: number, ends: RegExp): number {
    if (pending[at] !== "\r") return at + 1;
    if (at + 1 >= pending.length) {
      skipLf = true; // the \n, if any, arrives in the next chunk
      return at + 1;
    }
    if (pending[at + 1] !== "\n") return at + 1;
    ends.lastIndex = at + 2;
    return at + 2;
  }

  return {
    feed(chunk: string): SseEvent[] {
      pending += trimChunk(chunk);

      const out: SseEvent[] = [];
      const ends = /[\r\n]/g;
      let start = 0;
      for (let m = ends.exec(pending); m !== null; m = ends.exec(pending)) {
        line(pending.slice(start, m.index), out);
        start = nextLineStart(m.index, ends);
      }
      pending = pending.slice(start);
      if (pending.length > MAX_PENDING) throw new Error("event stream line too long");
      return out;
    },
  };
}
