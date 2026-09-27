//! FastLaunch launch token — a NEP-141 fungible token that is ALSO reward-bearing (B1): holders
//! earn the launch's `holder` fee bucket in wNEAR simply by HOLDING the token on-chain, with NO
//! stake and NO lock. A MasterChef-style reward-per-share accumulator is baked into the FT and
//! settled on every balance change, so a plain wallet balance accrues rewards with zero action.
//!
//!   * Fixed supply, minted ONCE to `owner_id` (the router) at init. No mint/burn method exists.
//!   * Standard NEP-141 core + NEP-145 storage + NEP-148 metadata, delegating the token bookkeeping
//!     to `near-contract-standards`' audited `FungibleToken`, WRAPPED with reward accounting.
//!   * Rewards arrive as wNEAR via `ft_transfer_call(this, amount, "reward")` (the router forwards
//!     the accrued holder bucket; external donations via the same path are harmless — they only
//!     benefit holders). Each holder claims their pro-rata share with `claim_rewards`.
//!
//! Reward accounting (all amounts wNEAR yocto):
//!   * `acc_reward_per_share` — cumulative reward per eligible token, scaled by `PREC` (1e24).
//!   * per-account `reward_debt` + `unclaimed`.
//!   * `eligible_supply` — Σ of NON-excluded balances (the router's unsold reserve + buyback-locked
//!     tokens and the DCL pool are excluded so their large balances never dilute real holders).
//!   * `reward_remainder` — integer-division dust carried forward; also holds rewards that arrive
//!     while `eligible_supply == 0`, folded in at the next distribute once `eligible_supply > 0`.
//!   * `settle(a)`: pending = bal(a)*acc/PREC − debt(a); unclaimed(a) += pending; debt = bal*acc/PREC.
//!     Run for BOTH parties BEFORE every balance move; debts + `eligible_supply` re-synced AFTER.
use near_contract_standards::fungible_token::core::FungibleTokenCore;
use near_contract_standards::fungible_token::metadata::{
    FungibleTokenMetadata, FungibleTokenMetadataProvider,
};
use near_contract_standards::fungible_token::resolver::FungibleTokenResolver;
use near_contract_standards::fungible_token::FungibleToken;
use near_contract_standards::storage_management::{
    StorageBalance, StorageBalanceBounds, StorageManagement,
};
use near_sdk::json_types::U128;
use near_sdk::serde_json::json;
use near_sdk::store::{LookupMap, LookupSet};
use near_sdk::{
    env, near, require, AccountId, BorshStorageKey, Gas, NearToken, PanicOnDefault, Promise,
    PromiseError, PromiseOrValue,
};
use primitive_types::U256;

/// Reward-per-share precision (1e24), matching the router's ACC_PRECISION for parity.
const PREC: u128 = 1_000_000_000_000_000_000_000_000;
/// wNEAR account id — reward custody + payout token. FLAG: this is `wrap.near` on mainnet; the
/// by-hash global-FT id changes with this constant, so a mainnet build republishes the global FT
/// and the factory must be pointed at the new hash (BRIEF gate 6).
const WNEAR: &str = "wrap.testnet";
const ONE_YOCTO: NearToken = NearToken::from_yoctonear(1);
/// Gas for the wNEAR payout leg of a claim + its unwind callback.
const GAS_FOR_WNEAR_TRANSFER: Gas = Gas::from_tgas(15);
const GAS_FOR_CLAIM_CB: Gas = Gas::from_tgas(10);

/// `a * b / c` computed in U256 so the `balance (≤1e33) * acc (scaled 1e24)` product never
/// overflows u128 (BRIEF gate 5). Mirrors the router's `curve::mul_div`.
fn mul_div(a: u128, b: u128, c: u128) -> u128 {
    debug_assert!(c != 0, "mul_div by zero");
    (U256::from(a) * U256::from(b) / U256::from(c)).as_u128()
}

fn emit(event: &str, data: near_sdk::serde_json::Value) {
    env::log_str(
        &json!({
            "standard": "fastlaunch_ft", "version": "1.0.0", "event": event, "data": data
        })
        .to_string(),
    );
}

