//! The section accordion's animation (SPEC R24.9).
//!
//! Opening one section closes whichever was open, and both move together over
//! 300 ms so the eye can follow which line went where. Without the tween the
//! rows below the header jump by a dozen lines in one frame and the reader has
//! to re-find their place.
//!
//! It is a **layout** tween and nothing else: heights change, colours do not.
//!
//! Pure: every function takes the current time rather than reading a clock, so
//! the whole animation is testable at exact instants without a terminal or a
//! sleep.

/// How long a section takes to open or close.
pub const DURATION_MS: u64 = 300;

/// Frame interval while animating — about 66 fps, which is what makes 300 ms
/// read as movement rather than as three or four steps.
pub const FRAME_MS: u64 = 15;

/// One section on the move.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Track {
    pub key: String,
    /// Rows it started this movement at.
    pub from: usize,
}

/// The pair of sections currently moving.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccordionAnim {
    /// The section growing to its full height.
    pub opening: Option<Track>,
    /// The section shrinking to nothing.
    pub closing: Option<Track>,
    pub started_at_ms: u64,
}

/// Ease-out cubic: quick to start, settling at the end.
///
/// The settle is the point — the last frames are where a reader picks out where
/// a line came to rest.
pub fn ease_out_cubic(t: f64) -> f64 {
    let t = t.clamp(0.0, 1.0);
    let u = 1.0 - t;
    1.0 - u * u * u
}

impl AccordionAnim {
    /// How far through the movement we are, 0.0 to 1.0.
    pub fn progress(&self, now_ms: u64) -> f64 {
        let elapsed = now_ms.saturating_sub(self.started_at_ms) as f64;
        (elapsed / DURATION_MS as f64).clamp(0.0, 1.0)
    }

    /// Still moving?
    pub fn is_active(&self, now_ms: u64) -> bool {
        self.progress(now_ms) < 1.0
    }

    /// The height `key` should be drawn at this frame, or `None` when it is not
    /// part of this movement.
    ///
    /// `natural` — the section's full height — is re-read every frame rather
    /// than captured at the start, because a section that is opening while its
    /// command prints output grows as it opens.
    pub fn height_for(&self, key: &str, natural: usize, now_ms: u64) -> Option<usize> {
        let eased = ease_out_cubic(self.progress(now_ms));
        if let Some(track) = &self.opening
            && track.key == key
        {
            return Some((eased * natural as f64).round() as usize);
        }
        if let Some(track) = &self.closing
            && track.key == key
        {
            // The closing height is frozen at retarget: a section that is on
            // its way out must not grow because new output arrived for it.
            return Some(((1.0 - eased) * track.from as f64).round() as usize);
        }
        None
    }

