// The transport: same-origin fetches with the bearer token. The token lives in
// this module's `token` variable and nowhere else — not in the URL, storage,
// cookies, a window property or the DOM — so a reload means pairing again.

import type { ErrorBody, WireIntent, WireReport } from "./wire.ts";

let token: string | null = null;
let onUnauthorized: () => void = () => {};

/** A non-2xx answer (or no answer). `code` and `message` are the server's own. */
export class ApiError extends Error {
  status: number;
  code: string;
  constructor(status: number, code: string, message: string) {
    super(message);
    this.name = "ApiError";
    this.status = status;
    this.code = code;
  }
}

/** Called when an authenticated request is refused as unauthorized. */
export function setOnUnauthorized(fn: () => void): void {
  onUnauthorized = fn;
}

export function isPaired(): boolean {
  return token !== null;
}

export function forgetToken(): void {
  token = null;
}

async function toApiError(res: Response): Promise<ApiError> {
  try {
    const body = (await res.json()) as Partial<ErrorBody>;
    const e = body.error;
    if (e && typeof e.code === "string" && typeof e.message === "string") {
      return new ApiError(res.status, e.code, e.message);
    }
  } catch {
    // not JSON: fall through to the generic message
  }
  return new ApiError(res.status, "http", `The server answered ${res.status}.`);
}

const UNREACHABLE = "Could not reach the server.";

/** Exchange the code the human read from the terminal for a session token. */
export async function pair(code: string): Promise<void> {
  let res: Response;
  try {
    res = await fetch("/api/pair", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ code }),
      credentials: "omit",
      cache: "no-store",
    });
  } catch {
    throw new ApiError(0, "network", UNREACHABLE);
  }
  if (!res.ok) throw await toApiError(res);
  const body = (await res.json()) as { token?: unknown };
  if (typeof body.token !== "string" || body.token === "") {
    throw new ApiError(res.status, "bad_response", "The server's answer had no token.");
  }
  token = body.token;
}

async function authed(path: string, init: RequestInit): Promise<Response> {
  if (token === null) throw new ApiError(401, "unauthorized", "Not paired.");
  let res: Response;
  try {
    res = await fetch(path, {
      ...init,
      headers: {
        ...(init.headers as Record<string, string> | undefined),
        Authorization: `Bearer ${token}`,
      },
      credentials: "omit",
      cache: "no-store",
      referrerPolicy: "no-referrer",
    });
  } catch (err) {
    if (err instanceof DOMException && err.name === "AbortError") throw err;
    throw new ApiError(0, "network", UNREACHABLE);
  }
  if (!res.ok) throw await toApiError(res);
  return res;
}

async function post(path: string, body: unknown, init: RequestInit = {}): Promise<void> {
  try {
    await authed(path, {
      ...init,
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body),
    });
  } catch (err) {
    if (err instanceof ApiError && err.status === 401) onUnauthorized();
    throw err;
  }
}

/** Ask for something. Rejects with the server's own message when it refuses. */
export function postIntent(intent: WireIntent): Promise<void> {
  return post("/api/intent", intent);
}

/** Tell the server what this tab is showing. Bounded, so a hung one cannot stall the next. */
export function postReport(report: WireReport): Promise<void> {
  return post("/api/report", report, { keepalive: true, signal: AbortSignal.timeout(5000) });
}

/** Open the event stream; the caller reads `response.body`. */
export function openEvents(signal: AbortSignal): Promise<Response> {
  return authed("/api/events", {
    method: "GET",
    headers: { Accept: "text/event-stream" },
    signal,
  });
}