#[near(serializers = [borsh])]
#[derive(BorshStorageKey)]
enum StorageKey {
    Token,
    RewardDebt,
    Unclaimed,
    Excluded,
}

#[near(contract_state)]
#[derive(PanicOnDefault)]
pub struct Contract {
    token: FungibleToken,
    metadata: FungibleTokenMetadata,
    /// The router (== `owner_id` at init, the mint target). Only the authority may mutate `excluded`.
    authority: AccountId,
    /// Cumulative wNEAR reward per eligible token, scaled by `PREC`.
    acc_reward_per_share: u128,
    /// Σ of all NON-excluded balances (the reward denominator). NOT `ft_total_supply`.
    eligible_supply: u128,
    /// Integer-division dust + rewards received while `eligible_supply == 0`, carried forward.
    reward_remainder: u128,
    reward_debt: LookupMap<AccountId, u128>,
    unclaimed: LookupMap<AccountId, u128>,
    /// Accounts that never accrue (router reserve/burn, DCL pool). Their balances are excluded
    /// from `eligible_supply`. Never iterated on any hot path (BRIEF gate 4).
    excluded: LookupSet<AccountId>,
}

#[near]
impl Contract {
    /// Deploy a launch token. Excludes `owner_id` (the router — it custodies the unsold reserve and
    /// buyback-locked supply, never a holder) BEFORE the mint, so its balance never enters
    /// `eligible_supply`. Registers `owner_id`, mints the entire `total_supply` to it; no further
    /// minting is ever possible. `metadata` must be a valid NEP-148 payload.
    #[init]
    pub fn new(owner_id: AccountId, total_supply: U128, metadata: FungibleTokenMetadata) -> Self {
        require!(!env::state_exists(), "already initialized");
        metadata.assert_valid();
        require!(total_supply.0 > 0, "total supply must be positive");
        let mut token = FungibleToken::new(StorageKey::Token);
        let mut excluded = LookupSet::new(StorageKey::Excluded);
        excluded.insert(owner_id.clone());
        token.internal_register_account(&owner_id);
        token.internal_deposit(&owner_id, total_supply.0);
        near_contract_standards::fungible_token::events::FtMint {
            owner_id: &owner_id,
            amount: total_supply,
            memo: Some("launch mint"),
        }
        .emit();
        Self {
            token,
            metadata,
            authority: owner_id,
            acc_reward_per_share: 0,
            eligible_supply: 0,
            reward_remainder: 0,
            reward_debt: LookupMap::new(StorageKey::RewardDebt),
            unclaimed: LookupMap::new(StorageKey::Unclaimed),
            excluded,
        }
    }

    // ---- reward accounting internals ----
    fn balance_of(&self, a: &AccountId) -> u128 {
        self.token.ft_balance_of(a.clone()).0
    }

    /// Bank an account's pending rewards into `unclaimed` at the CURRENT accumulator, using its
    /// CURRENT balance, then snapshot its debt. Excluded accounts never accrue (no-op).
    fn settle(&mut self, a: &AccountId) {
        if self.excluded.contains(a) {
            return;
        }
        let entitled = mul_div(self.balance_of(a), self.acc_reward_per_share, PREC);
        let debt = *self.reward_debt.get(a).unwrap_or(&0);
        let pending = entitled.saturating_sub(debt);
        if pending > 0 {
            let u = *self.unclaimed.get(a).unwrap_or(&0);
            self.unclaimed.insert(a.clone(), u + pending);
        }
        self.reward_debt.insert(a.clone(), entitled);
    }

    /// After a balance move, re-sync `eligible_supply` by the signed balance delta and re-snapshot
    /// the debt from the NEW balance. Must be preceded by `settle(a)`. No-op for excluded accounts.
    fn sync_after_move(&mut self, a: &AccountId, old_bal: u128) {
        if self.excluded.contains(a) {
            return;
        }
        let new_bal = self.balance_of(a);
        if new_bal >= old_bal {
            self.eligible_supply += new_bal - old_bal;
        } else {
            self.eligible_supply -= old_bal - new_bal;
        }
        self.reward_debt
            .insert(a.clone(), mul_div(new_bal, self.acc_reward_per_share, PREC));
    }
}