    /// Begin moving from whatever is on screen now to `next` being open.
    ///
    /// `prev` is the movement in flight, if any: interrupting one halfway must
    /// start the outgoing section from the height it is *currently drawn at*,
    /// not from its full height, or it jumps outwards before shrinking.
    pub fn retarget(
        prev: Option<&AccordionAnim>,
        open_now: Option<&str>,
        next: Option<&str>,
        natural_of: impl Fn(&str) -> usize,
        now_ms: u64,
    ) -> Option<AccordionAnim> {
        if open_now == next {
            return None;
        }
        let closing = open_now.map(|key| {
            let from = prev
                .and_then(|p| p.height_for(key, natural_of(key), now_ms))
                .unwrap_or_else(|| natural_of(key));
            Track {
                key: key.to_string(),
                from,
            }
        });
        let opening = next.map(|key| Track {
            key: key.to_string(),
            from: prev
                .and_then(|p| p.height_for(key, natural_of(key), now_ms))
                .unwrap_or(0),
        });
        if closing.is_none() && opening.is_none() {
            return None;
        }
        Some(AccordionAnim {
            opening,
            closing,
            started_at_ms: now_ms,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn anim(open: Option<&str>, close: Option<(&str, usize)>, started: u64) -> AccordionAnim {
        AccordionAnim {
            opening: open.map(|k| Track {
                key: k.into(),
                from: 0,
            }),
            closing: close.map(|(k, from)| Track {
                key: k.into(),
                from,
            }),
            started_at_ms: started,
        }
    }

    #[test]
    fn a_section_opens_from_nothing_to_its_full_height() {
        let a = anim(Some("a"), None, 1_000);
        assert_eq!(a.height_for("a", 20, 1_000), Some(0));
        assert_eq!(a.height_for("a", 20, 1_000 + DURATION_MS), Some(20));
        // ease_out_cubic(0.5) = 0.875 → 17.5 rows, rounded.
        assert_eq!(a.height_for("a", 20, 1_150), Some(18));
        assert_eq!(a.height_for("b", 20, 1_150), None, "only its own section");
    }

    #[test]
    fn a_section_closes_from_its_height_to_nothing() {
        let a = anim(None, Some(("a", 20)), 0);
        assert_eq!(a.height_for("a", 20, 0), Some(20));
        assert_eq!(a.height_for("a", 20, DURATION_MS), Some(0));
        assert!(a.height_for("a", 20, 150).unwrap() < 20);
    }

    /// Movement in one direction only: a height that went backwards would read
    /// as a stutter.
    #[test]
    fn heights_move_monotonically() {
        let opening = anim(Some("a"), Some(("b", 12)), 0);
        let mut last_open = 0usize;
        let mut last_close = 12usize;
        let mut t = 0;
        while t <= DURATION_MS {
            let o = opening.height_for("a", 20, t).unwrap();
            let c = opening.height_for("b", 12, t).unwrap();
            assert!(o >= last_open, "opening went backwards at {t}ms");
            assert!(c <= last_close, "closing grew at {t}ms");
            last_open = o;
            last_close = c;
            t += FRAME_MS;
        }
        assert_eq!(last_open, 20);
        assert_eq!(last_close, 0);
    }

    #[test]
    fn the_animation_ends_and_stays_ended() {
        let a = anim(Some("a"), None, 500);
        assert!(a.is_active(500));
        assert!(a.is_active(500 + DURATION_MS - 1));
        assert!(!a.is_active(500 + DURATION_MS));
        assert!(!a.is_active(500 + DURATION_MS * 10));
    }

    /// Interrupting a movement halfway must not make the outgoing section jump
    /// back to full height before it shrinks.
    #[test]
    fn interrupting_a_movement_continues_from_where_it_is_drawn() {
        let natural = |key: &str| if key == "a" { 20 } else { 12 };
        let first = AccordionAnim::retarget(None, None, Some("a"), natural, 0).unwrap();

        let mid_height = first.height_for("a", 20, 150).unwrap();
        assert!(mid_height > 0 && mid_height < 20);

        let second =
            AccordionAnim::retarget(Some(&first), Some("a"), Some("b"), natural, 150).unwrap();
        assert_eq!(
            second.closing.as_ref().unwrap().from,
            mid_height,
            "the outgoing section carries on from the height it is drawn at"
        );
        assert_eq!(second.opening.as_ref().unwrap().key, "b");
        assert_eq!(second.height_for("a", 20, 150), Some(mid_height));
    }

    /// Exactly one section is ever opening: the accordion's whole contract.
    #[test]
    fn only_one_section_opens_at_a_time() {
        let natural = |_: &str| 10usize;
        let a = AccordionAnim::retarget(None, None, Some("a"), natural, 0).unwrap();
        let b = AccordionAnim::retarget(Some(&a), Some("a"), Some("b"), natural, 100).unwrap();
        assert_eq!(b.opening.as_ref().unwrap().key, "b");
        assert_eq!(b.closing.as_ref().unwrap().key, "a");
        assert!(
            b.height_for("a", 10, 400).unwrap() == 0,
            "a finishes closed"
        );
        assert!(b.height_for("b", 10, 400).unwrap() == 10, "b finishes open");
    }

    /// Toggling the open section shut is a movement too — just with nothing
    /// opening.
    #[test]
    fn closing_the_open_section_animates_with_nothing_opening() {
        let natural = |_: &str| 8usize;
        let anim = AccordionAnim::retarget(None, Some("a"), None, natural, 0).unwrap();
        assert!(anim.opening.is_none());
        assert_eq!(anim.height_for("a", 8, 0), Some(8));
        assert_eq!(anim.height_for("a", 8, DURATION_MS), Some(0));
    }

    #[test]
    fn re_opening_what_is_already_open_is_not_a_movement() {
        let natural = |_: &str| 5usize;
        assert!(AccordionAnim::retarget(None, Some("a"), Some("a"), natural, 0).is_none());
        assert!(AccordionAnim::retarget(None, None, None, natural, 0).is_none());
    }

    /// A section that grows while it opens — output arriving mid-animation —
    /// opens to its new height, not the one it started with.
    #[test]
    fn an_opening_section_tracks_output_that_arrives_while_it_opens() {
        let a = anim(Some("a"), None, 0);
        assert_eq!(a.height_for("a", 10, DURATION_MS), Some(10));
        assert_eq!(a.height_for("a", 25, DURATION_MS), Some(25));
    }

    #[test]
    fn the_easing_starts_and_ends_where_it_should() {
        assert_eq!(ease_out_cubic(0.0), 0.0);
        assert_eq!(ease_out_cubic(1.0), 1.0);
        assert!(
            ease_out_cubic(0.5) > 0.5,
            "ease-out is past halfway at the midpoint"
        );
        assert_eq!(ease_out_cubic(-1.0), 0.0, "clamped");
        assert_eq!(ease_out_cubic(2.0), 1.0, "clamped");
    }
}
