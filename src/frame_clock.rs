use std::{num::NonZeroU32, time::Duration};

/// Next boundary on a fixed epoch's rational frame grid. Millihertz retains
/// fractional display rates; rounding up prevents early re-triggering.
/// Missed boundaries are skipped in constant time without catch-up bursts.
pub(crate) fn next_frame_delay(elapsed: Duration, millihertz: NonZeroU32) -> Duration {
    let rate = u128::from(millihertz.get());
    let phase = (elapsed.as_nanos() * rate) % 1_000_000_000_000;
    Duration::from_nanos((1_000_000_000_000 - phase).div_ceil(rate) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn work_and_fractional_periods_do_not_accumulate_drift() {
        for rate in [30_000, 59_940, 75_437, 144_000, 500_000] {
            let rate = NonZeroU32::new(rate).unwrap();
            let mut deadline = Duration::ZERO;
            for frame in 1..=10_000_u128 {
                let completed = deadline + Duration::from_micros(500);
                deadline = completed + next_frame_delay(completed, rate);
                assert_eq!(
                    deadline.as_nanos(),
                    (frame * 1_000_000_000_000).div_ceil(u128::from(rate.get()))
                );
            }
        }
    }

    #[test]
    fn missed_frames_always_skip_to_a_future_boundary() {
        for rate in [1, 59_940, 144_000, u32::MAX] {
            let rate = NonZeroU32::new(rate).unwrap();
            for elapsed in [
                Duration::ZERO,
                Duration::from_millis(534),
                Duration::from_secs(60 * 60 * 24 * 365),
                Duration::new(u64::MAX, 999_999_999),
            ] {
                let delay = next_frame_delay(elapsed, rate);
                assert!(!delay.is_zero());
                assert!(
                    delay.as_nanos() <= 1_000_000_000_000_u128.div_ceil(u128::from(rate.get()))
                );
            }
        }
        let elapsed = Duration::from_millis(534);
        let rate = NonZeroU32::new(144_000).unwrap();
        assert_eq!(
            elapsed + next_frame_delay(elapsed, rate),
            Duration::from_nanos(534_722_223)
        );
    }

    #[test]
    fn refresh_changes_use_the_new_grid_immediately() {
        let elapsed = Duration::from_millis(5);
        let slow = NonZeroU32::new(60_000).unwrap();
        let fast = NonZeroU32::new(144_000).unwrap();
        assert_eq!(
            elapsed + next_frame_delay(elapsed, slow),
            Duration::from_nanos(16_666_667)
        );
        assert_eq!(
            elapsed + next_frame_delay(elapsed, fast),
            Duration::from_nanos(6_944_445)
        );
    }
}
