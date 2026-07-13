//! Directional braille "light panel" — the TUI's activity language.
//!
//! Two braille cells side by side form a 4×4 dot grid. The *direction* dots
//! move encodes what kind of activity is happening; the *rate* the frame
//! advances encodes how much. One grammar, applied everywhere:
//!
//! | Pattern       | Meaning                                             |
//! |---------------|-----------------------------------------------------|
//! | `Rain`        | output coming down to you (final answer streaming, log tail) |
//! | `Rise`        | your work going up/out (uploads, submissions)       |
//! | `ScrollLeft`  | data flowing in from an external process (op output)|
//! | `ScrollRight` | data flowing out to an external process (tool call dispatched) |
//! | `Shimmer`     | nondeterministic exploration (model thinking)       |
//! | `CrissCross`  | many things at once (instance aggregate activity)   |
//!
//! The generator is **stateless**: a pure function of `(seed, frame, pattern)`.
//! Movement falls out of how the frame index enters the hash — e.g. for
//! `ScrollLeft` column `c` at frame `t` shows `h(t + c)`, so at `t+1` column
//! `c` shows exactly what column `c+1` showed at `t`: the dots march left.
//! Callers animate by advancing a frame counter (fast per event, slow on a
//! heartbeat, frozen when nothing more is expected) — no per-widget mutable
//! state, no `Instant`s inside the generator, and every property is testable.

/// Direction the dots move. See the module table for the semantic grammar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelPattern {
    /// Dots fall top→bottom.
    Rain,
    /// Dots climb bottom→top.
    Rise,
    /// Dots stream right→left.
    ScrollLeft,
    /// Dots stream left→right.
    ScrollRight,
    /// Full re-randomisation each frame (the classic liveness shimmer).
    Shimmer,
    /// Alternating columns fall/climb — mixed directions, "many things".
    CrissCross,
}

/// Mix `(seed, a, b)` into a well-distributed 64-bit value (splitmix64-style).
fn mix(seed: u64, a: u64, b: u64) -> u64 {
    let mut x =
        seed ^ a.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ b.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x ^= x >> 30;
    x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^= x >> 31;
    x
}

/// Whether the pattern lights the dot at `(col, row)` of the 4×4 grid at
/// `frame`. Movement is encoded in how `frame` combines with the coordinate:
/// the value that was at a cell moves to the neighbouring cell one frame later.
fn dot(seed: u64, pattern: PanelPattern, frame: u64, col: u64, row: u64) -> bool {
    // ~45% dot density reads as a busy-but-sparse cluster.
    const DENSITY: u64 = 45;
    let h = match pattern {
        PanelPattern::ScrollLeft => mix(seed, frame.wrapping_add(col), row),
        PanelPattern::ScrollRight => mix(seed, frame.wrapping_sub(col), row),
        PanelPattern::Rain => mix(seed, col, frame.wrapping_sub(row)),
        PanelPattern::Rise => mix(seed, col, frame.wrapping_add(row)),
        PanelPattern::Shimmer => mix(seed, frame.wrapping_mul(4).wrapping_add(col), row),
        PanelPattern::CrissCross => {
            if col.is_multiple_of(2) {
                mix(seed, col, frame.wrapping_sub(row))
            } else {
                mix(seed, col, frame.wrapping_add(row))
            }
        }
    };
    h % 100 < DENSITY
}

/// Braille dot bit for `(col_in_cell, row)`: dots 1-3 and 7 form the left
/// column, dots 4-6 and 8 the right (Unicode Braille Patterns layout).
fn braille_bit(col_in_cell: u64, row: u64) -> u32 {
    match (col_in_cell, row) {
        (0, 3) => 0x40,
        (1, 3) => 0x80,
        (0, r) => 0x01 << r,
        (_, r) => 0x08 << r,
    }
}

