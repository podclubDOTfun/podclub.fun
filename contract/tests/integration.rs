//! near-workspaces integration suite for the FastLaunch router (Phase 6b).
//!
//! Runs entirely in a LOCAL `neard` sandbox — no testnet, no faucet, no credentials. It deploys the
//! COMMITTED artifacts (`res/fastlaunch_router.wasm`, `res/fastlaunch_ft.wasm`, byte-identical to
//! what ships / what the FT global-contract hash points at) plus an in-repo mock DCL, and drives the
//! real async cross-contract paths the unit tests can't reach.
//!
//! DCL approach: (b) mock stub.
//!
//! create_launch note: near-workspaces 0.23 cannot publish a NEAR *global contract*, so the real
//! `create_launch` deploy leg (`use_global_contract(FT_GLOBAL_CODE_HASH)`) cannot resolve in-sandbox.
//! The launch lifecycle is therefore stood up via the owner path `register_launch` + a direct FT
//! deploy + the real `ft_on_transfer` "seed" — the same `record_launch`/seed/curve code. create_launch
//! INPUT validation is still exercised directly.
use near_workspaces::network::Sandbox;
use near_workspaces::types::{AccessKey, KeyType, NearToken, SecretKey};
use near_workspaces::{Account, AccountId, Contract, Worker};
use serde_json::{json, Value};

// ---- committed, byte-reproducible artifacts (deterministic; no runtime file discovery) ----
const ROUTER_WASM: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/res/fastlaunch_router.wasm"));
const FT_WASM: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/res/fastlaunch_ft.wasm"));

// Hardcoded external account ids the router calls by name (must be materialized EXACTLY in-sandbox).
const WNEAR: &str = "wrap.testnet";
const DCL: &str = "dclv2.ref-dev.testnet";

const YOCTO: u128 = 1;
fn near(n: u128) -> NearToken { NearToken::from_yoctonear(n * 1_000_000_000_000_000_000_000_000) }
fn millinear(n: u128) -> NearToken { NearToken::from_yoctonear(n * 1_000_000_000_000_000_000_000) }

/// Build the in-repo mock DCL to wasm (deps already cached). Deterministic given the toolchain.
fn mock_dcl_wasm() -> Vec<u8> {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../mock-dcl");
    let out = concat!(env!("CARGO_MANIFEST_DIR"), "/../mock-dcl/target/wasm32-unknown-unknown/release/mock_dcl.wasm");
    let status = std::process::Command::new("cargo")
        .args(["build", "--release", "--target", "wasm32-unknown-unknown"])
        .current_dir(dir)
        .status()
        .expect("spawn cargo to build mock-dcl");
    assert!(status.success(), "mock-dcl wasm build failed");
    std::fs::read(out).expect("read mock-dcl wasm")
}

/// Materialize an account with an EXACT id (incl. TLAs like `wrap.testnet` the sandbox root can't
/// otherwise mint) by patching state directly, then return a signer for it.
async fn named_account(worker: &Worker<Sandbox>, id: &str, bal: NearToken) -> anyhow::Result<Account> {
    let account_id: AccountId = id.parse()?;
    let sk = SecretKey::from_random(KeyType::ED25519);
    worker
        .patch(&account_id)
        .account(near_workspaces::AccountDetailsPatch::default().balance(bal))
        .access_key(sk.public_key(), AccessKey::full_access())
        .transact()
        .await?;
    Ok(Account::from_secret_key(account_id, sk, worker))
}

/// Deploy the committed FT wasm at `id` and mint `supply` to `owner`.
async fn deploy_ft(
    worker: &Worker<Sandbox>,
    id: &str,
    owner: &Account,
    supply: u128,
    symbol: &str,
) -> anyhow::Result<Contract> {
    let acct = named_account(worker, id, near(50)).await?;
    let ft = acct.deploy(FT_WASM).await?.into_result()?;
    ft.call("new")
        .args_json(json!({
            "owner_id": owner.id(),
            "total_supply": supply.to_string(),
            "metadata": { "spec": "ft-1.0.0", "name": symbol, "symbol": symbol, "decimals": 24 }
        }))
        .max_gas()
        .transact()
        .await?
        .into_result()?;
    Ok(ft)
}

/// NEP-145 register `who` on the token at `ft_id` (over-deposit is refunded).
async fn register(who: &Account, ft_id: &AccountId) -> anyhow::Result<()> {
    who.call(ft_id, "storage_deposit")
        .args_json(json!({ "account_id": who.id() }))
        .deposit(millinear(10))
        .max_gas()
        .transact()
        .await?
        .into_result()?;
    Ok(())
}

fn u128s(v: &Value, k: &str) -> u128 { v[k].as_str().unwrap().parse().unwrap() }

struct World {
    worker: Worker<Sandbox>,
    owner: Account,
    treasury: Account,
    router: Contract,
    wnear: Contract,
    bank: Account,
    dcl: Contract,
}

/// Stand up router + wNEAR (at wrap.testnet) + mock DCL (at dclv2.ref-dev.testnet).
async fn setup() -> anyhow::Result<World> {
    let worker = near_workspaces::sandbox().await?;
    let owner = worker.dev_create_account().await?;
    let treasury = worker.dev_create_account().await?;
    let bank = worker.dev_create_account().await?;

    let router = worker.dev_deploy(ROUTER_WASM).await?;
    router
        .call("new")
        .args_json(json!({ "owner_id": owner.id(), "treasury_id": treasury.id() }))
        .max_gas()
        .transact()
        .await?
        .into_result()?;

    // wNEAR: a plain NEP-141 (the router only ever receives/sends it) with a big float held by bank.
    let wnear = deploy_ft(&worker, WNEAR, &bank, 1_000_000_000_000_000_000_000_000_000_000, "wNEAR").await?;

    // mock DCL at the exact hardcoded id.
    let dcl_acct = named_account(&worker, DCL, near(50)).await?;
    let dcl = dcl_acct.deploy(&mock_dcl_wasm()).await?.into_result()?;
    dcl.call("new").max_gas().transact().await?.into_result()?;

    // The router custodies wNEAR (buy proceeds; sell/claim payouts) — register it on wNEAR.
    register(router.as_account(), wnear.id()).await?;

    Ok(World { worker, owner, treasury, router, wnear, bank, dcl })
}

