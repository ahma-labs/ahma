//! The numeric multi-select prompt shared by the setup and uninstall wizards.
//!
//! Setup and uninstall are mirror images (see [`crate::harness_target`]), and
//! they used to hold two independent copies of this parser — identical in
//! behaviour, different in local variable names, each with its own test suite.
//! That is the worst shape for a duplicate: a fix to selection parsing has to be
//! made twice, and the two test suites both pass while the implementations
//! diverge, because neither suite ever sees the other's code.
//!
//! Accepted input, in the order it is tried:
//! * `all` (case-insensitive) — every option.
//! * A run of bare digits, when there are fewer than ten options — `134` means
//!   options 1, 3 and 4. Only unambiguous below ten, which is why the digit
//!   branch is gated on `max_val < 10`.
//! * Otherwise a list separated by whitespace, `,`, `.` or `;`.
//!
//! Out-of-range and unparseable entries are dropped rather than rejected, and
//! duplicates collapse. Selections are returned as zero-based indices in the
//! order the user gave them.

use std::io::{self, Write};

/// Prompt for a multi-select and return the chosen zero-based indices.
///
/// `default` is parsed the same way as typed input, so a caller passing `"all"`
/// gets every option when the user presses Enter.
pub(crate) fn prompt_multi_select(question: &str, options: &[&str], default: &str) -> Vec<usize> {
    println!("{}", question);
    for (i, opt) in options.iter().enumerate() {
        println!("  {}) {}", i + 1, opt);
    }
    print!("  Selection [default: {}]: ", default);
    let _ = io::stdout().flush();
    let mut input = String::new();
    if io::stdin().read_line(&mut input).is_err() || input.trim().is_empty() {
        return parse_selection_string(default, options.len());
    }
    parse_selection_string(&input, options.len())
}

/// Prompt a multi-select that defaults to every option. Non-interactive
/// sessions select everything without prompting.
pub(crate) fn prompt_multi_select_all(
    interactive: bool,
    question: &str,
    labels: &[&str],
) -> Vec<usize> {
    if !interactive {
        return (0..labels.len()).collect();
    }
    prompt_multi_select(question, labels, "all")
}

/// Parse a selection string into zero-based indices.
pub(crate) fn parse_selection_string(input: &str, max_val: usize) -> Vec<usize> {
    let trimmed = input.trim();
    if trimmed.eq_ignore_ascii_case("all") {
        return (0..max_val).collect();
    }

    // A run of bare digits is only unambiguous while every option is a single
    // digit; at ten or more, "12" has to mean option 12, not options 1 and 2.
    let is_pure_digits = !trimmed.is_empty() && trimmed.chars().all(|c| c.is_ascii_digit());
    if is_pure_digits && max_val < 10 {
        parse_digit_sequence(trimmed, max_val)
    } else {
        parse_separated_list(trimmed, max_val)
    }
}

fn parse_digit_sequence(input: &str, max_val: usize) -> Vec<usize> {
    collect_in_range(
        input
            .chars()
            .filter_map(|c| c.to_digit(10))
            .map(|d| d as usize),
        max_val,
    )
}

fn parse_separated_list(input: &str, max_val: usize) -> Vec<usize> {
    let normalized = input.replace([',', '.', ';'], " ");
    collect_in_range(
        normalized
            .split_whitespace()
            .filter_map(|part| part.parse::<usize>().ok()),
        max_val,
    )
}

/// Keep the 1-based entries within range, convert to zero-based, and drop
/// repeats while preserving the order they were given in.
fn collect_in_range(entries: impl Iterator<Item = usize>, max_val: usize) -> Vec<usize> {
    let mut selections = Vec::new();
    for idx in entries.filter(|&n| n >= 1 && n <= max_val).map(|n| n - 1) {
        if !selections.contains(&idx) {
            selections.push(idx);
        }
    }
    selections
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_selects_every_option_case_insensitively() {
        assert_eq!(parse_selection_string("all", 3), vec![0, 1, 2]);
        assert_eq!(parse_selection_string("  ALL  ", 2), vec![0, 1]);
    }

    #[test]
    fn a_digit_run_selects_each_digit_while_options_stay_single_digit() {
        assert_eq!(parse_selection_string("134", 5), vec![0, 2, 3]);
    }

    /// At ten or more options a digit run has to be read as one number, because
    /// "12" can legitimately mean option 12.
    #[test]
    fn a_digit_run_is_one_number_once_ten_options_exist() {
        assert_eq!(parse_selection_string("12", 12), vec![11]);
        assert_eq!(parse_selection_string("12", 5), vec![0, 1]);
    }

    #[test]
    fn separators_are_interchangeable() {
        for input in ["1,3", "1 3", "1.3", "1;3", " 1 , 3 "] {
            assert_eq!(
                parse_selection_string(input, 4),
                vec![0, 2],
                "input {input:?}"
            );
        }
    }

    #[test]
    fn out_of_range_and_unparseable_entries_are_dropped_not_rejected() {
        assert_eq!(parse_selection_string("1,99,x,2", 3), vec![0, 1]);
        assert_eq!(parse_selection_string("0", 3), Vec::<usize>::new());
    }

    #[test]
    fn repeats_collapse_and_the_given_order_is_kept() {
        assert_eq!(parse_selection_string("3,1,3", 3), vec![2, 0]);
        assert_eq!(parse_selection_string("313", 3), vec![2, 0]);
    }

    #[test]
    fn nothing_selectable_yields_nothing() {
        assert_eq!(parse_selection_string("", 3), Vec::<usize>::new());
        assert_eq!(parse_selection_string("   ", 3), Vec::<usize>::new());
        assert_eq!(parse_selection_string("all", 0), Vec::<usize>::new());
    }

    /// `0` is not a valid 1-based choice and `9` is past the end; both drop out
    /// of a digit run without taking the rest of the run with them.
    #[test]
    fn a_digit_run_drops_only_the_digits_that_are_out_of_range() {
        assert_eq!(parse_selection_string("0192", 5), vec![0, 1]);
    }

    #[test]
    fn a_non_interactive_select_all_takes_everything_without_prompting() {
        assert_eq!(
            prompt_multi_select_all(false, "q", &["a", "b", "c"]),
            vec![0, 1, 2]
        );
        assert_eq!(
            prompt_multi_select_all(false, "q", &[]),
            Vec::<usize>::new()
        );
    }
}
