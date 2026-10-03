//! Whether a sandboxed command may open the GPU (SPEC R6.2.7).
//!
//! Off by default: the Seatbelt profile is `(deny default)` and grants no
//! `iokit-open`, so Metal cannot create a device or a command queue inside the
//! sandbox — llama.cpp reports `failed to create command queue`, a Metal probe
//! sees no GPU, and GPU-accelerated tests silently fall back to the CPU or fail.
//! That is a *capability*, not a path: it cannot be granted at a prompt, and the
//! request flow must say so instead of offering a directory to grant.
//!
//! The opt-in (`[sandbox] allow_gpu = true`) adds the narrow user-client set
//! Apple's own profiles use for Metal — the Apple-silicon (`AGX*`) and
//! Intel/AMD (`IOAccel*`) device, shared and context clients plus `IOSurface` —
//! never a blanket `(allow iokit-open)`, which would also hand over cameras,
//! HID devices and every other driver with a user client.
//!
//! Process-global like the keychain and signal toggles: set once at startup
//! from the resolved settings, read by every profile generation.

use std::sync::atomic::{AtomicBool, Ordering};

static ALLOW_GPU: AtomicBool = AtomicBool::new(false);

/// Install the resolved `[sandbox] allow_gpu` value.
pub fn set_allow_gpu(allowed: bool) {
    ALLOW_GPU.store(allowed, Ordering::Relaxed);
}

/// Whether sandboxed commands may open the GPU.
pub fn allow_gpu_enabled() -> bool {
    ALLOW_GPU.load(Ordering::Relaxed)
}

/// The IOKit user-client classes Metal needs. Taken from Apple's shipped
/// `/System/Library/Sandbox/Profiles/*.sb`, which allow exactly these for
/// GPU-using daemons: `AGX*` is Apple silicon hardware, `IOAccel*` and
/// `IGAccel*` are AMD and Intel, and `IOGPUDeviceUserClient` is the IOGPUFamily
/// client that paravirtualised GPUs present inside a VM (Apple's own
/// `ParavirtualizedGraphicsGPUTask.sb` allows exactly that one plus IOSurface),
/// which is what a macOS CI runner has.
const GPU_USER_CLIENTS: &[&str] = &[
    "AGXDeviceUserClient",
    "AGXSharedUserClient",
    "AGXCommandQueue",
    "IOGPUDeviceUserClient",
    "IOAccelDevice2",
    "IOAccelSharedUserClient2",
    "IOAccelContext2",
    "IOAccelCommandQueue",
    "IGAccelDevice",
    "IGAccelSharedUserClient",
    "IGAccelCommandQueue",
    "IOSurfaceRootUserClient",
    "IOSurfaceAcceleratorClient",
];

/// The Seatbelt rules for GPU access under the current setting: empty when the
/// GPU stays denied, the narrow Metal allow-set when it is enabled.
pub fn seatbelt_gpu_rules() -> String {
    if !allow_gpu_enabled() {
        return String::new();
    }
    let mut out = String::from("(allow iokit-get-properties)\n(allow iokit-open\n");
    for class in GPU_USER_CLIENTS {
        out.push_str(&format!("    (iokit-user-client-class \"{class}\")\n"));
    }
    out.push_str(")\n");
    out
}