impl World {
    /// Register `who` on wNEAR and transfer them `amount` wNEAR from the bank.
    async fn give_wnear(&self, who: &Account, amount: u128) -> anyhow::Result<()> {
        register(who, self.wnear.id()).await?;
        self.bank
            .call(self.wnear.id(), "ft_transfer")
            .args_json(json!({ "receiver_id": who.id(), "amount": amount.to_string() }))
            .deposit(NearToken::from_yoctonear(YOCTO))
            .max_gas()
            .transact()
            .await?
            .into_result()?;
        Ok(())
    }

    /// Deploy a reward-bearing launch FT at `ft_id` minted to the ROUTER (mirroring production, where
    /// `create_launch` mints the whole supply to the router and it seeds `token_reserve` directly in
    /// `on_ft_deployed`) — so the router is the FT's auto-excluded owner AND its reward `authority`
    /// (the account allowed to `add_excluded(DCL)` at `seed_pool`). The harness can't run the private
    /// factory callback, so it materializes the same end state via the real `ft_on_transfer` "seed"
    /// path with a round-trip: router → creator (deliver the on-curve supply), then creator → router
    /// with msg `"seed"` (sets `token_reserve`). Net result: router holds the full reserve (excluded),
    /// `eligible_supply == 0`, `authority == router` — identical to production.
    async fn register_launch_seeded(
        &self,
        ft_id: &str,
        creator: &Account,
        virtual_near: u128,
        on_curve_supply: u128,
        graduation_near: u128,
        split: (u16, u16, u16),
    ) -> anyhow::Result<Contract> {
        let ft = deploy_ft(&self.worker, ft_id, self.router.as_account(), on_curve_supply, "TCAT").await?;
        self.owner
            .call(self.router.id(), "register_launch")
            .args_json(json!({
                "token_id": ft.id(), "creator": creator.id(), "name": "Test Cat", "symbol": "TCAT",
                "tier": "Economy", "virtual_near": virtual_near.to_string(),
                "on_curve_supply": on_curve_supply.to_string(), "graduation_near": graduation_near.to_string(),
                "creator_bps": split.0, "buyback_bps": split.1, "holder_bps": split.2, "socials": null
            }))
            .max_gas()
            .transact()
            .await?
            .into_result()?;
        // Round-trip the on-curve supply through the creator to drive the real "seed" path: the router
        // (owner) hands the creator the supply, the creator seeds it back. Both legs settle correctly
        // (router is excluded so eligible_supply nets back to 0).
        register(creator, ft.id()).await?;
        self.router
            .as_account()
            .call(ft.id(), "ft_transfer")
            .args_json(json!({ "receiver_id": creator.id(), "amount": on_curve_supply.to_string() }))
            .deposit(NearToken::from_yoctonear(YOCTO))
            .max_gas()
            .transact()
            .await?
            .into_result()?;
        let seed_msg = json!({ "action": "seed", "min_out": "0" }).to_string();
        creator
            .call(ft.id(), "ft_transfer_call")
            .args_json(json!({ "receiver_id": self.router.id(), "amount": on_curve_supply.to_string(), "msg": seed_msg }))
            .deposit(NearToken::from_yoctonear(YOCTO))
            .max_gas()
            .transact()
            .await?
            .into_result()?;
        Ok(ft)
    }

    async fn go_live(&self, token_id: &AccountId) -> anyhow::Result<()> {
        self.owner
            .call(self.router.id(), "go_live")
            .args_json(json!({ "token_id": token_id }))
            .max_gas()
            .transact()
            .await?
            .into_result()?;
        Ok(())
    }
    /// Buy `amount` wNEAR worth of `token_id`. Registers the buyer on the launch FT so delivery
    /// lands. Returns the full execution (all receipts) so callers can assert success/logs.
    async fn buy(
        &self,
        buyer: &Account,
        token_id: &AccountId,
        amount: u128,
        min_out: u128,
        dev: bool,
    ) -> anyhow::Result<near_workspaces::result::ExecutionFinalResult> {
        register(buyer, token_id).await?;
        let msg = json!({ "token_id": token_id, "min_out": min_out.to_string(), "dev_buy": dev }).to_string();
        Ok(buyer
            .call(self.wnear.id(), "ft_transfer_call")
            .args_json(json!({ "receiver_id": self.router.id(), "amount": amount.to_string(), "msg": msg }))
            .deposit(NearToken::from_yoctonear(YOCTO))
            .max_gas()
            .transact()
            .await?)
    }

    async fn sell(
        &self,
        seller: &Account,
        ft_id: &AccountId,
        amount: u128,
        min_out: u128,
    ) -> anyhow::Result<near_workspaces::result::ExecutionFinalResult> {
        let msg = json!({ "action": "sell", "min_out": min_out.to_string() }).to_string();
        Ok(seller
            .call(ft_id, "ft_transfer_call")
            .args_json(json!({ "receiver_id": self.router.id(), "amount": amount.to_string(), "msg": msg }))
            .deposit(NearToken::from_yoctonear(YOCTO))
            .max_gas()
            .transact()
            .await?)
    }

    async fn launch(&self, token_id: &AccountId) -> anyhow::Result<Value> {
        Ok(self.router.view("get_launch").args_json(json!({ "token_id": token_id })).await?.json::<Value>()?)
    }

    async fn accrual(&self, token_id: &AccountId) -> anyhow::Result<Value> {
        Ok(self.router.view("get_accrual").args_json(json!({ "token_id": token_id })).await?.json::<Value>()?)
    }

    async fn protocol_fees(&self) -> anyhow::Result<u128> {
        Ok(self.router.view("get_protocol_fees").await?.json::<String>()?.parse()?)
    }

    async fn ft_balance(&self, ft_id: &AccountId, who: &AccountId) -> anyhow::Result<u128> {
        Ok(self.worker.view(ft_id, "ft_balance_of").args_json(json!({ "account_id": who })).await?.json::<String>()?.parse()?)
    }

