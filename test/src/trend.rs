//! What the long suite measures across waves, and when a wave counts as a
//! regression. Pure, so the rule is tested here rather than at 3 a.m.

/// One wave's numbers.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Wave {
    pub n: usize,
    /// Pods the wave asked for.
    pub pods: usize,
    /// Create → every pod Ready.
    pub ramp_ms: u64,
    /// 95th percentile of create → each pod seen Ready.
    pub ready_p95_ms: u64,
    /// 95th percentile round trip, pod and Service probes together.
    pub reach_p95_ms: u64,
    /// Probes that got no answer.
    pub reach_fail: usize,
    /// Delete → nothing left (objects and CiliumEndpoints).
    pub drain_ms: u64,
    /// What was still there when the drain gave up.
    pub residue: usize,
}

impl Wave {
    /// The wave's JSON members for its result line.
    pub fn json(&self) -> String {
        format!(
            "\"wave\": {}, \"pods\": {}, \"ramp_ms\": {}, \"ready_p95_ms\": {}, \"reach_p95_ms\": {}, \"reach_fail\": {}, \"drain_ms\": {}, \"residue\": {}",
            self.n, self.pods, self.ramp_ms, self.ready_p95_ms, self.reach_p95_ms, self.reach_fail, self.drain_ms, self.residue
        )
    }
}

/// Slower than 1.5× the baseline plus 2 s to get Ready, or 2× plus 50 ms to
/// answer, is a slowdown. The absolute terms keep scheduling noise on a
/// fast first wave from reading as a regression.
pub const READY_FACTOR: (u64, u64) = (3, 2);
pub const READY_SLACK_MS: u64 = 2000;
pub const REACH_FACTOR: u64 = 2;
pub const REACH_SLACK_MS: u64 = 50;

/// The 95th percentile (nearest rank) of `v`; 0 when empty.
pub fn p95(v: &[u64]) -> u64 {
    if v.is_empty() {
        return 0;
    }
    let mut s = v.to_vec();
    s.sort_unstable();
    let rank = (s.len() * 95).div_ceil(100).max(1);
    s[rank - 1]
}

/// The first wave that regressed, and how; else a summary. Each wave is
/// compared with the first wave *of the same size* (sizes vary by design),
/// and any residue at all is a regression.
pub fn regression(waves: &[Wave]) -> Result<String, (usize, String)> {
    for (i, w) in waves.iter().enumerate() {
        if w.residue > 0 {
            return Err((w.n, format!("wave {} left {} objects/endpoints behind after draining", w.n, w.residue)));
        }
        let Some(base) = waves[..i].iter().find(|b| b.pods == w.pods) else { continue };
        let ready_limit = base.ready_p95_ms * READY_FACTOR.0 / READY_FACTOR.1 + READY_SLACK_MS;
        if w.ready_p95_ms > ready_limit {
            return Err((w.n, format!("wave {} ({} pods) got Ready in p95 {} ms; wave {} took {} ms (limit {ready_limit})", w.n, w.pods, w.ready_p95_ms, base.n, base.ready_p95_ms)));
        }
        let reach_limit = base.reach_p95_ms * REACH_FACTOR + REACH_SLACK_MS;
        if w.reach_p95_ms > reach_limit {
            return Err((w.n, format!("wave {} ({} pods) answered in p95 {} ms; wave {} took {} ms (limit {reach_limit})", w.n, w.pods, w.reach_p95_ms, base.n, base.reach_p95_ms)));
        }
    }
    let pods: usize = waves.iter().map(|w| w.pods).sum();
    Ok(format!("{} waves, {pods} pods in all: no slowdown against the first wave of each size, nothing left behind", waves.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(n: usize, pods: usize, ready: u64, reach: u64) -> Wave {
        Wave { n, pods, ready_p95_ms: ready, reach_p95_ms: reach, ..Default::default() }
    }

    #[test]
    fn p95_is_nearest_rank() {
        assert_eq!(p95(&[]), 0);
        assert_eq!(p95(&[7]), 7);
        let v: Vec<u64> = (1..=100).collect();
        assert_eq!(p95(&v), 95);
        assert_eq!(p95(&[5, 1, 3]), 5);
    }

    #[test]
    fn steady_waves_pass() {
        let waves = [w(1, 100, 20_000, 3), w(2, 50, 9_000, 2), w(3, 100, 25_000, 5), w(4, 50, 11_000, 4)];
        assert!(regression(&waves).is_ok());
    }

    #[test]
    fn a_slowdown_names_the_first_wave_against_its_own_size() {
        // Wave 2 is smaller, so it is not compared with wave 1; wave 3 is
        // wave 1's size and far slower.
        let waves = [w(1, 100, 20_000, 3), w(2, 50, 90_000, 2), w(3, 100, 40_000, 3), w(4, 100, 60_000, 3)];
        let (n, why) = regression(&waves).unwrap_err();
        assert_eq!(n, 3, "{why}");
        let waves = [w(1, 10, 1000, 2), w(2, 10, 1000, 60)];
        assert_eq!(regression(&waves).unwrap_err().0, 2);
    }

    #[test]
    fn noise_on_a_fast_first_wave_is_not_a_regression() {
        let waves = [w(1, 10, 500, 1), w(2, 10, 2400, 40)];
        assert!(regression(&waves).is_ok());
    }

    #[test]
    fn residue_fails_the_wave_that_left_it() {
        let mut waves = vec![w(1, 10, 1000, 2), w(2, 10, 1000, 2)];
        waves[1].residue = 3;
        assert_eq!(regression(&waves).unwrap_err().0, 2);
    }
}
