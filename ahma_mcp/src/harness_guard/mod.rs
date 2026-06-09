pub mod format_healer;
pub mod loop_detector;
pub mod skill_injector;
pub mod write_guard;

pub use format_healer::{clean_json_trailing_commas, heal_tool_arguments, heal_tool_name};
pub use loop_detector::LoopDetector;
pub use skill_injector::SkillInjector;
pub use write_guard::check_write_allowance;

pub struct HarnessGuard {
    pub enabled: bool,
    pub loop_detector: LoopDetector,
    pub skill_injector: SkillInjector,
}

impl HarnessGuard {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            loop_detector: LoopDetector::new(3),
            skill_injector: SkillInjector::new(),
        }
    }
}