    /// Permissionless keeper: flush the launch's accrued `holder` bucket into the reward-bearing FT
    /// (`distribute_holder_rewards` forwards the wNEAR to the FT via `ft_transfer_call(.., "reward")`).
    async fn distribute(&self, who: &Account, token_id: &AccountId) -> anyhow::Result<()> {
        who.call(self.router.id(), "distribute_holder_rewards")
            .args_json(json!({ "token_id": token_id }))
            .max_gas()
            .transact()
            .await?
            .into_result()?;
        Ok(())
    }

    /// Claim `who`'s accrued wNEAR rewards from the reward-bearing launch FT itself.
    async fn claim_rewards(&self, who: &Account, ft_id: &AccountId) -> anyhow::Result<()> {
        who.call(ft_id, "claim_rewards").max_gas().transact().await?.into_result()?;
        Ok(())
    }

    /// A buy that does NOT storage-register the buyer on the launch FT — the token delivery leg
    /// therefore fails and the router must roll the whole buy back (fee unbooked, wNEAR refunded).
    async fn buy_unregistered(
        &self,
        buyer: &Account,
        token_id: &AccountId,
        amount: u128,
    ) -> anyhow::Result<near_workspaces::result::ExecutionFinalResult> {
        let msg = json!({ "token_id": token_id, "min_out": "0", "dev_buy": false }).to_string();
        Ok(buyer
            .call(self.wnear.id(), "ft_transfer_call")
            .args_json(json!({ "receiver_id": self.router.id(), "amount": amount.to_string(), "msg": msg }))
            .deposit(NearToken::from_yoctonear(YOCTO))
            .max_gas()
            .transact()
            .await?)
    }

    /// The launch FT's settled-preview of an account's claimable wNEAR rewards (pending + unclaimed).
    async fn ft_rewards(&self, ft_id: &AccountId, who: &AccountId) -> anyhow::Result<u128> {
        Ok(self
            .worker
            .view(ft_id, "get_rewards")
            .args_json(json!({ "account": who }))
            .await?
            .json::<String>()?
            .parse()?)
    }

    /// The launch FT's `eligible_supply` (Σ non-excluded balances — the reward denominator).
    async fn ft_eligible(&self, ft_id: &AccountId) -> anyhow::Result<u128> {
        Ok(self.worker.view(ft_id, "get_eligible_supply").await?.json::<String>()?.parse()?)
    }
}

// ============================ TESTS ============================
// ---- Coverage 3 + 4: constant-product curve to the yocto, 1% fee, 90/10 split, bucket booking ----
#[tokio::test]
async fn curve_and_fee_split_exact_to_yocto() -> anyhow::Result<()> {
    let w = setup().await?;
    // sanity: `new` wired ownership + treasury as passed.
    assert_eq!(w.router.view("get_owner").await?.json::<String>()?, w.owner.id().to_string());
    assert_eq!(w.router.view("get_treasury").await?.json::<String>()?, w.treasury.id().to_string());
    let creator = w.worker.dev_create_account().await?;
    let ft = w
        .register_launch_seeded("cat.test.near", &creator, 1_000_000, 1_000_000, u128::MAX >> 4, (7000, 2000, 1000))
        .await?;
    w.go_live(ft.id()).await?;
    // quote (view) must equal the realized curve output, to the yocto.
    let quote: u128 = w
        .router
        .view("quote_buy")
        .args_json(json!({ "token_id": ft.id(), "amount": "1000000" }))
        .await?
        .json::<String>()?
        .parse()?;
    assert_eq!(quote, 497_487, "constant-product quote off");

    let buyer = w.worker.dev_create_account().await?;
    w.give_wnear(&buyer, 1_000_000).await?;
    let r = w.buy(&buyer, ft.id(), 1_000_000, 0, false).await?;
    assert!(r.is_success(), "buy failed: {:?}", r.into_result().err());

    let l = w.launch(ft.id()).await?;
    assert_eq!(u128s(&l, "token_reserve"), 1_000_000 - 497_487, "reserve");
    assert_eq!(u128s(&l, "real_near"), 990_000, "real_near");
    assert_eq!(u128s(&l, "volume"), 1_000_000, "volume (gross)");
    assert_eq!(u128s(&l, "fees_collected"), 9_000, "pool cut");

    assert_eq!(w.protocol_fees().await?, 1_000, "protocol 10%");
    let a = w.accrual(ft.id()).await?;
    assert_eq!(u128s(&a, "creator"), 6_300, "creator 70% of pool");
    assert_eq!(u128s(&a, "buyback"), 1_800, "buyback 20% of pool");
    assert_eq!(u128s(&a, "holder"), 900, "holder 10% (absorbs remainder)");
    // protocol + pool == fee; buckets sum to pool — nothing created or destroyed.
    assert_eq!(1_000 + 9_000, 10_000);
    assert_eq!(6_300 + 1_800 + 900, 9_000);

    // tokens were actually delivered to the buyer.
    assert_eq!(w.ft_balance(ft.id(), buyer.id()).await?, 497_487, "delivered tokens");
    Ok(())
}