/// Explain a GPU failure the sandbox caused, if `stderr`/`stdout` show one, so
/// the agent learns it hit a boundary a human can lift in settings — and does
/// not ask for a directory grant that cannot help.
pub fn gpu_denial_note(stderr: &str, stdout: &str) -> Option<String> {
    let hit = stderr
        .lines()
        .chain(stdout.lines())
        .any(looks_like_gpu_denial);
    if !hit {
        return None;
    }
    if allow_gpu_enabled() {
        // The GPU is opened, but something Metal needs is still withheld (a
        // buffer, a shader compile): say so rather than call it the tool's
        // own failure, which is what silence implied.
        return Some(
            "A Metal operation failed although `[sandbox] allow_gpu` is already on (SPEC R6.2.7), \
             so a part of the GPU stack the sandbox still withholds is suspected. This is a \
             capability, not a path — no `sandbox_grant` can help. Tell the human, and quote the \
             failing lines above so the missing piece can be identified; or continue on the CPU."
                .to_string(),
        );
    }
    Some(
        "The sandbox denied GPU access: Metal cannot open the GPU inside a sandboxed command \
         (SPEC R6.2.7), so GPU-accelerated work falls back to the CPU or fails with errors like \
         `failed to create command queue`, a failed Metal buffer allocation, or no Metal device. \
         This is a capability, not a path — no `sandbox_grant` can help. Nothing more for you to \
         do here: tell the human that `[sandbox] allow_gpu = true` in ~/.ahma/settings.toml \
         enables it (takes effect on the next command for terminal hooks, on the next server \
         start for an MCP session), or continue on the CPU."
            .to_string(),
    )
}

fn looks_like_gpu_denial(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    let metal = lower.contains("metal") || lower.contains("mtl");
    (lower.contains("failed to create command queue"))
        || lower.contains("mtlcreatesystemdefaultdevice")
        || lower.contains("no metal device")
        || lower.contains("metal device not found")
        || lower.contains("unable to create metal device")
        // ggml (whisper.cpp, llama.cpp) and Metal buffer allocations.
        || (metal && lower.contains("failed to allocate"))
        || (lower.contains("mtlbuffer") && lower.contains("nil"))
        || (lower.contains("deny(") && lower.contains("iokit-open"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpu_stays_denied_by_default_and_opt_in_adds_only_metal_clients() {
        set_allow_gpu(false);
        assert_eq!(seatbelt_gpu_rules(), "");
        set_allow_gpu(true);
        let rules = seatbelt_gpu_rules();
        assert!(rules.contains("(allow iokit-open\n"), "{rules}");
        assert!(rules.contains("\"AGXDeviceUserClient\""), "{rules}");
        assert!(rules.contains("\"IOAccelDevice2\""), "{rules}");
        assert!(
            !rules.contains("(allow iokit-open)"),
            "never a blanket iokit allow: {rules}"
        );
        set_allow_gpu(false);
    }

    #[test]
    fn a_gpu_failure_is_explained_as_a_capability_not_a_path() {
        set_allow_gpu(false);
        let note = gpu_denial_note("ggml_metal_init: error: failed to create command queue", "")
            .expect("llama.cpp denial");
        assert!(note.contains("allow_gpu"), "{note}");
        assert!(note.contains("not a path"), "{note}");
        assert!(gpu_denial_note("", "MTLCreateSystemDefaultDevice() returned nil").is_some());
        assert!(gpu_denial_note("Permission denied: /etc/shadow", "").is_none());
        set_allow_gpu(true);
        let allowed = gpu_denial_note("failed to create command queue", "")
            .expect("a Metal failure with the GPU allowed is still explained");
        assert!(
            allowed.contains("allow_gpu` is already on") && allowed.contains("quote"),
            "{allowed}"
        );
        set_allow_gpu(false);
    }

    /// whisper.cpp and llama.cpp report a failed Metal allocation through
    /// ggml; that is the same withheld capability (bug report: an app
    /// segfaulted after "fails to allocate a Metal buffer").
    #[test]
    fn a_failed_metal_allocation_is_recognised() {
        set_allow_gpu(false);
        for line in [
            "ggml_metal_init: error: failed to allocate buffer, size = 256.00 MiB",
            "ggml_backend_metal_buffer_type_alloc_buffer: error: failed to allocate buffer",
            "whisper_init: MTLBuffer allocation returned nil",
        ] {
            assert!(gpu_denial_note(line, "").is_some(), "{line}");
        }
        assert!(gpu_denial_note("failed to allocate buffer for tokenizer", "").is_none());
    }
}
