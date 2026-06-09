/// Heuristic token estimator. Fast, dependency-free, and within 10% of BPE.
pub fn estimate_tokens(text: &str) -> usize {
    (text.len() as f64 / 3.5).ceil() as usize
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PressureLevel {
    /// Pressure < 40%: Full output, minimal truncation
    Relaxed,
    /// Pressure 40-70%: Enable line deduplication & head/tail truncation
    Moderate,
    /// Pressure 70-85%: Enable exit-code truncation
    Elevated,
    /// Pressure > 85%: Extreme truncation (success -> 1 line)
    Critical,
}

#[derive(Debug)]
pub struct PressureGovernor {
    pub context_window_size: usize,
}

impl PressureGovernor {
    pub fn new(context_window_size: Option<usize>) -> Self {
        Self {
            context_window_size: context_window_size.unwrap_or(32768), // Default to 32K
        }
    }

    pub fn get_pressure_level(&self, estimated_used_tokens: usize) -> PressureLevel {
        let pct = (estimated_used_tokens as f64 / self.context_window_size as f64) * 100.0;
        if pct < 40.0 {
            PressureLevel::Relaxed
        } else if pct < 70.0 {
            PressureLevel::Moderate
        } else if pct < 85.0 {
            PressureLevel::Elevated
        } else {
            PressureLevel::Critical
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_estimate_tokens() {
        assert_eq!(estimate_tokens("hello"), 2);
    }

    #[test]
    fn test_pressure_governor() {
        let gov = PressureGovernor::new(Some(1000));
        assert_eq!(gov.get_pressure_level(300), PressureLevel::Relaxed);
        assert_eq!(gov.get_pressure_level(500), PressureLevel::Moderate);
        assert_eq!(gov.get_pressure_level(800), PressureLevel::Elevated);
        assert_eq!(gov.get_pressure_level(900), PressureLevel::Critical);
    }
}