// ---- Coverage 1: full lifecycle happy path buy -> sell -> graduate -> seed_pool (mock DCL success) ----
#[tokio::test]
async fn lifecycle_buy_sell_graduate_seed_pool() -> anyhow::Result<()> {
    let w = setup().await?;
    let creator = w.worker.dev_create_account().await?;
    // token_x case: "cat…" sorts below "wrap.testnet", so the single-sided range sits ABOVE spot.
    let ft = w
        .register_launch_seeded("cat.test.near", &creator, 1_000_000, 1_000_000, 500_000, (7000, 2000, 1000))
        .await?;
    w.go_live(ft.id()).await?;

    let trader = w.worker.dev_create_account().await?;
    w.give_wnear(&trader, 2_000_000).await?;

    // buy, then sell some back (exercise both curve directions), then buy across the threshold.
    assert!(w.buy(&trader, ft.id(), 200_000, 0, false).await?.is_success());
    let held = w.ft_balance(ft.id(), trader.id()).await?;
    assert!(held > 0, "buyer got tokens");
    assert!(w.sell(&trader, ft.id(), held / 2, 0).await?.is_success(), "sell");
    assert!(w.buy(&trader, ft.id(), 800_000, 0, false).await?.is_success());

    let l = w.launch(ft.id()).await?;
    assert!(u128s(&l, "real_near") >= 500_000, "reached graduation threshold");

    // graduate closes the curve (moves no funds), then seed_pool migrates to the locked DCL position.
    w.owner.call(w.router.id(), "graduate").args_json(json!({ "token_id": ft.id() })).max_gas().transact().await?.into_result()?;
    let graduated = w.launch(ft.id()).await?;
    assert_eq!(graduated["phase"], "Graduated");
    let migrating = u128s(&graduated, "token_reserve");
    assert!(migrating > 0);

    let r = w
        .owner
        .call(w.router.id(), "seed_pool")
        .args_json(json!({ "token_id": ft.id() })) // MEDIUM-1: points derived on-chain, no owner input
        .max_gas()
        .transact()
        .await?;
    assert!(r.is_success(), "seed_pool saga failed: {:?}", r.clone().into_result().err());

    // Router state: Pooled, curve reserve cleared.
    let pooled = w.launch(ft.id()).await?;
    assert_eq!(pooled["phase"], "Pooled", "phase after seed_pool");
    assert_eq!(u128s(&pooled, "token_reserve"), 0, "reserve migrated");

    // DCL state: single-sided pool funded on token_x, LP position owned by the router (locked).
    let pool_id = format!("{}|{}|2000", ft.id(), WNEAR);
    let pool = w.dcl.view("get_pool").args_json(json!({ "pool_id": pool_id })).await?.json::<Value>()?;
    assert_eq!(u128s(&pool, "total_x"), migrating, "all launch tokens are token_x liquidity");
    assert_eq!(u128s(&pool, "total_y"), 0, "single-sided: zero wNEAR");
    let lp = w.dcl.view("get_liquidity").args_json(json!({ "lpt_id": "1" })).await?.json::<Value>()?;
    assert_eq!(lp["owner_id"], w.router.id().to_string(), "LP-NFT owned by router");
    Ok(())
}

// ---- Coverage 3: dev-buy 4% cap enforced (Seeding-only, creator-only, cumulative cap) ----
#[tokio::test]
async fn dev_buy_capped_at_4pct() -> anyhow::Result<()> {
    let w = setup().await?;
    let creator = w.worker.dev_create_account().await?;
    // Huge graduation so nothing graduates mid-test. Seeding phase (no go_live) — dev buys only run here.
    let ft = w
        .register_launch_seeded("cat.test.near", &creator, 1_000_000, 1_000_000, u128::MAX >> 4, (7000, 2000, 1000))
        .await?;
    w.give_wnear(&creator, 500_000).await?;

    // Cap = 4% of on_curve_supply = 40_000 tokens. An over-cap dev buy is refunded WHOLE, no state change.
    let over = w.buy(&creator, ft.id(), 100_000, 0, true).await?;
    assert!(over.is_success(), "ft_transfer_call itself resolves (router refunds)");
    assert_eq!(w.ft_balance(ft.id(), creator.id()).await?, 0, "over-cap dev buy delivered nothing");
    let l = w.launch(ft.id()).await?;
    assert_eq!(u128s(&l, "dev_bought"), 0, "dev_bought unchanged after over-cap refund");
    assert_eq!(u128s(&l, "token_reserve"), 1_000_000, "reserve unchanged after over-cap refund");

    // An under-cap dev buy fills: 20_000 wNEAR -> 19_800 after 1% fee -> 19_415 tokens (< 40_000 cap).
    let under = w.buy(&creator, ft.id(), 20_000, 0, true).await?;
    assert!(under.is_success());
    assert_eq!(w.ft_balance(ft.id(), creator.id()).await?, 19_415, "under-cap dev buy delivered exactly");
    let l = w.launch(ft.id()).await?;
    assert_eq!(u128s(&l, "dev_bought"), 19_415, "dev_bought booked");

    // A non-creator cannot dev buy at all (require! creator) — refunded, nothing delivered.
    let rando = w.worker.dev_create_account().await?;
    w.give_wnear(&rando, 20_000).await?;
    let _ = w.buy(&rando, ft.id(), 20_000, 0, true).await?;
    assert_eq!(w.ft_balance(ft.id(), rando.id()).await?, 0, "non-creator dev buy delivered nothing");
    Ok(())
}

