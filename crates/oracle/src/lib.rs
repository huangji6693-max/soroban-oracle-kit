#![no_std]
//! # oracle
//!
//! A minimal, admin-curated price-feed oracle contract for Soroban.
//!
//! The contract stores the latest price for each asset alongside the ledger
//! timestamp at which it was written, so consumers can both read a price and
//! reason about its freshness. It is the foundation the rest of the kit builds
//! on — TWAP accumulation, staleness guards, multi-source median aggregation,
//! and pool adapters are tracked as individual seed issues so each lands in its
//! own file without merge conflicts.
//!
//! ## Pricing convention
//!
//! Prices are stored as `i128` in fixed-point with `DECIMALS` (7) implied
//! decimal places — matching Stellar's native asset precision. A price of
//! `1_0000000` therefore means `1.0` units of the quote asset per unit of the
//! priced asset.

use soroban_sdk::{contract, contractimpl, contracttype, Address, Env, Vec};

/// Implied decimal places for every stored price.
pub const DECIMALS: u32 = 7;

#[derive(Clone, Debug, Eq, PartialEq)]
#[contracttype]
pub struct PriceData {
    /// Fixed-point price with `DECIMALS` implied decimals.
    pub price: i128,
    /// Ledger timestamp (seconds) at which the price was written.
    pub timestamp: u64,
}

#[derive(Clone)]
#[contracttype]
pub enum DataKey {
    Admin,
    /// Set of assets that have ever been priced.
    Assets,
    Price(Address),
}

#[contract]
pub struct Oracle;

#[contractimpl]
impl Oracle {
    /// One-time setup. `admin` is the only address allowed to publish prices.
    pub fn init(env: Env, admin: Address) {
        if env.storage().instance().has(&DataKey::Admin) {
            panic!("already initialized");
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage()
            .instance()
            .set(&DataKey::Assets, &Vec::<Address>::new(&env));
    }

    /// Publish the latest `price` for `asset`. Admin-only.
    ///
    /// The current ledger timestamp is recorded with the price.
    pub fn set_price(env: Env, asset: Address, price: i128) {
        assert!(price >= 0, "price must be non-negative");
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .expect("not initialized");
        admin.require_auth();

        if !Self::has_price(env.clone(), asset.clone()) {
            let mut assets: Vec<Address> = env
                .storage()
                .instance()
                .get(&DataKey::Assets)
                .unwrap_or_else(|| Vec::new(&env));
            assets.push_back(asset.clone());
            env.storage().instance().set(&DataKey::Assets, &assets);
        }

        let data = PriceData {
            price,
            timestamp: env.ledger().timestamp(),
        };
        env.storage()
            .persistent()
            .set(&DataKey::Price(asset), &data);
    }

    /// Read the latest price for `asset`. Panics if no price has been set.
    pub fn get_price(env: Env, asset: Address) -> i128 {
        Self::get_price_data(env, asset).price
    }

    /// Read the latest price *and* its timestamp for `asset`.
    pub fn get_price_data(env: Env, asset: Address) -> PriceData {
        env.storage()
            .persistent()
            .get(&DataKey::Price(asset))
            .expect("no price for asset")
    }

    /// Whether a price has ever been published for `asset`.
    pub fn has_price(env: Env, asset: Address) -> bool {
        env.storage().persistent().has(&DataKey::Price(asset))
    }

    /// The list of all assets that have ever been priced.
    pub fn assets(env: Env) -> Vec<Address> {
        env.storage()
            .instance()
            .get(&DataKey::Assets)
            .unwrap_or_else(|| Vec::new(&env))
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::{testutils::Address as _, Address, Env};

    fn setup(env: &Env) -> (OracleClient<'_>, Address) {
        let admin = Address::generate(env);
        let id = env.register_contract(None, Oracle);
        let client = OracleClient::new(env, &id);
        client.init(&admin);
        (client, admin)
    }

    #[test]
    fn set_and_get_price() {
        let env = Env::default();
        env.mock_all_auths();
        let (oracle, _admin) = setup(&env);

        let xlm = Address::generate(&env);
        oracle.set_price(&xlm, &1_0000000);
        assert_eq!(oracle.get_price(&xlm), 1_0000000);
        assert!(oracle.has_price(&xlm));
    }

    #[test]
    fn records_timestamp() {
        let env = Env::default();
        env.mock_all_auths();
        let (oracle, _admin) = setup(&env);

        let usdc = Address::generate(&env);
        oracle.set_price(&usdc, &1_0000000);
        let data = oracle.get_price_data(&usdc);
        assert_eq!(data.price, 1_0000000);
        assert_eq!(data.timestamp, env.ledger().timestamp());
    }

    #[test]
    fn tracks_known_assets_without_duplicates() {
        let env = Env::default();
        env.mock_all_auths();
        let (oracle, _admin) = setup(&env);

        let a = Address::generate(&env);
        let b = Address::generate(&env);
        oracle.set_price(&a, &10);
        oracle.set_price(&b, &20);
        oracle.set_price(&a, &11); // update, not a new asset

        assert_eq!(oracle.assets().len(), 2);
    }

    #[test]
    fn unknown_asset_has_no_price() {
        let env = Env::default();
        env.mock_all_auths();
        let (oracle, _admin) = setup(&env);
        let unknown = Address::generate(&env);
        assert!(!oracle.has_price(&unknown));
    }

    #[test]
    #[should_panic(expected = "no price for asset")]
    fn get_price_panics_when_unset() {
        let env = Env::default();
        env.mock_all_auths();
        let (oracle, _admin) = setup(&env);
        let unknown = Address::generate(&env);
        oracle.get_price(&unknown);
    }
}
pub mod adapters;
pub mod admin_transfer;
pub mod age;
pub mod asset_config;
pub mod asset_count;
pub mod batch_set;
pub mod decimals;
pub mod deviation_breaker;
pub mod heartbeat;
pub mod inverse;
pub mod median;
pub mod nonzero;
pub mod pause;
pub mod ring_buffer;
pub mod roles;
pub mod sanity_bounds;
pub mod staleness;
pub mod try_get;
pub mod twap;
pub mod twap_query;