// ---- NEP-141 core (delegated, wrapped with settle-on-transfer) ----
#[near]
impl FungibleTokenCore for Contract {
    #[payable]
    fn ft_transfer(&mut self, receiver_id: AccountId, amount: U128, memo: Option<String>) {
        let sender = env::predecessor_account_id();
        self.settle(&sender);
        self.settle(&receiver_id);
        let (os, orr) = (self.balance_of(&sender), self.balance_of(&receiver_id));
        self.token.ft_transfer(receiver_id.clone(), amount, memo);
        self.sync_after_move(&sender, os);
        self.sync_after_move(&receiver_id, orr);
    }

    #[payable]
    fn ft_transfer_call(
        &mut self,
        receiver_id: AccountId,
        amount: U128,
        memo: Option<String>,
        msg: String,
    ) -> PromiseOrValue<U128> {
        let sender = env::predecessor_account_id();
        self.settle(&sender);
        self.settle(&receiver_id);
        // The delegate debits sender + credits receiver SYNCHRONOUSLY (then schedules the receiver's
        // ft_on_transfer + our ft_resolve_transfer), so balances already reflect the move here. Any
        // refund lands in ft_resolve_transfer, which settles + re-syncs again.
        let (os, orr) = (self.balance_of(&sender), self.balance_of(&receiver_id));
        let ret = self.token.ft_transfer_call(receiver_id.clone(), amount, memo, msg);
        self.sync_after_move(&sender, os);
        self.sync_after_move(&receiver_id, orr);
        ret
    }

    fn ft_total_supply(&self) -> U128 {
        self.token.ft_total_supply()
    }

    fn ft_balance_of(&self, account_id: AccountId) -> U128 {
        self.token.ft_balance_of(account_id)
    }
}

#[near]
impl FungibleTokenResolver for Contract {
    /// The refund path of `ft_transfer_call` moves balance back to the sender — MUST settle both
    /// parties here too (BRIEF Part A) so a refunded transfer accrues correctly.
    #[private]
    fn ft_resolve_transfer(
        &mut self,
        sender_id: AccountId,
        receiver_id: AccountId,
        amount: U128,
    ) -> U128 {
        self.settle(&sender_id);
        self.settle(&receiver_id);
        let (os, orr) = (self.balance_of(&sender_id), self.balance_of(&receiver_id));
        let (used, _burned) =
            self.token
                .internal_ft_resolve_transfer(&sender_id, receiver_id.clone(), amount);
        self.sync_after_move(&sender_id, os);
        self.sync_after_move(&receiver_id, orr);
        used.into()
    }
}

// ---- reward intake, claim, views ----
#[near]
impl Contract {
    /// Reward intake — wNEAR arrives via `ft_transfer_call(this, amount, "reward")`. Bumps the
    /// accumulator by `amount / eligible_supply` (scaled by PREC), carrying the integer-division
    /// remainder so no wNEAR leaks. If there are no eligible holders yet, the whole amount is parked
    /// in `reward_remainder` and folded in at the next reward once `eligible_supply > 0`. Returns 0
    /// = fully accepted (the wNEAR stays custodied by this FT).
    pub fn ft_on_transfer(
        &mut self,
        sender_id: AccountId,
        amount: U128,
        msg: String,
    ) -> PromiseOrValue<U128> {
        require!(
            env::predecessor_account_id().as_str() == WNEAR,
            "only wNEAR is accepted as reward"
        );
        require!(msg == "reward", "unknown reward msg");
        let _ = sender_id; // donations from any sender are harmless (only benefit holders)
        let total = amount.0 + self.reward_remainder;
        if self.eligible_supply == 0 {
            self.reward_remainder = total;
            emit("reward_parked", json!({ "amount": amount, "carried": U128(total) }));
            return PromiseOrValue::Value(U128(0));
        }
        let delta = mul_div(total, PREC, self.eligible_supply);
        let distributed = mul_div(delta, self.eligible_supply, PREC);
        self.acc_reward_per_share += delta;
        self.reward_remainder = total - distributed;
        emit(
            "reward",
            json!({
                "amount": amount,
                "acc_reward_per_share": U128(self.acc_reward_per_share),
                "eligible_supply": U128(self.eligible_supply),
                "remainder": U128(self.reward_remainder),
            }),
        );
        PromiseOrValue::Value(U128(0))
    }

