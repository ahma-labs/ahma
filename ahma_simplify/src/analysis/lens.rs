//! Analysis lens selection for `ahma simplify --lens`.
//!
//! A lens is one independent analysis pass over the codebase. This module is
//! the single place that maps CLI strings to the set of lenses to run.

use std::str::FromStr;

const VALID_LENS_NAMES: &str = "complexity, reuse, dead-code, altitude, all";

/// One independent analysis pass over the codebase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lens {
    /// The existing metrics analysis (maintainability index, cognitive
    /// complexity, hotspot ranking).
    Complexity,
    /// Duplicate-code detection.
    Reuse,
    /// Exported symbols with no apparent references.
    DeadCode,
    /// Chains of functions that only forward to the next one.
    Altitude,
}

impl Lens {
    const ALL: [Lens; 4] = [
        Lens::Complexity,
        Lens::Reuse,
        Lens::DeadCode,
        Lens::Altitude,
    ];
}

impl FromStr for Lens {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "complexity" => Ok(Lens::Complexity),
            "reuse" => Ok(Lens::Reuse),
            "dead-code" | "dead_code" | "deadcode" => Ok(Lens::DeadCode),
            "altitude" => Ok(Lens::Altitude),
            other => Err(anyhow::anyhow!(
                "unknown lens '{other}'; valid values are: {VALID_LENS_NAMES}"
            )),
        }
    }
}

/// Parses `--lens` values into the deterministic, deduplicated set of lenses
/// to run. `"all"` expands to every [`Lens`] variant; an empty `values` slice
/// also means "all". The result is always ordered by [`Lens`] declaration
/// order, regardless of input order.
pub fn parse_lenses(values: &[String]) -> anyhow::Result<Vec<Lens>> {
    if values.is_empty() || values.iter().any(|v| v.trim().eq_ignore_ascii_case("all")) {
        return Ok(Lens::ALL.to_vec());
    }

    let mut requested = Vec::new();
    for value in values {
        let lens = value.parse::<Lens>()?;
        if !requested.contains(&lens) {
            requested.push(lens);
        }
    }

    Ok(Lens::ALL
        .into_iter()
        .filter(|lens| requested.contains(lens))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_complexity() {
        assert_eq!("complexity".parse::<Lens>().unwrap(), Lens::Complexity);
    }

    #[test]
    fn parses_reuse() {
        assert_eq!("reuse".parse::<Lens>().unwrap(), Lens::Reuse);
    }

    #[test]
    fn parses_altitude() {
        assert_eq!("altitude".parse::<Lens>().unwrap(), Lens::Altitude);
        assert_eq!("ALTITUDE".parse::<Lens>().unwrap(), Lens::Altitude);
    }

    #[test]
    fn parses_dead_code_in_each_spelling() {
        for spelling in ["dead-code", "dead_code", "deadcode", "DEAD-CODE"] {
            assert_eq!(
                spelling.parse::<Lens>().unwrap(),
                Lens::DeadCode,
                "{spelling}"
            );
        }
    }

    #[test]
    fn parsing_is_case_insensitive() {
        assert_eq!("Complexity".parse::<Lens>().unwrap(), Lens::Complexity);
        assert_eq!("REUSE".parse::<Lens>().unwrap(), Lens::Reuse);
        assert_eq!("ReUsE".parse::<Lens>().unwrap(), Lens::Reuse);
    }

    #[test]
    fn unknown_value_names_valid_options_in_error() {
        let err = "wrapper-chains".parse::<Lens>().unwrap_err();
        let message = err.to_string();
        assert!(message.contains("wrapper-chains"));
        assert!(message.contains("complexity"));
        assert!(message.contains("reuse"));
    }

    #[test]
    fn all_expands_to_every_variant_in_declaration_order() {
        let lenses = parse_lenses(&["all".to_string()]).unwrap();
        assert_eq!(
            lenses,
            vec![
                Lens::Complexity,
                Lens::Reuse,
                Lens::DeadCode,
                Lens::Altitude
            ]
        );
    }

    #[test]
    fn all_is_case_insensitive() {
        let lenses = parse_lenses(&["ALL".to_string()]).unwrap();
        assert_eq!(
            lenses,
            vec![
                Lens::Complexity,
                Lens::Reuse,
                Lens::DeadCode,
                Lens::Altitude
            ]
        );
    }

    #[test]
    fn empty_input_means_all() {
        let lenses = parse_lenses(&[]).unwrap();
        assert_eq!(
            lenses,
            vec![
                Lens::Complexity,
                Lens::Reuse,
                Lens::DeadCode,
                Lens::Altitude
            ]
        );
    }

    #[test]
    fn duplicates_collapse() {
        let lenses = parse_lenses(&[
            "reuse".to_string(),
            "reuse".to_string(),
            "complexity".to_string(),
        ])
        .unwrap();
        assert_eq!(lenses, vec![Lens::Complexity, Lens::Reuse]);
    }

    #[test]
    fn result_is_ordered_by_declaration_not_input_order() {
        let lenses = parse_lenses(&["reuse".to_string(), "complexity".to_string()]).unwrap();
        assert_eq!(lenses, vec![Lens::Complexity, Lens::Reuse]);
    }

    #[test]
    fn single_lens_selection() {
        let lenses = parse_lenses(&["reuse".to_string()]).unwrap();
        assert_eq!(lenses, vec![Lens::Reuse]);
    }

    #[test]
    fn unknown_value_in_list_is_an_error() {
        let err = parse_lenses(&["complexity".to_string(), "bogus".to_string()]).unwrap_err();
        assert!(err.to_string().contains("bogus"));
    }

    #[test]
    fn selecting_one_lens_excludes_the_others() {
        let lenses = parse_lenses(&["reuse".to_string()]).unwrap();
        assert!(lenses.contains(&Lens::Reuse));
        assert!(!lenses.contains(&Lens::Complexity));
    }
}