/// Render the two-cell panel for `frame`. In unicode mode this is two braille
/// cells (never both blank — an empty frame gets a centre dot so the panel
/// stays visible); otherwise a two-char ASCII spinner driven by the same frame.
pub fn panel_glyphs(seed: u64, frame: u64, pattern: PanelPattern, unicode: bool) -> String {
    if !unicode {
        const ASCII: [char; 4] = ['|', '/', '-', '\\'];
        let c = ASCII[(frame % ASCII.len() as u64) as usize];
        return format!("{c}{c}");
    }

    let mut cells = [0u32; 2];
    for col in 0..4u64 {
        for row in 0..4u64 {
            if dot(seed, pattern, frame, col, row) {
                cells[(col / 2) as usize] |= braille_bit(col % 2, row);
            }
        }
    }
    if cells[0] == 0 && cells[1] == 0 {
        cells[0] = 0x02; // dot 2: left cell, second row — a quiet minimum
    }
    cells
        .iter()
        .map(|&bits| char::from_u32(0x2800 + bits).unwrap_or('⠿'))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The defining property of each directional pattern: the frame index
    /// shifts the grid one cell along the pattern's axis.
    #[test]
    fn patterns_move_along_their_axis() {
        let seed = 12345;
        for frame in 100..110u64 {
            for col in 0..3u64 {
                for row in 0..4u64 {
                    // ScrollLeft: what col c+1 shows now, col c shows next frame.
                    assert_eq!(
                        dot(seed, PanelPattern::ScrollLeft, frame + 1, col, row),
                        dot(seed, PanelPattern::ScrollLeft, frame, col + 1, row),
                    );
                    // ScrollRight: the mirror.
                    assert_eq!(
                        dot(seed, PanelPattern::ScrollRight, frame + 1, col + 1, row),
                        dot(seed, PanelPattern::ScrollRight, frame, col, row),
                    );
                }
            }
            for col in 0..4u64 {
                for row in 0..3u64 {
                    // Rain: a dot at row r falls to row r+1 next frame.
                    assert_eq!(
                        dot(seed, PanelPattern::Rain, frame + 1, col, row + 1),
                        dot(seed, PanelPattern::Rain, frame, col, row),
                    );
                    // Rise: a dot at row r+1 climbs to row r next frame.
                    assert_eq!(
                        dot(seed, PanelPattern::Rise, frame + 1, col, row),
                        dot(seed, PanelPattern::Rise, frame, col, row + 1),
                    );
                }
            }
        }
    }

    #[test]
    fn crisscross_mixes_directions_per_column() {
        let seed = 7;
        for frame in 50..55u64 {
            for row in 0..3u64 {
                // Even columns rain (fall), odd columns rise.
                assert_eq!(
                    dot(seed, PanelPattern::CrissCross, frame + 1, 0, row + 1),
                    dot(seed, PanelPattern::CrissCross, frame, 0, row),
                );
                assert_eq!(
                    dot(seed, PanelPattern::CrissCross, frame + 1, 1, row),
                    dot(seed, PanelPattern::CrissCross, frame, 1, row + 1),
                );
            }
        }
    }

    #[test]
    fn glyphs_are_visible_braille_and_two_cells() {
        for frame in 0..64u64 {
            for pattern in [
                PanelPattern::Rain,
                PanelPattern::Rise,
                PanelPattern::ScrollLeft,
                PanelPattern::ScrollRight,
                PanelPattern::Shimmer,
                PanelPattern::CrissCross,
            ] {
                let g = panel_glyphs(9, frame, pattern, true);
                assert_eq!(g.chars().count(), 2);
                assert!(
                    g.chars().any(|c| (0x2801..=0x28FF).contains(&(c as u32))),
                    "panel must never be fully blank: {g:?}"
                );
                assert!(
                    g.chars().all(|c| (0x2800..=0x28FF).contains(&(c as u32))),
                    "both cells must be braille: {g:?}"
                );
            }
        }
    }

    #[test]
    fn ascii_fallback_is_a_spinner() {
        let g = panel_glyphs(1, 3, PanelPattern::Rain, false);
        assert_eq!(g.chars().count(), 2);
        assert!(g.chars().all(|c| matches!(c, '|' | '/' | '-' | '\\')));
        // The frame drives the ASCII spinner too.
        assert_ne!(
            panel_glyphs(1, 0, PanelPattern::Rain, false),
            panel_glyphs(1, 1, PanelPattern::Rain, false)
        );
    }

    #[test]
    fn shimmer_varies_across_frames() {
        let mut seen = std::collections::HashSet::new();
        for frame in 0..16u64 {
            seen.insert(panel_glyphs(42, frame, PanelPattern::Shimmer, true));
        }
        assert!(seen.len() > 4, "shimmer should look random frame to frame");
    }
}