// ---- Coverage 4: fee buckets book, unbook conservatively on a failed buy, then claim/buyback drain ----
#[tokio::test]
async fn fee_buckets_book_unbook_and_claim() -> anyhow::Result<()> {
    let w = setup().await?;
    let creator = w.worker.dev_create_account().await?;
    let ft = w
        .register_launch_seeded("cat.test.near", &creator, 1_000_000, 1_000_000, u128::MAX >> 4, (7000, 2000, 1000))
        .await?;
    w.go_live(ft.id()).await?;
    let trader = w.worker.dev_create_account().await?;
    w.give_wnear(&trader, 1_000_000).await?;
    assert!(w.buy(&trader, ft.id(), 1_000_000, 0, false).await?.is_success());

    // Booked: 1% fee = 10_000 -> protocol 1_000 + pool 9_000 -> creator 6_300 / buyback 1_800 / holder 900.
    let a = w.accrual(ft.id()).await?;
    assert_eq!((u128s(&a, "creator"), u128s(&a, "buyback"), u128s(&a, "holder")), (6_300, 1_800, 900));
    assert_eq!(w.protocol_fees().await?, 1_000);

    // A FAILED buy (buyer never registered on the launch FT -> delivery traps) must unbook EXACTLY:
    // every bucket returns to its pre-buy value and the buyer's wNEAR is fully refunded.
    let ghost = w.worker.dev_create_account().await?;
    w.give_wnear(&ghost, 500_000).await?;
    let _ = w.buy_unregistered(&ghost, ft.id(), 500_000).await?;
    let a2 = w.accrual(ft.id()).await?;
    assert_eq!((u128s(&a2, "creator"), u128s(&a2, "buyback"), u128s(&a2, "holder")), (6_300, 1_800, 900), "unbooked");
    assert_eq!(w.protocol_fees().await?, 1_000, "protocol unbooked");
    assert_eq!(w.ft_balance(w.wnear.id(), ghost.id()).await?, 500_000, "failed buy fully refunded");
    assert_eq!(w.ft_balance(ft.id(), ghost.id()).await?, 0, "no tokens delivered on failed buy");

    // Creator claims its bucket -> wNEAR paid, bucket zeroed.
    register(&creator, w.wnear.id()).await?;
    creator.call(w.router.id(), "claim_creator_fees").args_json(json!({ "token_id": ft.id() })).max_gas().transact().await?.into_result()?;
    assert_eq!(u128s(&w.accrual(ft.id()).await?, "creator"), 0, "creator bucket drained");
    assert_eq!(w.ft_balance(w.wnear.id(), creator.id()).await?, 6_300, "creator paid in wNEAR");

    // Owner claims the protocol fees.
    register(&w.owner, w.wnear.id()).await?;
    w.owner.call(w.router.id(), "claim_protocol_fees").max_gas().transact().await?.into_result()?;
    assert_eq!(w.protocol_fees().await?, 0, "protocol drained");
    assert_eq!(w.ft_balance(w.wnear.id(), w.owner.id()).await?, 1_000, "owner paid protocol fees");

    // Buyback spends its bucket on the curve and locks (burns) the bought tokens.
    let before = u128s(&w.launch(ft.id()).await?, "token_reserve");
    w.owner.call(w.router.id(), "execute_buyback").args_json(json!({ "token_id": ft.id() })).max_gas().transact().await?.into_result()?;
    assert_eq!(u128s(&w.accrual(ft.id()).await?, "buyback"), 0, "buyback bucket spent");
    let l = w.launch(ft.id()).await?;
    assert!(u128s(&l, "burned") > 0 && u128s(&l, "token_reserve") < before, "tokens locked off the curve");
    Ok(())
}

// ---- B1 + LOW-1: hold-to-earn — buy -> HOLD -> distribute -> claim; a late buyer earns nothing prior ----
// No stake anywhere: the reward-bearing launch FT accrues the holder bucket to plain wallet balances.
#[tokio::test]
async fn holder_hold_distribute_claim_and_late_buyer_earns_nothing() -> anyhow::Result<()> {
    const PREC: u128 = 1_000_000_000_000_000_000_000_000; // FT accumulator scale (1e24)
    let w = setup().await?;
    let creator = w.worker.dev_create_account().await?;
    let ft = w
        .register_launch_seeded("cat.test.near", &creator, 10_000_000, 10_000_000, u128::MAX >> 4, (2000, 1000, 7000))
        .await?;
    w.go_live(ft.id()).await?;

    // Two DIFFERENT non-owner accounts buy and simply HOLD — no stake, the mechanism is gone.
    let alice = w.worker.dev_create_account().await?;
    w.give_wnear(&alice, 5_000_000).await?;
    assert!(w.buy(&alice, ft.id(), 5_000_000, 0, false).await?.is_success());
    let alice_tokens = w.ft_balance(ft.id(), alice.id()).await?;
    let bob = w.worker.dev_create_account().await?;
    w.give_wnear(&bob, 2_000_000).await?;
    assert!(w.buy(&bob, ft.id(), 2_000_000, 0, false).await?.is_success());
    let bob_tokens = w.ft_balance(ft.id(), bob.id()).await?;

    // Gate 1: eligible_supply is EXACTLY the two holders — the router's huge unsold reserve is
    // excluded, so it can never dilute or steal real holders' rewards.
    let elig = alice_tokens + bob_tokens;
    assert_eq!(w.ft_eligible(ft.id()).await?, elig, "eligible == Σ non-excluded holders");
    assert_eq!(w.ft_rewards(ft.id(), w.router.id()).await?, 0, "excluded router accrues nothing");

    // The holder bucket accrued from both buys; a PERMISSIONLESS keeper flushes it into the FT.
    let holder_bucket = u128s(&w.accrual(ft.id()).await?, "holder");
    assert!(holder_bucket > 0, "holder bucket accrued from the two buys");
    let rando = w.worker.dev_create_account().await?;
    w.distribute(&rando, ft.id()).await?;
    assert_eq!(u128s(&w.accrual(ft.id()).await?, "holder"), 0, "bucket zeroed once forwarded to the FT");

    // Both accrue purely by HOLDING — pro-rata to balance, exact to the yocto via the FT accumulator.
    let alice_pending = w.ft_rewards(ft.id(), alice.id()).await?;
    let bob_pending = w.ft_rewards(ft.id(), bob.id()).await?;
    assert!(alice_pending > 0 && bob_pending > 0, "both holders earned by holding");
    assert!(alice_pending > bob_pending, "alice held more → earned proportionally more");
    let acc = holder_bucket * PREC / elig; // reward-per-share the FT booked (floor)
    assert_eq!(alice_pending, alice_tokens * acc / PREC, "alice pro-rata exact");
    assert_eq!(bob_pending, bob_tokens * acc / PREC, "bob pro-rata exact");
    // Gate 2 conservation: paid + carried dust never exceeds the forwarded bucket (no mint).
    let remainder = w.worker.view(ft.id(), "get_reward_remainder").await?.json::<String>()?.parse::<u128>()?;
    let accounted = alice_pending + bob_pending + remainder;
    assert!(accounted <= holder_bucket, "no mint: {} > {}", accounted, holder_bucket);
    assert!(holder_bucket - accounted < 2, "unbounded leak: {}", holder_bucket - accounted);

    // LOW-1: carol buys AFTER the distribute. Her debt is snapshotted at the current accumulator on
    // delivery, so she has ZERO claim on the already-distributed round.
    let carol = w.worker.dev_create_account().await?;
    w.give_wnear(&carol, 2_000_000).await?;
    assert!(w.buy(&carol, ft.id(), 2_000_000, 0, false).await?.is_success());
    let carol_tokens = w.ft_balance(ft.id(), carol.id()).await?;
    assert_eq!(w.ft_rewards(ft.id(), carol.id()).await?, 0, "LOW-1: late buyer earns nothing prior");
    assert_eq!(w.ft_eligible(ft.id()).await?, elig + carol_tokens, "carol joins eligible_supply");

    // Alice claims her rewards straight from the FT in wNEAR; the pending zeroes out.
    let before = w.ft_balance(w.wnear.id(), alice.id()).await?;
    w.claim_rewards(&alice, ft.id()).await?;
    let after = w.ft_balance(w.wnear.id(), alice.id()).await?;
    assert_eq!(after - before, alice_pending, "alice paid exactly her pending in wNEAR");
    assert_eq!(w.ft_rewards(ft.id(), alice.id()).await?, 0, "pending cleared after claim");

    // Settle-on-transfer: bob sends half his balance to a fresh wallet mid-cycle. Bob KEEPS the
    // rewards he accrued before the move; the receiver starts earning only from now.
    let dave = w.worker.dev_create_account().await?;
    register(&dave, ft.id()).await?;
    let bob_before = w.ft_rewards(ft.id(), bob.id()).await?;
    bob.call(ft.id(), "ft_transfer")
        .args_json(json!({ "receiver_id": dave.id(), "amount": (bob_tokens / 2).to_string() }))
        .deposit(NearToken::from_yoctonear(YOCTO))
        .max_gas()
        .transact()
        .await?
        .into_result()?;
    assert_eq!(w.ft_rewards(ft.id(), bob.id()).await?, bob_before, "sender keeps pre-transfer rewards");
    assert_eq!(w.ft_rewards(ft.id(), dave.id()).await?, 0, "receiver earns only from now on");
    assert_eq!(w.ft_eligible(ft.id()).await?, elig + carol_tokens, "transfer between eligible parties nets zero");
    Ok(())
}

