//! Windowed Time-Weighted Average Price (TWAP) query.
//!
//! Builds on the [`crate::twap`] accumulator and [`crate::ring_buffer`] sample
//! history to answer the question downstream consumers actually ask:
//! *"What was the time-weighted average price of `asset` over the last
//! `window_secs` seconds?"*
//!
//! ## How it works
//!
//! Given two cumulative accumulator snapshots $(A_0, t_0)$ and $(A_1, t_1)$ with
//! $t_1 > t_0$, the exact time-weighted average price over $[t_0, t_1]$ is:
//!
//! $$\text{TWAP}(t_0, t_1) = \frac{A_1 - A_0}{t_1 - t_0}$$
//!
//! A single-block or short-lived price spike of duration $\Delta t_{\text{spike}}$
//! inside a window of length $W$ contributes at most $\frac{\Delta t_{\text{spike}}}{W}$
//! of its magnitude to the average, forcing an attacker to sustain the manipulated
//! price across many ledgers (and arbitrageurs) to move the oracle reading.
//!
//! ## History resolution & Insufficient-history behavior
//!
//! - [`Oracle::twap_between`] computes the exact TWAP between any two caller-held
//!   [`TwapObservation`] snapshots. Panics with `"window must be positive"` if
//!   `end.last_timestamp <= start.last_timestamp`.
//! - [`Oracle::twap`] computes the TWAP for `asset` over `[now - window_secs, now]`:
//!   1. If [`crate::ring_buffer`] samples exist for `asset`, it integrates the
//!      step-function price history across `[now - window_secs, now]`. If the
//!      oldest retained sample is newer than `now - window_secs`, it panics with
//!      `"window exceeds available history"`.
//!   2. Otherwise it reads the cumulative [`TwapObservation`] from [`crate::twap`],
//!      extends the accumulator to the current ledger timestamp `now` using
//!      `last_price * (now - last_timestamp)`, and divides by `window_secs`. If
//!      `window_secs > now` or only a single observation exists with elapsed time
//!      `< window_secs`, it panics with `"window exceeds available history"`.
//!
//! ## Trust assumptions
//!
//! Inherits the writer trust of [`crate::twap`] and [`crate::ring_buffer`]: the
//! admin is trusted to publish observations with honest ledger timestamps.
//! Integer division truncates toward zero by at most `1` ulp (`10^-7`).

use crate::ring_buffer::RingKey;
use crate::twap::{TwapKey, TwapObservation};
use crate::{Oracle, OracleClient, PriceData};
use soroban_sdk::{contractimpl, Address, Env, Vec};

#[contractimpl]
impl Oracle {
    /// Compute the time-weighted average price between two accumulator
    /// snapshots `start` and `end`.
    ///
    /// # Panics
    /// - `"window must be positive"` if `end.last_timestamp <= start.last_timestamp`.
    /// - `"twap accumulator overflow"` if subtracting accumulators overflows `i128`.
    pub fn twap_between(_env: Env, start: TwapObservation, end: TwapObservation) -> i128 {
        assert!(
            end.last_timestamp > start.last_timestamp,
            "window must be positive"
        );
        let elapsed = (end.last_timestamp - start.last_timestamp) as i128;
        let delta = end
            .accumulator
            .checked_sub(start.accumulator)
            .expect("twap accumulator overflow");
        delta / elapsed
    }

    /// Compute the time-weighted average price of `asset` over the last
    /// `window_secs` seconds ending at `env.ledger().timestamp()`.
    ///
    /// # Panics
    /// - `"window must be positive"` if `window_secs == 0`.
    /// - `"no twap observation for asset"` if neither ring-buffer samples nor a
    ///   TWAP observation exist for `asset`.
    /// - `"window exceeds available history"` if the requested `window_secs`
    ///   reaches earlier than the oldest recorded observation for `asset`.
    /// - `"twap accumulator overflow"` on arithmetic overflow.
    pub fn twap(env: Env, asset: Address, window_secs: u64) -> i128 {
        assert!(window_secs > 0, "window must be positive");
        let now = env.ledger().timestamp();
        assert!(now >= window_secs, "window exceeds available history");
        let window_start = now - window_secs;

        let ring: Vec<PriceData> = env
            .storage()
            .persistent()
            .get(&RingKey::Ring(asset.clone()))
            .unwrap_or_else(|| Vec::new(&env));

        if !ring.is_empty() {
            return compute_twap_from_samples(&ring, window_start, now, window_secs);
        }

        let obs: TwapObservation = env
            .storage()
            .persistent()
            .get(&TwapKey::Observation(asset))
            .expect("no twap observation for asset");

        let tail_elapsed = now.saturating_sub(obs.last_timestamp);
        if obs.accumulator == 0 {
            assert!(
                tail_elapsed >= window_secs,
                "window exceeds available history"
            );
            return obs.last_price;
        }

        let tail_increment = obs
            .last_price
            .checked_mul(tail_elapsed as i128)
            .expect("twap accumulator overflow");
        let current_accumulator = obs
            .accumulator
            .checked_add(tail_increment)
            .expect("twap accumulator overflow");

        current_accumulator / (window_secs as i128)
    }
}

