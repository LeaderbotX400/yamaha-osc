//! Bridge-driven fades: the optional second way to ramp a level.
//!
//! The default path is QLab's own 1D Fade, where QLab sends a stream of `/fader` messages and the
//! bridge just translates each one. This module serves the cases that path is bad at: many
//! channels ramping at once (one packet instead of hundreds, see the fade rate budget in
//! `docs/qlab-setup.md`) and long slow ramps where a dropped UDP step would be visible.
//!
//! The step values are computed here as a plain sequence so the shape is testable without a clock,
//! and the curve maths comes from [`yamaha_rcp::Curve`] so a bridge fade and a
//! [`yamaha_rcp::Mixer::fade`] produce identical ramps.

use std::time::Duration;

use yamaha_rcp::{Curve, MIN_DB_RAW, NEG_INF};

/// How many writes a fade of this length makes at this tick rate.
fn steps(duration: Duration, tick: Duration) -> usize {
    (duration.as_millis() / tick.as_millis().max(1)) as usize
}

/// Compute every value a fade will write, in order, ending exactly on `to`.
///
/// The result is never empty: a fade shorter than one tick, or one already at its target,
/// collapses to a single write of the target value.
pub fn plan(from: i32, to: i32, duration: Duration, tick: Duration, curve: Curve) -> Vec<i32> {
    let count = steps(duration, tick);
    if count <= 1 || from == to {
        return vec![to];
    }

    // -inf is a sentinel rather than a point on the dB scale, so a ramp towards it runs to the
    // bottom of the real scale and places the sentinel as its final step. Interpolating towards
    // -32768 directly would sweep through -327 dB and be inaudible almost immediately.
    let ramp_from = if from == NEG_INF { MIN_DB_RAW } else { from };
    let ramp_to = if to == NEG_INF { MIN_DB_RAW } else { to };
    let span = (ramp_to - ramp_from) as f32;

    let mut values = Vec::with_capacity(count);
    for step in 1..count {
        let t = curve.shape(step as f32 / count as f32);
        values.push(ramp_from + (span * t).round() as i32);
    }
    values.push(to);
    values
}

#[cfg(test)]
mod tests {
    use super::*;

    fn linear(from: i32, to: i32, ms: u64) -> Vec<i32> {
        plan(
            from,
            to,
            Duration::from_millis(ms),
            Duration::from_millis(33),
            Curve::Linear,
        )
    }

    #[test]
    fn a_fade_always_ends_exactly_on_target() {
        for (from, to, ms) in [(0, -4000, 1000), (-4000, 0, 500), (0, -1, 2000)] {
            let values = linear(from, to, ms);
            assert_eq!(*values.last().unwrap(), to, "{from} -> {to} over {ms}ms");
        }
    }

    #[test]
    fn a_fade_shorter_than_one_tick_is_a_single_jump() {
        assert_eq!(linear(0, -4000, 0), [-4000]);
        assert_eq!(linear(0, -4000, 10), [-4000]);
        assert_eq!(linear(0, -4000, 33), [-4000]);
    }

    #[test]
    fn a_fade_to_the_current_value_does_not_ramp() {
        assert_eq!(linear(-1000, -1000, 5000), [-1000]);
    }

    #[test]
    fn a_downward_fade_never_goes_back_up() {
        let values = linear(0, -4000, 1000);
        assert!(values.len() > 10, "expected a real ramp: {values:?}");
        assert!(values.windows(2).all(|w| w[1] <= w[0]), "{values:?}");
    }

    #[test]
    fn an_upward_fade_never_dips() {
        let values = linear(-4000, 0, 1000);
        assert!(values.windows(2).all(|w| w[1] >= w[0]), "{values:?}");
    }

    #[test]
    fn tick_rate_controls_the_number_of_writes() {
        let slow = plan(
            0,
            -4000,
            Duration::from_secs(1),
            Duration::from_millis(100),
            Curve::Linear,
        );
        let fast = plan(
            0,
            -4000,
            Duration::from_secs(1),
            Duration::from_millis(10),
            Curve::Linear,
        );
        assert_eq!(slow.len(), 10);
        assert_eq!(fast.len(), 100);
    }

    #[test]
    fn a_fade_to_silence_stays_on_the_real_db_scale_until_the_last_step() {
        let values = linear(0, NEG_INF, 1000);
        assert_eq!(*values.last().unwrap(), NEG_INF);
        let ramp = &values[..values.len() - 1];
        assert!(
            ramp.iter().all(|&v| (MIN_DB_RAW..=0).contains(&v)),
            "ramp left the audible scale: {values:?}"
        );
        assert!(
            ramp.iter().any(|&v| v > MIN_DB_RAW / 2),
            "the ramp should spend real time in audible territory: {values:?}"
        );
    }

    #[test]
    fn a_fade_up_from_silence_starts_at_the_bottom_of_the_scale() {
        let values = linear(NEG_INF, 0, 1000);
        assert_eq!(*values.last().unwrap(), 0);
        assert!(values[0] < MIN_DB_RAW / 2, "should start low: {values:?}");
        assert!(values.windows(2).all(|w| w[1] >= w[0]), "{values:?}");
    }

    #[test]
    fn a_cosine_fade_eases_in_and_out() {
        let values = plan(
            0,
            -1000,
            Duration::from_secs(1),
            Duration::from_millis(100),
            Curve::Cosine,
        );
        let first = values[0].abs();
        let middle = (values[5] - values[4]).abs();
        assert!(
            first < middle,
            "an eased fade should start slower than its middle: {values:?}"
        );
        assert_eq!(*values.last().unwrap(), -1000);
    }

    #[test]
    fn every_curve_covers_the_full_span() {
        for curve in [Curve::Linear, Curve::Cosine, Curve::Exponential] {
            let values = plan(
                0,
                -1000,
                Duration::from_secs(1),
                Duration::from_millis(50),
                curve,
            );
            assert_eq!(*values.last().unwrap(), -1000, "{curve:?}");
            assert!(
                values.iter().all(|&v| (-1000..=0).contains(&v)),
                "{curve:?} overshot: {values:?}"
            );
        }
    }
}
