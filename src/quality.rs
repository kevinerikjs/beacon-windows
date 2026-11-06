//! `auto` quality: the phone reports how well playback goes (`quality_feedback`, 0…1) and the
//! host moves along a ladder of presets. A port of PhorosSession.QualityLadder, same policy.

use std::time::{Duration, Instant};

pub const TIERS: [&str; 4] = ["360p30", "480p30", "720p30", "1080p30"];
const STEP_DOWN_BELOW: f64 = 0.45;
const STEP_UP_ABOVE: f64 = 0.80;
const SUSTAINED_LOW: Duration = Duration::from_secs(3);
const SUSTAINED_HIGH: Duration = Duration::from_secs(12);
const MINIMUM_INTERVAL: Duration = Duration::from_secs(6);

pub struct Ladder {
    index: usize,
    latest: f64,
    low_since: Option<Instant>,
    high_since: Option<Instant>,
    last_change: Option<Instant>,
}

impl Default for Ladder {
    fn default() -> Self {
        Ladder { index: TIERS.len() - 1, latest: 1.0, low_since: None, high_since: None, last_change: None }
    }
}

impl Ladder {
    pub fn current(&self) -> &'static str {
        TIERS[self.index]
    }

    pub fn feedback(&mut self, quality: f64) {
        self.latest = quality;
    }

    /// The preset to move to now, if any. Call every couple of seconds.
    pub fn evaluate(&mut self, now: Instant) -> Option<&'static str> {
        let may_change = self.last_change.is_none_or(|t| now - t >= MINIMUM_INTERVAL);
        if self.latest < STEP_DOWN_BELOW {
            self.high_since = None;
            let since = *self.low_since.get_or_insert(now);
            if may_change && now - since >= SUSTAINED_LOW && self.index > 0 {
                return Some(self.change(self.index - 1, now));
            }
        } else if self.latest > STEP_UP_ABOVE {
            self.low_since = None;
            let since = *self.high_since.get_or_insert(now);
            if may_change && now - since >= SUSTAINED_HIGH && self.index < TIERS.len() - 1 {
                return Some(self.change(self.index + 1, now));
            }
        } else {
            self.low_since = None;
            self.high_since = None;
        }
        None
    }

    pub fn set(&mut self, preset: &str, now: Instant) {
        if let Some(i) = TIERS.iter().position(|t| *t == preset) {
            self.change(i, now);
        }
    }

    fn change(&mut self, index: usize, now: Instant) -> &'static str {
        self.index = index;
        self.last_change = Some(now);
        self.low_since = None;
        self.high_since = None;
        TIERS[index]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_down_after_sustained_low_and_up_after_sustained_high() {
        let t0 = Instant::now();
        let s = |n| t0 + Duration::from_secs(n);
        let mut l = Ladder::default();
        l.feedback(0.2);
        assert_eq!(l.evaluate(s(0)), None);
        assert_eq!(l.evaluate(s(2)), None);
        assert_eq!(l.evaluate(s(3)), Some("720p30"));
        assert_eq!(l.evaluate(s(7)), None, "six seconds between changes");
        assert_eq!(l.evaluate(s(9)), None, "low again only since 7");
        assert_eq!(l.evaluate(s(10)), Some("480p30"));
        l.feedback(0.9);
        assert_eq!(l.evaluate(s(11)), None);
        assert_eq!(l.evaluate(s(22)), None);
        assert_eq!(l.evaluate(s(23)), Some("720p30"));
        l.feedback(0.6);
        assert_eq!(l.evaluate(s(40)), None, "the middle band holds");
    }
}
