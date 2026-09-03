/// Clean ANSI escape sequences and resolve carriage-return progress bars.
/// Regex-free, high-performance state-machine logic.
pub fn strip_ansi_and_carriage_returns(input: &str) -> String {
    let mut cleaned = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next(); // consume '['
                while let Some(&next) = chars.peek() {
                    chars.next();
                    if next.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
        } else {
            cleaned.push(c);
        }
    }

    // Resolve carriage returns by keeping only the segment after the last '\r' on each line
    let mut final_output = Vec::new();
    for line in cleaned.lines() {
        if let Some(last_r_idx) = line.rfind('\r') {
            final_output.push(line[last_r_idx + 1..].to_string());
        } else {
            final_output.push(line.to_string());
        }
    }

    let mut result = final_output.join("\n");
    if input.ends_with('\n') && !result.ends_with('\n') {
        result.push('\n');
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_ansi_colors() {
        let colored = "\x1b[31mError:\x1b[0m \x1b[1mSomething failed\x1b[0m";
        assert_eq!(
            strip_ansi_and_carriage_returns(colored),
            "Error: Something failed"
        );
    }

    #[test]
    fn test_carriage_returns() {
        let progress = "Downloading...\r10% finished\r100% finished";
        assert_eq!(strip_ansi_and_carriage_returns(progress), "100% finished");
    }

    #[test]
    fn test_normal_text() {
        let text = "Hello World\nNew Line";
        assert_eq!(
            strip_ansi_and_carriage_returns(text),
            "Hello World\nNew Line"
        );
    }
}
