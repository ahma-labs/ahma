// The transport: same-origin fetches with the bearer token. The token lives in
// this module's `token` variable and nowhere else — not in the URL, storage,
// cookies or the DOM — so a reload means pairing again. The one exception is
// `ahma desktop`'s own window: there the native host puts the token on a
// frozen global before this code runs (SPEC R-DESK.2), and nothing pairs.
let token = null;
let onUnauthorized = () => { };
/** A non-2xx answer (or no answer). `code` and `message` are the server's own. */
export class ApiError extends Error {
    status;
    code;
    constructor(status, code, message) {
        super(message);
        this.name = "ApiError";
        this.status = status;
        this.code = code;
    }
}
/** Called when an authenticated request is refused as unauthorized. */
export function setOnUnauthorized(fn) {
    onUnauthorized = fn;
}
/** The token `ahma desktop`'s window was given, if this page is in one. */
export function desktopToken() {
    const host = globalThis.__ahmaDesktop;
    return typeof host?.token === "string" && host.token !== "" ? host.token : null;
}
/** Use `ahma desktop`'s token: this page needs no pairing. */
export function adoptToken(given) {
    token = given;
}
export function forgetToken() {
    token = null;
}
async function toApiError(res) {
    try {
        const body = (await res.json());
        const e = body.error;
        if (e && typeof e.code === "string" && typeof e.message === "string") {
            return new ApiError(res.status, e.code, e.message);
        }
    }
    catch {
        // not JSON: fall through to the generic message
    }
    return new ApiError(res.status, "http", `The server answered ${res.status}.`);
}
const UNREACHABLE = "Could not reach the server.";
/** Exchange the code the human read from the terminal for a session token. */
export async function pair(code) {
    let res;
    try {
        res = await fetch("/api/pair", {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify({ code }),
            credentials: "omit",
            cache: "no-store",
        });
    }
    catch {
        throw new ApiError(0, "network", UNREACHABLE);
    }
    if (!res.ok)
        throw await toApiError(res);
    const body = (await res.json());
    if (typeof body.token !== "string" || body.token === "") {
        throw new ApiError(res.status, "bad_response", "The server's answer had no token.");
    }
    token = body.token;
}
async function authed(path, init) {
    if (token === null)
        throw new ApiError(401, "unauthorized", "Not paired.");
    let res;
    try {
        res = await fetch(path, {
            ...init,
            headers: {
                ...init.headers,
                Authorization: `Bearer ${token}`,
            },
            credentials: "omit",
            cache: "no-store",
            referrerPolicy: "no-referrer",
        });
    }
    catch (err) {
        if (err instanceof DOMException && err.name === "AbortError")
            throw err;
        throw new ApiError(0, "network", UNREACHABLE);
    }
    if (!res.ok)
        throw await toApiError(res);
    return res;
}
async function post(path, body, init = {}) {
    try {
        await authed(path, {
            ...init,
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify(body),
        });
    }
    catch (err) {
        if (err instanceof ApiError && err.status === 401)
            onUnauthorized();
        throw err;
    }
}
/** Ask for something. Rejects with the server's own message when it refuses. */
export function postIntent(intent) {
    return post("/api/intent", intent);
}
/** Tell the server what this tab is showing. Bounded, so a hung one cannot stall the next. */
export function postReport(report) {
    return post("/api/report", report, { keepalive: true, signal: AbortSignal.timeout(5000) });
}
/** Open the event stream; the caller reads `response.body`. */
export function openEvents(signal) {
    return authed("/api/events", {
        method: "GET",
        headers: { Accept: "text/event-stream" },
        signal,
    });
}
