//! Integration test suite demonstrating price-manipulation scenarios and how
//! the kit's defenses (TWAP accumulation, multi-source median aggregation, and
//! deviation circuit breakers) dampen or reject them compared to raw spot reads.

use oracle::{
    adapters::{amm_spot::AmmSpotSource, source::PriceSource},
    Oracle, OracleClient,
};
use soroban_sdk::{
    testutils::{Address as _, Ledger as _},
    vec, Address, Env,
};

const ONE: i128 = 1_0000000; // 1.0000000 (7 decimals)
const BPS_DENOMINATOR: i128 = 10_000;

fn setup(env: &Env) -> (OracleClient<'_>, Address) {
    let admin = Address::generate(env);
    let contract_id = env.register_contract(None, Oracle);
    let client = OracleClient::new(env, &contract_id);
    client.init(&admin);
    (client, admin)
}

/// Computes the relative deviation in basis points: `|actual - baseline| * 10_000 / baseline`.
fn deviation_bps(baseline: i128, actual: i128) -> i128 {
    let diff = (actual - baseline).abs();
    (diff * BPS_DENOMINATOR) / baseline
}

#[test]
fn single_ledger_flash_spike_fully_moves_spot_but_barely_moves_twap() {
    let env = Env::default();
    env.mock_all_auths();

    let (oracle, _admin) = setup(&env);
    let asset = Address::generate(&env);

    // 1. Establish a steady 1.00 baseline at t = 1_000.
    let window_start: u64 = 1_000;
    env.ledger().set_timestamp(window_start);
    oracle.set_price(&asset, &ONE);
    oracle.record_twap_observation(&asset, &ONE);
    let start_obs = oracle.get_twap_observation(&asset);

    // Steady baseline holds for 3,595 seconds (nearly 1 hour).
    let spike_time: u64 = window_start + 3_595;
    env.ledger().set_timestamp(spike_time);

    // 2. Attacker executes a 10x flash-loan spike (10.00) for a single 5-second ledger.
    let spike_price: i128 = 10 * ONE;
    oracle.set_price(&asset, &spike_price);
    oracle.record_twap_observation(&asset, &spike_price);

    // At the moment of the attack, the raw spot price is 100% manipulated (+900% / 90,000 bps).
    let spot_during_spike = oracle.get_price(&asset);
    assert_eq!(spot_during_spike, spike_price);
    assert_eq!(deviation_bps(ONE, spot_during_spike), 90_000);

    // 3. Arbitrageurs restore the pool price to 1.00 in the very next ledger (5 seconds later).
    let window_end: u64 = spike_time + 5;
    env.ledger().set_timestamp(window_end);
    oracle.set_price(&asset, &ONE);
    oracle.record_twap_observation(&asset, &ONE);
    let end_obs = oracle.get_twap_observation(&asset);

    // Compute the 1-hour (3,600s) windowed TWAP from the accumulator snapshots:
    // (3,595s * 1.0 + 5s * 10.0) / 3,600s = 3,645 / 3,600 = 1.0125 (1_0125000).
    let elapsed = (end_obs.last_timestamp - start_obs.last_timestamp) as i128;
    assert_eq!(elapsed, 3_600);
    let twap = (end_obs.accumulator - start_obs.accumulator) / elapsed;
    assert_eq!(twap, 1_0125000);

    // Whereas spot moved by 90,000 bps (900%), the 1-hour TWAP moved by only 125 bps (1.25%),
    // staying well within a 200 bps (2%) collateral safety tolerance.
    let twap_dev_bps = deviation_bps(ONE, twap);
    assert_eq!(twap_dev_bps, 125);
    assert!(twap_dev_bps <= 200);
}

#[test]
fn amm_spot_source_reflects_instant_reserve_manipulation_unlike_twap() {
    let env = Env::default();
    env.mock_all_auths();

    let (oracle, _admin) = setup(&env);
    let asset = Address::generate(&env);

    // Constant-product pool starts balanced: 1,000,000 base : 1,000,000 quote (spot = 1.00).
    let honest_pool = AmmSpotSource::new(1_000_000, 1_000_000);
    let honest_spot = honest_pool.price(&env, &asset);
    assert_eq!(honest_spot, ONE);

    env.ledger().set_timestamp(10_000);
    oracle.record_twap_observation(&asset, &honest_spot);
    let obs_start = oracle.get_twap_observation(&asset);

    // Honest reserves prevail for 599 seconds.
    env.ledger().set_timestamp(10_599);

    // Flash-loan attacker dumps base token or drains quote token so reserves become
    // 200,000 base : 5,000,000 quote -> instantaneous AMM spot jumps 25x to 25.00.
    let manipulated_pool = AmmSpotSource::new(200_000, 5_000_000);
    let manipulated_spot = manipulated_pool.price(&env, &asset);
    assert_eq!(manipulated_spot, 25 * ONE);
    oracle.record_twap_observation(&asset, &manipulated_spot);

    // 1 second later (t = 10_600), the flash-loan unwinds back to honest reserves.
    env.ledger().set_timestamp(10_600);
    oracle.record_twap_observation(&asset, &honest_spot);
    let obs_end = oracle.get_twap_observation(&asset);

    let elapsed = (obs_end.last_timestamp - obs_start.last_timestamp) as i128;
    assert_eq!(elapsed, 600);
    let twap = (obs_end.accumulator - obs_start.accumulator) / elapsed;

    // Spot jumped +2,400% (240,000 bps), while the 10-minute TWAP moved by only +4% (400 bps).
    assert_eq!(deviation_bps(ONE, manipulated_spot), 240_000);
    assert_eq!(twap, 1_0400000);
    assert_eq!(deviation_bps(ONE, twap), 400);
}

