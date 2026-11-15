// Small pure formatters shared by the views.
/** "/Users/me/code/project" -> "…/code/project"; short paths are left alone. */
export function shortenPath(path, keep = 2) {
    const parts = path.split(/[\\/]+/).filter((p) => p !== "");
    if (parts.length <= keep)
        return path;
    return "…/" + parts.slice(-keep).join("/");
}
/** 850 -> "850ms", 1234 -> "1.2s", 184000 -> "3m 04s", 3720000 -> "1h 02m". */
export function formatDuration(ms) {
    if (ms < 1000)
        return `${Math.round(ms)}ms`;
    const s = ms / 1000;
    if (s < 10)
        return `${s.toFixed(1)}s`;
    if (s < 60)
        return `${Math.round(s)}s`;
    const m = Math.floor(s / 60);
    if (m < 60)
        return `${m}m ${String(Math.floor(s % 60)).padStart(2, "0")}s`;
    return `${Math.floor(m / 60)}h ${String(m % 60).padStart(2, "0")}m`;
}
/**
 * Whether `typed` reads as a whole pairing code: eight letters or digits once
 * case, spaces and the dash are ignored, as the server reads it.
 */
export function looksLikeFullCode(typed) {
    return /^[0-9A-Z]{8}$/.test(typed.replace(/[\s-]/g, "").toUpperCase());
}