    /// Claim the caller's accrued wNEAR rewards. Settles, zeroes `unclaimed` optimistically, pays
    /// out via `wNEAR.ft_transfer`; `on_claim` restores it on transfer failure (exact unwind). The
    /// caller must be wNEAR-registered (standard). Zero-unclaimed is a no-op, not a panic.
    pub fn claim_rewards(&mut self) -> PromiseOrValue<U128> {
        let caller = env::predecessor_account_id();
        self.settle(&caller);
        let amount = *self.unclaimed.get(&caller).unwrap_or(&0);
        if amount == 0 {
            return PromiseOrValue::Value(U128(0));
        }
        self.unclaimed.insert(caller.clone(), 0);
        emit("claim", json!({ "account": caller, "amount": U128(amount) }));
        let args = json!({ "receiver_id": caller, "amount": U128(amount) })
            .to_string()
            .into_bytes();
        PromiseOrValue::Promise(
            Promise::new(WNEAR.parse().unwrap())
                .function_call(
                    "ft_transfer".to_string(),
                    args,
                    ONE_YOCTO,
                    GAS_FOR_WNEAR_TRANSFER,
                )
                .then(
                    Self::ext(env::current_account_id())
                        .with_static_gas(GAS_FOR_CLAIM_CB)
                        .on_claim(caller, U128(amount)),
                ),
        )
    }

    #[private]
    pub fn on_claim(
        &mut self,
        account: AccountId,
        amount: U128,
        #[callback_result] res: Result<(), PromiseError>,
    ) {
        if res.is_err() {
            let u = *self.unclaimed.get(&account).unwrap_or(&0);
            self.unclaimed.insert(account.clone(), u + amount.0);
            emit("claim_reverted", json!({ "account": account, "amount": amount }));
        }
    }

    /// Settled-preview of an account's claimable rewards (pending + already-`unclaimed`), for the UI.
    pub fn get_rewards(&self, account: AccountId) -> U128 {
        let u = *self.unclaimed.get(&account).unwrap_or(&0);
        if self.excluded.contains(&account) {
            return U128(u);
        }
        let entitled = mul_div(self.balance_of(&account), self.acc_reward_per_share, PREC);
        let debt = *self.reward_debt.get(&account).unwrap_or(&0);
        U128(u + entitled.saturating_sub(debt))
    }

    /// Exclude an account from rewards (authority only) — settles its pending into `unclaimed`
    /// (which stays claimable), removes its balance from `eligible_supply`, and stops it accruing.
    /// The router calls this to exclude the DCL pool before graduation (BRIEF gates 1 + 3).
    pub fn add_excluded(&mut self, account: AccountId) {
        require!(
            env::predecessor_account_id() == self.authority,
            "only the authority may exclude"
        );
        if self.excluded.contains(&account) {
            return;
        }
        self.settle(&account);
        let b = self.balance_of(&account);
        if b > 0 {
            self.eligible_supply -= b;
        }
        self.excluded.insert(account.clone());
        self.reward_debt.insert(account.clone(), 0);
        emit(
            "exclude",
            json!({ "account": account, "eligible_supply": U128(self.eligible_supply) }),
        );
    }

    pub fn is_excluded(&self, account: AccountId) -> bool {
        self.excluded.contains(&account)
    }
    pub fn get_eligible_supply(&self) -> U128 {
        U128(self.eligible_supply)
    }
    pub fn get_acc_reward_per_share(&self) -> U128 {
        U128(self.acc_reward_per_share)
    }
    pub fn get_reward_remainder(&self) -> U128 {
        U128(self.reward_remainder)
    }
    pub fn get_authority(&self) -> AccountId {
        self.authority.clone()
    }
}