fn compute_twap_from_samples(
    ring: &Vec<PriceData>,
    window_start: u64,
    now: u64,
    window_secs: u64,
) -> i128 {
    let first = ring.get(0).unwrap();
    assert!(
        first.timestamp <= window_start,
        "window exceeds available history"
    );

    let mut weighted_sum: i128 = 0;
    let len = ring.len();

    for i in 0..len {
        let current = ring.get(i).unwrap();
        let seg_start = current.timestamp.max(window_start);
        let seg_end = if i + 1 < len {
            ring.get(i + 1).unwrap().timestamp.min(now)
        } else {
            now
        };

        if seg_end > seg_start {
            let dt = (seg_end - seg_start) as i128;
            let term = current
                .price
                .checked_mul(dt)
                .expect("twap accumulator overflow");
            weighted_sum = weighted_sum
                .checked_add(term)
                .expect("twap accumulator overflow");
        }
    }

    weighted_sum / (window_secs as i128)
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::{
        testutils::{Address as _, Ledger as _},
        Env,
    };

    fn setup(env: &Env) -> (OracleClient<'_>, Address) {
        let admin = Address::generate(env);
        let id = env.register_contract(None, Oracle);
        let client = OracleClient::new(env, &id);
        client.init(&admin);
        (client, admin)
    }

    #[test]
    fn matches_hand_computed_average_over_known_series() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(0);
        let (oracle, _admin) = setup(&env);

        let xlm = Address::generate(&env);
        // [0, 10) at 1.0, [10, 30) at 2.0, [30, 40) at 4.0
        // Weighted sum = 1.0*10 + 2.0*20 + 4.0*10 = 90.0 over 40s -> 2.25 (2_2500000)
        oracle.record_twap_observation(&xlm, &1_0000000);
        env.ledger().set_timestamp(10);
        oracle.record_twap_observation(&xlm, &2_0000000);
        env.ledger().set_timestamp(30);
        oracle.record_twap_observation(&xlm, &4_0000000);
        env.ledger().set_timestamp(40);

        assert_eq!(oracle.twap(&xlm, &40), 2_2500000);
    }

    #[test]
    fn twap_between_two_observations_matches_subwindow() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(100);
        let (oracle, _admin) = setup(&env);

        let xlm = Address::generate(&env);
        oracle.record_twap_observation(&xlm, &1_0000000); // t=100

        env.ledger().set_timestamp(120);
        oracle.record_twap_observation(&xlm, &2_0000000); // t=120
        let snap_start = oracle.get_twap_observation(&xlm);

        env.ledger().set_timestamp(150);
        oracle.record_twap_observation(&xlm, &5_0000000); // t=150 (30s at 2.0)

        env.ledger().set_timestamp(170);
        oracle.record_twap_observation(&xlm, &5_0000000); // t=170 (20s at 5.0)
        let snap_end = oracle.get_twap_observation(&xlm);

        // Over [120, 170]: (30 * 2.0 + 20 * 5.0) / 50 = 160 / 50 = 3.2 (3_2000000)
        assert_eq!(oracle.twap_between(&snap_start, &snap_end), 3_2000000);
    }

    #[test]
    fn subwindow_query_over_ring_buffer_samples() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(1_000);
        let (oracle, _admin) = setup(&env);

        let xlm = Address::generate(&env);
        oracle.push_sample(&xlm, &1_0000000); // t=1000
        env.ledger().set_timestamp(1_050);
        oracle.push_sample(&xlm, &3_0000000); // t=1050
        env.ledger().set_timestamp(1_100);

        // Last 60s = [1040, 1100]: 10s at 1.0 + 50s at 3.0 = 160.0 / 60 = 2_6666666
        assert_eq!(oracle.twap(&xlm, &60), 2_6666666);
    }

    #[test]
    #[should_panic(expected = "window exceeds available history")]
    fn window_exceeding_history_panics() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(1_000);
        let (oracle, _admin) = setup(&env);

        let xlm = Address::generate(&env);
        oracle.record_twap_observation(&xlm, &1_0000000);
        env.ledger().set_timestamp(1_030);

        // Only 30s of history exist since the single observation at t=1000.
        oracle.twap(&xlm, &60);
    }

    #[test]
    #[should_panic(expected = "window must be positive")]
    fn zero_window_panics() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(1_000);
        let (oracle, _admin) = setup(&env);

        let xlm = Address::generate(&env);
        oracle.record_twap_observation(&xlm, &1_0000000);
        oracle.twap(&xlm, &0);
    }

    #[test]
    #[should_panic(expected = "no twap observation for asset")]
    fn unknown_asset_panics() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(1_000);
        let (oracle, _admin) = setup(&env);

        let unknown = Address::generate(&env);
        oracle.twap(&unknown, &10);
    }
}