// ---- Regression MEDIUM-1 (FIXED): permissionless graduate AND permissionless seed ----
// graduate() is permissionless (threshold-gated, moves no funds) and freezes both buy and sell —
// the freeze is INTENDED. MEDIUM-1's fix makes the exit (seed_pool) permissionless too, with the DCL
// price points derived on-chain, so a negligent/lost-key owner can no longer strand holders in
// Graduated forever: ANY caller can drive the launch through to Pooled.
#[tokio::test]
async fn medium1_permissionless_graduate_freezes_trades() -> anyhow::Result<()> {
    let w = setup().await?;
    let creator = w.worker.dev_create_account().await?;
    let ft = w
        .register_launch_seeded("cat.test.near", &creator, 1_000_000, 1_000_000, 500_000, (7000, 2000, 1000))
        .await?;
    w.go_live(ft.id()).await?;
    let trader = w.worker.dev_create_account().await?;
    w.give_wnear(&trader, 1_000_000).await?;
    assert!(w.buy(&trader, ft.id(), 600_000, 0, false).await?.is_success());
    let held = w.ft_balance(ft.id(), trader.id()).await?;
    assert!(held > 0);
    assert!(u128s(&w.launch(ft.id()).await?, "real_near") >= 500_000, "eligible to graduate");

    // A NON-owner (no privileges) can graduate — permissionless, threshold-gated, moves no funds.
    let rando = w.worker.dev_create_account().await?;
    rando.call(w.router.id(), "graduate").args_json(json!({ "token_id": ft.id() })).max_gas().transact().await?.into_result()?;
    assert_eq!(w.launch(ft.id()).await?["phase"], "Graduated", "any account graduated the launch");

    // Buy is frozen: refunded whole, no tokens delivered.
    let buyer = w.worker.dev_create_account().await?;
    w.give_wnear(&buyer, 100_000).await?;
    let _ = w.buy(&buyer, ft.id(), 100_000, 0, false).await?;
    assert_eq!(w.ft_balance(ft.id(), buyer.id()).await?, 0, "buy frozen once graduated");
    assert_eq!(w.ft_balance(w.wnear.id(), buyer.id()).await?, 100_000, "buyer's wNEAR refunded");

    // Sell is frozen too (intended): the seller's tokens come straight back, curve untouched.
    let real_before = u128s(&w.launch(ft.id()).await?, "real_near");
    let _ = w.sell(&trader, ft.id(), held, 0).await?;
    assert_eq!(w.ft_balance(ft.id(), trader.id()).await?, held, "sell frozen: tokens returned");
    assert_eq!(u128s(&w.launch(ft.id()).await?, "real_near"), real_before, "curve untouched by frozen sell");

    // MEDIUM-1 fix: the SAME non-owner can now seed the pool (points derived on-chain), so the launch
    // is never stranded — it reaches Pooled without any owner action.
    let migrating = u128s(&w.launch(ft.id()).await?, "token_reserve");
    let r = rando
        .call(w.router.id(), "seed_pool")
        .args_json(json!({ "token_id": ft.id() }))
        .max_gas()
        .transact()
        .await?;
    assert!(r.is_success(), "permissionless seed_pool failed: {:?}", r.clone().into_result().err());
    let l = w.launch(ft.id()).await?;
    assert_eq!(l["phase"], "Pooled", "non-owner drove the launch to Pooled — holders not stranded");
    assert_eq!(u128s(&l, "token_reserve"), 0, "reserve migrated into the locked position");
    // The derived pool holds the full reserve, single-sided, locked to the router.
    let pool = w.dcl.view("get_pool").args_json(json!({ "pool_id": format!("{}|{}|2000", ft.id(), WNEAR) })).await?.json::<Value>()?;
    assert_eq!(u128s(&pool, "total_x"), migrating, "full reserve seeded as token_x liquidity");
    let lp = w.dcl.view("get_liquidity").args_json(json!({ "lpt_id": "1" })).await?.json::<Option<Value>>()?.expect("LP-NFT minted");
    assert_eq!(lp["owner_id"], w.router.id().to_string(), "LP locked to the router");
    Ok(())
}

