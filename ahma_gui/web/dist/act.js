// How a view asks the server for something: send the intent, and when the server
// refuses, show its own words. The page never retries and never reads the code.
import { ApiError, postIntent } from "./api.js";
import { notify } from "./view/toast.js";
/** Resolves true when the server accepted the intent (204). */
export async function send(intent) {
    try {
        await postIntent(intent);
        return true;
    }
    catch (err) {
        if (err instanceof ApiError) {
            if (err.status !== 401)
                notify(err.message); // 401 returns to pairing instead
        }
        else {
            notify("Something went wrong sending that.");
        }
        return false;
    }
}