#[test]
fn sustained_manipulation_moves_twap_proportionally_to_duration() {
    let env = Env::default();
    env.mock_all_auths();

    let (oracle, _admin) = setup(&env);
    let asset = Address::generate(&env);

    // Window [0, 1_000]. Compare a 10-second spike (1% of window) against a
    // 500-second sustained manipulation (50% of window) at 3x baseline price.
    env.ledger().set_timestamp(1_000);
    oracle.record_twap_observation(&asset, &ONE);
    let base_obs = oracle.get_twap_observation(&asset);

    // Brief 10s spike at the end of a 1,000s window:
    env.ledger().set_timestamp(1_990);
    oracle.record_twap_observation(&asset, &(3 * ONE));
    env.ledger().set_timestamp(2_000);
    oracle.record_twap_observation(&asset, &ONE);
    let brief_end = oracle.get_twap_observation(&asset);
    let brief_twap = (brief_end.accumulator - base_obs.accumulator) / 1_000;
    // (990 * 1.0 + 10 * 3.0) / 1000 = 1.02
    assert_eq!(brief_twap, 1_0200000);

    // Sustained 500s spike across half of the next 1,000s window [2_000, 3_000]:
    env.ledger().set_timestamp(2_500);
    oracle.record_twap_observation(&asset, &(3 * ONE));
    env.ledger().set_timestamp(3_000);
    oracle.record_twap_observation(&asset, &ONE);
    let sustained_end = oracle.get_twap_observation(&asset);
    let sustained_twap = (sustained_end.accumulator - brief_end.accumulator) / 1_000;
    // (500 * 1.0 + 500 * 3.0) / 1000 = 2.00
    assert_eq!(sustained_twap, 2_0000000);

    // Brief manipulation barely moves TWAP (200 bps), whereas moving it by 10,000 bps
    // requires holding the distorted price across 50% of all ledgers in the window.
    assert!(deviation_bps(ONE, brief_twap) < deviation_bps(ONE, sustained_twap));
}

#[test]
fn median_aggregation_ignores_minority_corrupted_feeds() {
    let env = Env::default();
    env.mock_all_auths();

    let (oracle, _admin) = setup(&env);
    let asset = Address::generate(&env);

    // 5 independent price reporters: 3 honest around $1.00, 2 compromised reporting $1,000.00.
    let submissions = vec![
        &env,
        ONE,         // honest (1.0000000)
        1_0010000,   // honest (1.0010000)
        9990000,     // honest (0.9990000)
        1_000 * ONE, // 1,000x outlier spike
        500 * ONE,   // 500x outlier spike
    ];

    let published = oracle.set_price_median(&asset, &submissions);
    // Sorted: [0.999, 1.000, 1.001, 500.0, 1000.0] -> median is 1.001 (10 bps from 1.00).
    assert_eq!(published, 1_0010000);
    assert_eq!(deviation_bps(ONE, published), 10);
}

#[test]
fn deviation_breaker_rejects_flash_spike_and_preserves_honest_price() {
    let env = Env::default();
    env.mock_all_auths();

    let (oracle, _admin) = setup(&env);
    let asset = Address::generate(&env);

    // Seed baseline price of 1.00 with a 500 bps (5%) circuit breaker.
    let max_dev_bps: u32 = 500;
    oracle.set_price_guarded(&asset, &ONE, &max_dev_bps);
    assert_eq!(oracle.get_price(&asset), ONE);

    // Flash manipulation attempting to push price to 5.00 (+400%) trips the breaker.
    let rejected = oracle.try_set_price_guarded(&asset, &(5 * ONE), &max_dev_bps);
    assert!(rejected.is_err());

    // Stored spot price remains untouched at the honest 1.00 baseline.
    assert_eq!(oracle.get_price(&asset), ONE);
}
