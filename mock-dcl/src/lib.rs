//! Test-only mock of the Ref/Rhea `dclv2` concentrated-liquidity DEX.
//!
//! It honors ONLY the surface the podclub router's `seed_pool` saga actually calls — the six-leg
//! chain proven live on `dclv2.ref-dev.testnet` (commit 905bb08):
//!   register DCL on the launch FT (that FT's `storage_deposit`, handled by the real FT wasm)
//!   -> `storage_deposit` on DCL (this mock)
//!   -> `create_pool` (this mock)
//!   -> `ft_transfer_call` on the launch FT with msg `"Deposit"` -> the FT calls this mock's
//!      `ft_on_transfer`
//!   -> `add_liquidity` (this mock, NON-payable)
//!   -> the router's `on_pool_seeded` callback.
//!
//! It is deliberately NOT a faithful DCL: no real point/tick math, no swaps, no fees. Its only jobs
//! are (1) let the router's saga WIRING run to completion deterministically in a local sandbox, and
//! (2) expose `mock_set_fail_add_liquidity` so a test can force the LAST leg to trap AFTER the
//! deposit leg has already moved the tokens — pinning the Phase-6a MEDIUM-2 partial-fail path.
//! Fidelity to real DCL is covered by the live testnet proof, not here.
use near_sdk::json_types::U128;
use near_sdk::store::{IterableMap, LookupMap};
use near_sdk::{env, near, AccountId, PanicOnDefault, PromiseOrValue};

/// The plain-deposit variant of DCL's `TokenReceiverMessage`. The router sends the JSON string
/// `"Deposit"` (an empty msg fails E600 on real DCL); serde maps that unit variant to `Msg::Deposit`.
#[derive(near_sdk::serde::Deserialize)]
#[serde(crate = "near_sdk::serde")]
enum Msg {
    Deposit,
}

#[near(serializers = [json, borsh])]
#[derive(Clone)]
pub struct PoolInfo {
    pub pool_id: String,
    pub token_x: AccountId,
    pub token_y: AccountId,
    pub fee: u32,
    pub total_x: U128,
    pub total_y: U128,
}

#[near(serializers = [json, borsh])]
#[derive(Clone)]
pub struct LiquidityInfo {
    pub lpt_id: U128,
    pub owner_id: AccountId,
    pub pool_id: String,
    pub amount_x: U128,
    pub amount_y: U128,
    pub left_point: i32,
    pub right_point: i32,
}

#[near(serializers = [json])]
pub struct StorageBalance {
    pub total: U128,
    pub available: U128,
}

#[near(contract_state)]
#[derive(PanicOnDefault)]
pub struct Contract {
    pools: IterableMap<String, PoolInfo>,
    /// lpt id -> position. The router's LP position is owned by whoever called `add_liquidity`.
    liquidity: IterableMap<u128, LiquidityInfo>,
    /// internal deposited balances: account -> [(token, amount)] (what `list_user_assets` returns).
    assets: IterableMap<AccountId, Vec<(AccountId, U128)>>,
    registered: LookupMap<AccountId, bool>,
    next_lpt: u128,
    /// Test switch: when true, `add_liquidity` traps — used to pin the MEDIUM-2 partial-fail path.
    fail_add_liquidity: bool,
}

#[near]
impl Contract {
    #[init]
    pub fn new() -> Self {
        Self {
            pools: IterableMap::new(b"p"),
            liquidity: IterableMap::new(b"l"),
            assets: IterableMap::new(b"a"),
            registered: LookupMap::new(b"r"),
            next_lpt: 1,
            fail_add_liquidity: false,
        }
    }

    // ---------------- test-only control (intentionally unauthenticated: this is a fixture) --------

    /// Arm/disarm the forced `add_liquidity` failure used by the MEDIUM-2 regression test.
    pub fn mock_set_fail_add_liquidity(&mut self, fail: bool) {
        self.fail_add_liquidity = fail;
    }

    // ---------------- DCL interface the router's saga drives -------------------------------------

    /// Leg 2: register + pre-fund the caller's slot-based storage. The router MUST NOT pass
    /// `registration_only` (real DCL would then leave no asset headroom); the mock just records the
    /// account. `account_id` defaults to the predecessor (the router) exactly like real DCL.
    #[payable]
    pub fn storage_deposit(
        &mut self,
        account_id: Option<AccountId>,
        _registration_only: Option<bool>,
    ) -> StorageBalance {
        let who = account_id.unwrap_or_else(env::predecessor_account_id);
        self.registered.insert(who, true);
        StorageBalance {
            total: U128(env::attached_deposit().as_yoctonear()),
            available: U128(0),
        }
    }