// ---- Regression MEDIUM-2 (FIXED): seed_pool partial-fail is resumable, a retry completes cleanly --
// The saga debits the token deposit to DCL (leg 4) BEFORE add_liquidity (leg 5). We arm the mock to
// trap leg 5 AFTER the deposit lands, then disarm and RETRY. The fix records per-leg progress in a
// `seed_stage` sub-state, so the retry skips the non-idempotent create+deposit legs (which would
// otherwise re-trap / double-move) and resumes at add_liquidity — landing in Pooled with the tokens
// in the locked position and NOTHING stranded in the router's DCL internal balance.
#[tokio::test]
async fn medium2_seed_pool_partial_fail_then_retry_completes() -> anyhow::Result<()> {
    let w = setup().await?;
    let creator = w.worker.dev_create_account().await?;
    let ft = w
        .register_launch_seeded("cat.test.near", &creator, 1_000_000, 1_000_000, 500_000, (7000, 2000, 1000))
        .await?;
    w.go_live(ft.id()).await?;
    let trader = w.worker.dev_create_account().await?;
    w.give_wnear(&trader, 1_000_000).await?;
    assert!(w.buy(&trader, ft.id(), 700_000, 0, false).await?.is_success());
    w.owner.call(w.router.id(), "graduate").args_json(json!({ "token_id": ft.id() })).max_gas().transact().await?.into_result()?;
    let migrating = u128s(&w.launch(ft.id()).await?, "token_reserve");
    assert_eq!(w.ft_balance(ft.id(), w.router.id()).await?, migrating, "router holds the reserve pre-seed");

    // --- Attempt 1: arm leg 5 to trap. The deposit (leg 4) lands first, so tokens leave the router. ---
    w.dcl.call("mock_set_fail_add_liquidity").args_json(json!({ "fail": true })).max_gas().transact().await?.into_result()?;
    let _ = w
        .owner
        .call(w.router.id(), "seed_pool")
        .args_json(json!({ "token_id": ft.id() }))
        .max_gas()
        .transact()
        .await?;

    // Router reverted to Graduated and PRESERVED its progress (seed_stage == Deposited) for the retry.
    let l = w.launch(ft.id()).await?;
    assert_eq!(l["phase"], "Graduated", "phase reverted Pooling -> Graduated");
    assert_eq!(l["seed_stage"], "Deposited", "progress recorded: pool created + tokens deposited");
    assert_eq!(u128s(&l, "token_reserve"), migrating, "reserve bookkeeping still counts the parked tokens");
    assert_eq!(w.ft_balance(ft.id(), w.router.id()).await?, 0, "tokens moved into DCL's internal balance");

    // Mid-failure snapshot: tokens parked in the router's DCL internal balance; no LP minted yet.
    let stranded = w.dcl.view("list_user_assets").args_json(json!({ "account_id": w.router.id() })).await?.json::<Vec<(String, String)>>()?;
    assert_eq!(stranded.iter().find(|(t, _)| t == ft.id().as_str()).map(|(_, a)| a.parse::<u128>().unwrap()), Some(migrating), "deposit parked in DCL");
    let pool = w.dcl.view("get_pool").args_json(json!({ "pool_id": format!("{}|{}|2000", ft.id(), WNEAR) })).await?.json::<Value>()?;
    assert_eq!(u128s(&pool, "total_x"), 0, "no liquidity added yet");
    assert!(w.dcl.view("get_liquidity").args_json(json!({ "lpt_id": "1" })).await?.json::<Option<Value>>()?.is_none(), "no LP-NFT minted yet");

    // --- Attempt 2 (the fix): disarm the trap and retry. The saga resumes from Deposited: it skips
    //     create_pool (would re-trap "pool already exists") and the deposit (would double-move), and
    //     runs only add_liquidity, which consumes the already-parked tokens. ---
    w.dcl.call("mock_set_fail_add_liquidity").args_json(json!({ "fail": false })).max_gas().transact().await?.into_result()?;
    w.owner
        .call(w.router.id(), "seed_pool")
        .args_json(json!({ "token_id": ft.id() }))
        .max_gas()
        .transact()
        .await?
        .into_result()?;

    // The retry completed cleanly to Pooled with the reserve cleared into the locked position.
    let l = w.launch(ft.id()).await?;
    assert_eq!(l["phase"], "Pooled", "retry resumed and completed the migration");
    assert_eq!(u128s(&l, "token_reserve"), 0, "reserve cleared — tokens now in the locked DCL position");

    // Nothing stranded: add_liquidity consumed the parked deposit (no double-deposit, no leftover).
    let after = w.dcl.view("list_user_assets").args_json(json!({ "account_id": w.router.id() })).await?.json::<Vec<(String, String)>>()?;
    let left = after.iter().find(|(t, _)| t == ft.id().as_str()).map(|(_, a)| a.parse::<u128>().unwrap()).unwrap_or(0);
    assert_eq!(left, 0, "zero stranded: the entire deposit was consumed by add_liquidity");

    // The pool now holds exactly the migrated amount, and the LP position is minted+locked to the router.
    let pool = w.dcl.view("get_pool").args_json(json!({ "pool_id": format!("{}|{}|2000", ft.id(), WNEAR) })).await?.json::<Value>()?;
    assert_eq!(u128s(&pool, "total_x"), migrating, "full reserve added as liquidity (no double-create)");
    let lp = w.dcl.view("get_liquidity").args_json(json!({ "lpt_id": "1" })).await?.json::<Option<Value>>()?.expect("LP-NFT minted");
    assert_eq!(lp["owner_id"], w.router.id().to_string(), "LP position locked to the router");
    assert_eq!(u128s(&lp, "amount_x"), migrating, "LP position holds the full migrated reserve");
    Ok(())
}