// ---- NEP-145 storage management (delegated) ----
#[near]
impl StorageManagement for Contract {
    #[payable]
    fn storage_deposit(
        &mut self,
        account_id: Option<AccountId>,
        registration_only: Option<bool>,
    ) -> StorageBalance {
        let sb = self.token.storage_deposit(account_id.clone(), registration_only);
        // Snapshot a newly-registered non-excluded account's debt at the CURRENT accumulator so it
        // earns only from NOW on, never retroactively. Balance is 0 at first registration → debt 0;
        // idempotent if the account was already known.
        let acct = account_id.unwrap_or_else(env::predecessor_account_id);
        if !self.excluded.contains(&acct) && self.reward_debt.get(&acct).is_none() {
            let entitled = mul_div(self.balance_of(&acct), self.acc_reward_per_share, PREC);
            self.reward_debt.insert(acct, entitled);
        }
        sb
    }

    #[payable]
    fn storage_withdraw(&mut self, amount: Option<NearToken>) -> StorageBalance {
        self.token.storage_withdraw(amount)
    }

    #[payable]
    fn storage_unregister(&mut self, force: Option<bool>) -> bool {
        self.token.storage_unregister(force)
    }

    fn storage_balance_bounds(&self) -> StorageBalanceBounds {
        self.token.storage_balance_bounds()
    }

    fn storage_balance_of(&self, account_id: AccountId) -> Option<StorageBalance> {
        self.token.storage_balance_of(account_id)
    }
}