    /// Leg 3: create the pool. Real DCL panics if the pool already exists — the mock mirrors that so
    /// a MEDIUM-2 retry hits the same wall. `init_point` is accepted but unused (no tick math here).
    #[payable]
    pub fn create_pool(
        &mut self,
        token_a: AccountId,
        token_b: AccountId,
        fee: u32,
        init_point: i32,
    ) -> String {
        let pool_id = format!("{}|{}|{}", token_a, token_b, fee);
        assert!(!self.pools.contains_key(&pool_id), "E404: pool already exists");
        self.pools.insert(
            pool_id.clone(),
            PoolInfo {
                pool_id: pool_id.clone(),
                token_x: token_a,
                token_y: token_b,
                fee,
                total_x: U128(0),
                total_y: U128(0),
            },
        );
        let _ = init_point;
        pool_id
    }

    /// Leg 4 receiver: the launch FT calls this after `ft_transfer_call(receiver=DCL, msg)`.
    /// predecessor == the launch FT (the deposited token). Only the JSON string `"Deposit"` credits
    /// the balance; anything else refunds in full (mirrors real DCL's E600).
    pub fn ft_on_transfer(
        &mut self,
        sender_id: AccountId,
        amount: U128,
        msg: String,
    ) -> PromiseOrValue<U128> {
        let token = env::predecessor_account_id();
        match near_sdk::serde_json::from_str::<Msg>(&msg) {
            Ok(Msg::Deposit) => {
                let mut v = self.assets.get(&sender_id).cloned().unwrap_or_default();
                match v.iter_mut().find(|(t, _)| *t == token) {
                    Some(e) => e.1 = U128(e.1 .0 + amount.0),
                    None => v.push((token, amount)),
                }
                self.assets.insert(sender_id, v);
                PromiseOrValue::Value(U128(0)) // accept all
            }
            Err(_) => PromiseOrValue::Value(amount), // refund all (E600: invalid msg)
        }
    }

    /// Leg 5: NON-payable (near-sdk rejects any attached deposit — real DCL: "doesn't accept
    /// deposit"). Mints a locked LP position to the caller (the router) and CONSUMES the caller's
    /// deposited internal balance (real DCL draws the liquidity from `list_user_assets`; an
    /// insufficient balance traps E101). Consuming it makes "zero stranded" observable after a
    /// resumed MEDIUM-2 retry. When the fail switch is armed it traps FIRST — AFTER leg 4 already
    /// moved the tokens but BEFORE they are consumed — reproducing the MEDIUM-2 partial-fail state.
    pub fn add_liquidity(
        &mut self,
        pool_id: String,
        left_point: i32,
        right_point: i32,
        amount_x: U128,
        amount_y: U128,
        min_amount_x: U128,
        min_amount_y: U128,
    ) -> U128 {
        assert!(!self.fail_add_liquidity, "MOCK: forced add_liquidity failure (MEDIUM-2)");
        let mut pool = self.pools.get(&pool_id).cloned().expect("pool not found");
        let who = env::predecessor_account_id();
        // Draw the liquidity from the caller's deposited balance (real DCL: E101 if short). A retry
        // that (wrongly) re-deposited would show a double balance here; the fix keeps it at exactly
        // the amount deposited once, so after this consume the caller has zero left — nothing stranded.
        self.debit(&who, &pool.token_x, amount_x.0);
        self.debit(&who, &pool.token_y, amount_y.0);
        pool.total_x = U128(pool.total_x.0 + amount_x.0);
        pool.total_y = U128(pool.total_y.0 + amount_y.0);
        self.pools.insert(pool_id.clone(), pool);
        let lpt = self.next_lpt;
        self.next_lpt += 1;
        self.liquidity.insert(
            lpt,
            LiquidityInfo {
                lpt_id: U128(lpt),
                owner_id: who,
                pool_id,
                amount_x,
                amount_y,
                left_point,
                right_point,
            },
        );
        let _ = (min_amount_x, min_amount_y);
        U128(lpt)
    }

    /// Subtract `amt` of `token` from `who`'s deposited internal balance (E101 if short). No-op for 0
    /// (the single-sided position deposits nothing on the wNEAR side).
    fn debit(&mut self, who: &AccountId, token: &AccountId, amt: u128) {
        if amt == 0 {
            return;
        }
        let mut v = self.assets.get(who).cloned().unwrap_or_default();
        let e = v
            .iter_mut()
            .find(|(t, _)| t == token)
            .expect("E101: insufficient internal balance");
        assert!(e.1 .0 >= amt, "E101: insufficient internal balance");
        e.1 = U128(e.1 .0 - amt);
        self.assets.insert(who.clone(), v);
    }

    // ---------------- views ----------------------------------------------------------------------

    pub fn get_pool(&self, pool_id: String) -> Option<PoolInfo> {
        self.pools.get(&pool_id).cloned()
    }

    pub fn get_liquidity(&self, lpt_id: U128) -> Option<LiquidityInfo> {
        self.liquidity.get(&lpt_id.0).cloned()
    }

    pub fn list_user_assets(&self, account_id: AccountId) -> Vec<(AccountId, U128)> {
        self.assets.get(&account_id).cloned().unwrap_or_default()
    }

    pub fn is_registered(&self, account_id: AccountId) -> bool {
        self.registered.get(&account_id).copied().unwrap_or(false)
    }
}