// ---- Regression: pause gates TRADES, but holders can still CLAIM their rewards ----
// Staking/unstaking is gone (B1). The natural "exit still works while paused" property is now that
// accrued rewards live in the launch FT, custodied independently of the router — so a holder can
// claim regardless of the router's circuit breaker.
#[tokio::test]
async fn pause_blocks_trades_allows_reward_claims() -> anyhow::Result<()> {
    let w = setup().await?;
    let creator = w.worker.dev_create_account().await?;
    let ft = w
        .register_launch_seeded("cat.test.near", &creator, 10_000_000, 10_000_000, u128::MAX >> 4, (2000, 1000, 7000))
        .await?;
    w.go_live(ft.id()).await?;
    let trader = w.worker.dev_create_account().await?;
    w.give_wnear(&trader, 5_000_000).await?;
    assert!(w.buy(&trader, ft.id(), 3_000_000, 0, false).await?.is_success());

    // Flush the accrued holder bucket into the FT so the trader has claimable rewards, THEN pause.
    let rando = w.worker.dev_create_account().await?;
    w.distribute(&rando, ft.id()).await?;
    let pending = w.ft_rewards(ft.id(), trader.id()).await?;
    assert!(pending > 0, "trader accrued rewards by holding");

    // Owner trips the circuit breaker.
    w.owner.call(w.router.id(), "set_paused").args_json(json!({ "paused": true })).max_gas().transact().await?.into_result()?;
    assert!(w.router.view("is_paused").await?.json::<bool>()?);

    // Buy blocked: refunded, nothing delivered, curve untouched.
    let buyer = w.worker.dev_create_account().await?;
    w.give_wnear(&buyer, 500_000).await?;
    let real_before = u128s(&w.launch(ft.id()).await?, "real_near");
    let _ = w.buy(&buyer, ft.id(), 500_000, 0, false).await?;
    assert_eq!(w.ft_balance(ft.id(), buyer.id()).await?, 0, "buy blocked while paused");
    assert_eq!(w.ft_balance(w.wnear.id(), buyer.id()).await?, 500_000, "buyer refunded while paused");
    assert_eq!(u128s(&w.launch(ft.id()).await?, "real_near"), real_before, "curve untouched while paused");

    // Reward claim still works: rewards are custodied by the FT, not gated by the router's breaker.
    let before = w.ft_balance(w.wnear.id(), trader.id()).await?;
    w.claim_rewards(&trader, ft.id()).await?;
    let after = w.ft_balance(w.wnear.id(), trader.id()).await?;
    assert_eq!(after - before, pending, "holder claimed rewards from the FT while the router was paused");
    Ok(())
}

// ---- Coverage 5: King-of-the-Hill ranks Live launches by bonding progress ----
#[tokio::test]
async fn king_of_the_hill_ranks_by_progress() -> anyhow::Result<()> {
    let w = setup().await?;
    let creator = w.worker.dev_create_account().await?;
    let cat = w
        .register_launch_seeded("cat.test.near", &creator, 10_000_000, 10_000_000, 10_000_000, (7000, 2000, 1000))
        .await?;
    let dog = w
        .register_launch_seeded("dog.test.near", &creator, 10_000_000, 10_000_000, 10_000_000, (7000, 2000, 1000))
        .await?;
    w.go_live(cat.id()).await?;
    w.go_live(dog.id()).await?;

    let trader = w.worker.dev_create_account().await?;
    w.give_wnear(&trader, 3_000_000).await?;
    assert!(w.buy(&trader, cat.id(), 2_000_000, 0, false).await?.is_success());
    assert!(w.buy(&trader, dog.id(), 500_000, 0, false).await?.is_success());

    // cat has more bonding progress -> it is the King and leads the board.
    let king = w.router.view("get_king").await?.json::<Value>()?;
    assert_eq!(king["token_id"], cat.id().to_string(), "cat is King (more progress)");
    let board = w.router.view("get_king_of_the_hill").args_json(json!({ "limit": 10 })).await?.json::<Vec<Value>>()?;
    assert_eq!(board.len(), 2, "both Live launches on the board");
    assert_eq!(board[0]["token_id"], cat.id().to_string(), "cat ranked first");
    assert_eq!(board[1]["token_id"], dog.id().to_string(), "dog ranked second");
    assert!(board[0]["progress_bps"].as_u64().unwrap() > board[1]["progress_bps"].as_u64().unwrap());
    Ok(())
}

// ---- Coverage 1 (input leg): create_launch validates its inputs BEFORE the (untestable) deploy ----
// The global-contract deploy leg can't resolve in-sandbox (near-workspaces 0.23 can't publish a
// global contract — see the header note), but every input guard runs synchronously first, so the
// rejection paths are fully exercised here.
#[tokio::test]
async fn create_launch_input_validation() -> anyhow::Result<()> {
    let w = setup().await?;
    let creator = w.worker.dev_create_account().await?;
    let base = |name: &str, sym: &str, c: u16, b: u16, h: u16| {
        json!({ "name": name, "symbol": sym, "tier": "Economy", "virtual_near": "1000000000000000000000000",
                "creator_bps": c, "buyback_bps": b, "holder_bps": h, "socials": null })
    };
    let call = |args: Value, dep: NearToken| {
        let c = creator.clone();
        let router = w.router.id().clone();
        async move { c.call(&router, "create_launch").args_json(args).deposit(dep).max_gas().transact().await }
    };

    // Fee split that doesn't total 100% is rejected (checked first, before the deploy).
    assert!(call(base("Good Name", "TCAT", 5000, 2000, 1000), millinear(180)).await?.into_result().is_err(), "bad split");
    // Name longer than 16 chars is rejected.
    assert!(call(base("ThisNameIsWayTooLong", "TCAT", 7000, 2000, 1000), millinear(180)).await?.into_result().is_err(), "name > 16");
    // Ticker outside 3..=7 alphanumerics is rejected.
    assert!(call(base("Good Name", "AB", 7000, 2000, 1000), millinear(180)).await?.into_result().is_err(), "ticker too short");
    // Under-paying the tier launch fee (Economy = 0.18 NEAR) is rejected.
    assert!(call(base("Good Name", "TCAT", 7000, 2000, 1000), millinear(100)).await?.into_result().is_err(), "insufficient fee");
    Ok(())
}
