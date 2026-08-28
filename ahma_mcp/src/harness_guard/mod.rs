pub mod format_healer;
pub mod loop_detector;
pub mod skill_injector;
pub mod write_guard;

pub use format_healer::{clean_json_trailing_commas, heal_tool_arguments, heal_tool_name};
pub use loop_detector::LoopDetector;
pub use skill_injector::SkillInjector;
pub use write_guard::check_write_allowance;

use parking_lot::Mutex;
use serde_json::{Map, Value};

/// Outcome of a [`ToolGuard`] inspecting a pending tool call.
pub enum GuardOutcome {
    /// Proceed with the call. The guard may have healed the name/args in place.
    Proceed,
    /// Block the call; return this message to the model instead of executing.
    Block(String),
}

/// Read-only context a guard may need while inspecting a call.
pub struct GuardContext<'a> {
    /// Every currently-known tool name (hard-coded + configured), used for
    /// near-miss name healing.
    pub known_tools: &'a [&'a str],
}

/// A single self-correction guard. Guards run in order before a tool call; each
/// may heal the name/args in place or block the call. `observe` lets stateful
/// guards (e.g. loop detection) record the outcome after the call completes.
///
/// This is the extension point for harness self-correction: add a new behaviour
/// by implementing `ToolGuard` and pushing it into [`HarnessGuard::pipeline`].
pub trait ToolGuard: Send + Sync {
    /// Inspect (and optionally heal) a pending call; optionally block it.
    fn inspect(
        &self,
        ctx: &GuardContext,
        name: &mut String,
        args: &mut Map<String, Value>,
    ) -> GuardOutcome;

    /// Observe a completed call. Default: no-op (stateless guards).
    fn observe(&self, _name: &str, _args: &Map<String, Value>, _failed: bool) {}
}

/// Corrects near-miss tool-name typos (Levenshtein distance ≤ 2).
pub struct NameHealingGuard;

impl ToolGuard for NameHealingGuard {
    fn inspect(
        &self,
        ctx: &GuardContext,
        name: &mut String,
        _args: &mut Map<String, Value>,
    ) -> GuardOutcome {
        if let Some(healed) = heal_tool_name(name, ctx.known_tools)
            && healed != *name
        {
            tracing::warn!("Healed tool name from '{name}' to '{healed}'");
            *name = healed;
        }
        GuardOutcome::Proceed
    }
}

/// Heals known argument-format mistakes (e.g. a string where a singleton array
/// is expected for `run_terminal_command`).
pub struct ArgHealingGuard;

impl ToolGuard for ArgHealingGuard {
    fn inspect(
        &self,
        _ctx: &GuardContext,
        name: &mut String,
        args: &mut Map<String, Value>,
    ) -> GuardOutcome {
        heal_tool_arguments(name, args);
        GuardOutcome::Proceed
    }
}

/// Breaks failure loops: blocks a call whose identical (name, args) has already
/// failed `max_retries` times, nudging the model toward a different approach.
pub struct LoopGuard {
    detector: Mutex<LoopDetector>,
}

impl LoopGuard {
    pub fn new(max_retries: u32) -> Self {
        Self {
            detector: Mutex::new(LoopDetector::new(max_retries)),
        }
    }

    fn args_key(args: &Map<String, Value>) -> String {
        Value::Object(args.clone()).to_string()
    }
}

impl ToolGuard for LoopGuard {
    fn inspect(
        &self,
        _ctx: &GuardContext,
        name: &mut String,
        args: &mut Map<String, Value>,
    ) -> GuardOutcome {
        let key = Self::args_key(args);
        let is_loop = self.detector.lock().is_loop(name, &key);
        if is_loop {
            GuardOutcome::Block(
                "LOOP_DETECTED: This exact call has failed 3 times. The approach is not working.\n\
                 Hint: Re-read the error messages above. Try a fundamentally different approach or read relevant documentation first."
                    .to_string(),
            )
        } else {
            GuardOutcome::Proceed
        }
    }

    fn observe(&self, name: &str, args: &Map<String, Value>, failed: bool) {
        let key = Self::args_key(args);
        let mut d = self.detector.lock();
        if failed {
            d.record_failure(name, &key);
        } else {
            d.record_success();
        }
    }
}

/// The ordered self-correction pipeline applied to every tool call when
/// `enabled`. Guards heal near-miss names/args and break failure loops.
pub struct HarnessGuard {
    pub enabled: bool,
    pub pipeline: Vec<Box<dyn ToolGuard>>,
}

impl HarnessGuard {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            pipeline: vec![
                Box::new(NameHealingGuard),
                Box::new(ArgHealingGuard),
                Box::new(LoopGuard::new(3)),
            ],
        }
    }

    /// Run every guard's `inspect` in order. The first `Block` short-circuits
    /// and is returned; otherwise the (possibly healed) call proceeds.
    pub fn inspect(
        &self,
        ctx: &GuardContext,
        name: &mut String,
        args: &mut Map<String, Value>,
    ) -> GuardOutcome {
        for guard in &self.pipeline {
            if let GuardOutcome::Block(msg) = guard.inspect(ctx, name, args) {
                return GuardOutcome::Block(msg);
            }
        }
        GuardOutcome::Proceed
    }

    /// Notify every guard of a completed call so stateful guards can update.
    pub fn observe(&self, name: &str, args: &Map<String, Value>, failed: bool) {
        for guard in &self.pipeline {
            guard.observe(name, args, failed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx<'a>(known: &'a [&'a str]) -> GuardContext<'a> {
        GuardContext { known_tools: known }
    }

    #[test]
    fn pipeline_heals_name_and_args() {
        let guard = HarnessGuard::new(true);
        let known = ["run_terminal_command", "status"];
        let mut name = "run_terminal_commnd".to_string();
        let mut args: Map<String, Value> = serde_json::from_value(serde_json::json!({
            "args": "echo hi"
        }))
        .unwrap();

        let outcome = guard.inspect(&ctx(&known), &mut name, &mut args);
        assert!(matches!(outcome, GuardOutcome::Proceed));
        assert_eq!(name, "run_terminal_command");
        assert_eq!(args.get("args").unwrap(), &serde_json::json!(["echo hi"]));
    }

    #[test]
    fn pipeline_blocks_after_three_identical_failures() {
        let guard = HarnessGuard::new(true);
        let known = ["status"];
        let args = Map::new();

        for _ in 0..3 {
            guard.observe("status", &args, true);
        }
        let mut name = "status".to_string();
        let mut a = args.clone();
        match guard.inspect(&ctx(&known), &mut name, &mut a) {
            GuardOutcome::Block(msg) => assert!(msg.contains("LOOP_DETECTED")),
            GuardOutcome::Proceed => panic!("expected the loop to be blocked"),
        }
    }

    #[test]
    fn pipeline_success_clears_the_loop() {
        let guard = HarnessGuard::new(true);
        let known = ["status"];
        let args = Map::new();
        for _ in 0..3 {
            guard.observe("status", &args, true);
        }
        guard.observe("status", &args, false); // success resets
        let mut name = "status".to_string();
        let mut a = args.clone();
        assert!(matches!(
            guard.inspect(&ctx(&known), &mut name, &mut a),
            GuardOutcome::Proceed
        ));
    }
}