// ---- NEP-148 metadata ----
#[near]
impl FungibleTokenMetadataProvider for Contract {
    fn ft_metadata(&self) -> FungibleTokenMetadata {
        self.metadata.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use near_contract_standards::fungible_token::metadata::FT_METADATA_SPEC;
    use near_sdk::test_utils::{accounts, VMContextBuilder};
    use near_sdk::testing_env;

    const SUPPLY: u128 = 1_000_000_000_000_000_000; // 1e9 tokens @ 9 decimals

    fn meta() -> FungibleTokenMetadata {
        FungibleTokenMetadata {
            spec: FT_METADATA_SPEC.to_string(),
            name: "Test Cat".to_string(),
            symbol: "TCAT".to_string(),
            icon: None,
            reference: None,
            reference_hash: None,
            decimals: 9,
        }
    }

    fn router() -> AccountId {
        accounts(5) // "fargo" — stands in for the router/owner (excluded)
    }
    fn wnear() -> AccountId {
        WNEAR.parse().unwrap()
    }

    fn ctx(b: &mut VMContextBuilder, who: AccountId, deposit: NearToken) {
        testing_env!(b.predecessor_account_id(who).attached_deposit(deposit).build());
    }

    /// Init a contract with `router` as owner, and register + fund each `(holder, amount)` from the
    /// router. Returns the built contract.
    fn setup(b: &mut VMContextBuilder, holders: &[(AccountId, u128)]) -> Contract {
        testing_env!(b.predecessor_account_id(router()).build());
        let mut c = Contract::new(router(), U128(SUPPLY), meta());
        for (h, amt) in holders {
            ctx(b, router(), NearToken::from_millinear(10));
            c.storage_deposit(Some(h.clone()), None);
            if *amt > 0 {
                ctx(b, router(), NearToken::from_yoctonear(1));
                c.ft_transfer(h.clone(), U128(*amt), None);
            }
        }
        c
    }

    /// Simulate a wNEAR reward arriving via ft_on_transfer.
    fn reward(b: &mut VMContextBuilder, c: &mut Contract, amount: u128) {
        testing_env!(b.predecessor_account_id(wnear()).attached_deposit(NearToken::from_yoctonear(1)).build());
        let _ = c.ft_on_transfer(router(), U128(amount), "reward".to_string());
    }

    #[test]
    fn mints_full_supply_to_owner_once() {
        let mut b = VMContextBuilder::new();
        testing_env!(b.predecessor_account_id(router()).build());
        let c = Contract::new(router(), U128(SUPPLY), meta());
        assert_eq!(c.ft_total_supply().0, SUPPLY);
        assert_eq!(c.ft_balance_of(router()).0, SUPPLY);
        assert_eq!(c.ft_metadata().symbol, "TCAT");
        // owner is auto-excluded and contributes nothing to eligible_supply
        assert!(c.is_excluded(router()));
        assert_eq!(c.get_eligible_supply().0, 0);
    }

    #[test]
    #[should_panic(expected = "total supply must be positive")]
    fn rejects_zero_supply() {
        let mut b = VMContextBuilder::new();
        testing_env!(b.predecessor_account_id(router()).build());
        Contract::new(router(), U128(0), meta());
    }

    #[test]
    fn transfer_moves_balance_and_tracks_eligible() {
        let mut b = VMContextBuilder::new();
        let c = setup(&mut b, &[(accounts(0), 1_000)]);
        assert_eq!(c.ft_balance_of(router()).0, SUPPLY - 1_000);
        assert_eq!(c.ft_balance_of(accounts(0)).0, 1_000);
        // router excluded → only alice counts toward eligible_supply
        assert_eq!(c.get_eligible_supply().0, 1_000);
    }

    #[test]
    fn hold_only_account_accrues_across_distribute() {
        let mut b = VMContextBuilder::new();
        let mut c = setup(&mut b, &[(accounts(0), 1_000)]);
        // alice does NOTHING; a reward arrives; she accrues purely by holding.
        reward(&mut b, &mut c, 1_000);
        assert_eq!(c.get_rewards(accounts(0)).0, 1_000);
        // excluded router earns zero
        assert_eq!(c.get_rewards(router()).0, 0);
    }

    #[test]
    fn excluded_account_earns_zero() {
        let mut b = VMContextBuilder::new();
        // alice eligible; carol will be excluded by the authority even though she holds tokens.
        let mut c = setup(&mut b, &[(accounts(0), 1_000), (accounts(2), 1_000)]);
        assert_eq!(c.get_eligible_supply().0, 2_000);
        testing_env!(b.predecessor_account_id(router()).build());
        c.add_excluded(accounts(2));
        assert_eq!(c.get_eligible_supply().0, 1_000); // carol's balance removed
        reward(&mut b, &mut c, 1_000);
        assert_eq!(c.get_rewards(accounts(0)).0, 1_000); // alice gets it all
        assert_eq!(c.get_rewards(accounts(2)).0, 0); // carol excluded → nothing new
    }

    #[test]
    fn transfer_mid_accrual_splits_pro_rata_by_time_held() {
        let mut b = VMContextBuilder::new();
        let mut c = setup(&mut b, &[(accounts(0), 1_000), (accounts(1), 0)]);
        // R1: alice holds all 1000 → earns all 1000.
        reward(&mut b, &mut c, 1_000);
        // alice sends half to bob mid-cycle.
        ctx(&mut b, accounts(0), NearToken::from_yoctonear(1));
        c.ft_transfer(accounts(1), U128(500), None);
        assert_eq!(c.get_eligible_supply().0, 1_000); // nets zero between two eligible parties
        // R2: split 500/500 → alice +500, bob +500.
        reward(&mut b, &mut c, 1_000);
        assert_eq!(c.get_rewards(accounts(0)).0, 1_500); // all of R1 + half of R2
        assert_eq!(c.get_rewards(accounts(1)).0, 500); // half of R2, nothing of R1
    }

    #[test]
    fn eligible_supply_equals_sum_non_excluded_after_sequence() {
        let mut b = VMContextBuilder::new();
        let mut c = setup(&mut b, &[(accounts(0), 5_000), (accounts(1), 3_000)]);
        // a couple of transfers + an exclusion
        ctx(&mut b, accounts(0), NearToken::from_yoctonear(1));
        c.ft_transfer(accounts(1), U128(1_000), None);
        ctx(&mut b, accounts(1), NearToken::from_millinear(10));
        c.storage_deposit(Some(accounts(2)), None);
        ctx(&mut b, accounts(1), NearToken::from_yoctonear(1));
        c.ft_transfer(accounts(2), U128(500), None);
        testing_env!(b.predecessor_account_id(router()).build());
        c.add_excluded(accounts(2));
        let sum_non_excluded =
            c.ft_balance_of(accounts(0)).0 + c.ft_balance_of(accounts(1)).0; // carol excluded
        assert_eq!(c.get_eligible_supply().0, sum_non_excluded); // BRIEF gate 1 invariant
    }

    #[test]
    fn conservation_no_mint_bounded_dust() {
        let mut b = VMContextBuilder::new();
        // eligible = 3000 does NOT divide the reward evenly → exercises the remainder carry.
        let mut c = setup(&mut b, &[(accounts(0), 1_000), (accounts(1), 2_000)]);
        let received: u128 = 1_000_000;
        reward(&mut b, &mut c, received);
        let paid_out = c.get_rewards(accounts(0)).0 + c.get_rewards(accounts(1)).0;
        let accounted = paid_out + c.get_reward_remainder().0;
        // no mint: never exceed what was received (BRIEF gate 2)
        assert!(accounted <= received, "minted rewards: {} > {}", accounted, received);
        // the only shortfall is sub-yocto-per-holder flooring dust, bounded by #holders
        assert!(received - accounted < 2, "unbounded leak: {}", received - accounted);
    }

    #[test]
    fn reward_while_eligible_zero_carries_then_folds_in() {
        let mut b = VMContextBuilder::new();
        // no holders yet (only excluded router holds supply)
        let mut c = setup(&mut b, &[]);
        assert_eq!(c.get_eligible_supply().0, 0);
        reward(&mut b, &mut c, 777);
        // parked, accumulator untouched
        assert_eq!(c.get_reward_remainder().0, 777);
        assert_eq!(c.get_acc_reward_per_share().0, 0);
        // now a holder appears and another reward arrives → the parked amount folds in.
        ctx(&mut b, router(), NearToken::from_millinear(10));
        c.storage_deposit(Some(accounts(0)), None);
        ctx(&mut b, router(), NearToken::from_yoctonear(1));
        c.ft_transfer(accounts(0), U128(1_000), None);
        reward(&mut b, &mut c, 223);
        // 777 (parked) + 223 (new) = 1000, all to the sole holder
        assert_eq!(c.get_rewards(accounts(0)).0, 1_000);
        assert_eq!(c.get_reward_remainder().0, 0);
    }

    #[test]
    fn claim_zeroes_unclaimed_and_restores_on_failure() {
        let mut b = VMContextBuilder::new();
        let mut c = setup(&mut b, &[(accounts(0), 1_000)]);
        reward(&mut b, &mut c, 1_000);
        testing_env!(b.predecessor_account_id(accounts(0)).attached_deposit(NearToken::from_yoctonear(1)).build());
        let _ = c.claim_rewards();
        assert_eq!(c.get_rewards(accounts(0)).0, 0); // optimistic zero
        // simulate the wNEAR transfer failing → callback restores the unclaimed balance
        testing_env!(b.predecessor_account_id(accounts(0)).build());
        c.on_claim(accounts(0), U128(1_000), Err(PromiseError::Failed));
        assert_eq!(c.get_rewards(accounts(0)).0, 1_000);
    }

    #[test]
    fn zero_unclaimed_claim_is_noop() {
        let mut b = VMContextBuilder::new();
        let mut c = setup(&mut b, &[(accounts(0), 1_000)]);
        testing_env!(b.predecessor_account_id(accounts(0)).attached_deposit(NearToken::from_yoctonear(1)).build());
        match c.claim_rewards() {
            PromiseOrValue::Value(v) => assert_eq!(v.0, 0),
            _ => panic!("expected a no-op Value(0), got a Promise"),
        }
    }
}





