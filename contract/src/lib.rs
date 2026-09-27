// FastLaunch router — pump.fun-style bonding-curve engine.
//
// The router HOLDS each launch's on-curve token supply and trades it against a constant-product
// virtual-reserve curve (see `curve.rs`). No external DEX is in the buy/sell path, so delivery is
// a single atomic ft_transfer of tokens the router already owns — the exact amount is known
// in-call. This replaces the earlier "route the buy through a pre-seeded DCL pool" design, whose
// delivery leg could not know amount_out, raced on balance-diff, and could strand a buyer's wNEAR.
//
// Launch lifecycle:
//   register_launch → Seeding → (seed supply, optional dev buy ≤4%) → go_live → Live → Graduated
//     → seed_pool → Pooled
//   * Seeding: the on-curve supply is deposited into the router (ft_transfer_call, msg "seed");
//     the creator may take an optional pre-launch dev buy, capped on-chain at 4% of supply.
//   * Live: public buys (wNEAR in → tokens out) and sells (tokens in → wNEAR out) run the curve.
//   * Graduated: once real_near hits the launch's threshold, anyone may `graduate` to permanently
//     close the curve; the leftover tokens + real wNEAR stay held by the router as the earmarked
//     pool seed. `graduate` moves NO funds — it only closes the curve.
//   * Pooled: the owner-operated `seed_pool` migrates the leftover tokens into a permanent LOCKED
//     single-sided Rhea/Ref DCL position (LP NFT held by the router, which has no remove path).
//     The DCL ABI is verified against the live contract; deposit sizes need a testnet dry-run.
//
// Fees (LOCKED): a flat 1% trading fee is skimmed on every buy and sell, split 90/10 — 90% to the
// launch pool (creator/buyback/holder by the launch's immutable FeeSplit) and 10% to the protocol.
// Payouts use a zero-then-restore-on-failure callback so a failed delivery never charges the fee,
// drains the curve, or strands funds. The virtual ("shadow") reserve only anchors price and is
// never withdrawable: a sell can never pay out more real wNEAR than buyers actually put in. All
// fee/curve bookkeeping is in wNEAR yocto. 24h volume + fees are derived off-chain from the
// structured buy/sell/claim events + the per-launch cumulative counters.
use near_sdk::json_types::U128;
use near_sdk::serde_json::json;
use near_sdk::store::{IterableMap, LookupSet, Vector};
use near_sdk::{
    env, near, require, AccountId, BorshStorageKey, CryptoHash, Gas, NearToken, PanicOnDefault,
    Promise, PromiseError, PromiseOrValue,
};

mod curve;

pub type TokenId = AccountId;

/// wNEAR (testnet). The trading fee is always charged in wNEAR on both sides.
const WNEAR: &str = "wrap.testnet";
/// Flat trading fee in basis points. 100 = 1%.
const FEE_BPS: u128 = 100;
/// Dev/creator pre-launch buy cap, in bps of the on-curve supply. 400 = 4% (anti-rug).
const DEV_BUY_CAP_BPS: u128 = 400;
/// Gas for a single ft_transfer payout leg (wNEAR to a seller, or the launch token to a buyer).
const GAS_FOR_FT_TRANSFER: Gas = Gas::from_tgas(15);
/// Gas for a delivery-resolution callback (rolls the curve back if the payout/delivery failed).
const GAS_FOR_DELIVER_CB: Gas = Gas::from_tgas(15);
/// Gas for the claim resolution callback (restores the accrual if the payout failed).
const GAS_FOR_CLAIM_CB: Gas = Gas::from_tgas(15);

// ---- Factory: one NEP-141 launch token deployed per `create_launch` ----
// The router IS the factory. `create_launch` charges the tier launch fee (real transfer to the
// treasury), points a fresh subaccount at the launch-token code already published as a NEAR global
// contract (NEP-591) minting the whole supply to the router, then seeds the curve from that owned
// supply. Because the token id is always a factory-minted subaccount and `creator` is the caller,
// no one can squat an id to siphon another launch's fees.
/// SHA-256 code hash of the NEP-141 reward-bearing launch-token wasm (`contract-ft`,
/// res/fastlaunch_ft.wasm, 215473 bytes). The wasm is published ONCE as an immutable global-by-hash
/// contract; every launch subaccount references it via `use_global_contract` and pays only for its
/// own state — not the code. This is the whole reason a launch costs ~0.1 NEAR instead of ~3.4.
///
/// B1 (2026-09-27): the FT became reward-bearing (hold-to-earn), so its code — and thus this
/// by-hash id — CHANGED. The prior global (`9B5cmmBFpdXH3TTNBRzofcp9Swkb4fS5GW9KbUqc8WdD`, deployed
/// 2026-09-25) is superseded; the new FT must be REPUBLISHED as a global and this hash points at it.
/// Rebuild path: rebuild `contract-ft`, re-copy to `res/`, re-`deploy-as-global`, update these bytes.
const FT_GLOBAL_CODE_HASH: CryptoHash = [
    0x56, 0x9a, 0x25, 0xe3, 0x90, 0xb9, 0x03, 0xdb, 0xf8, 0x47, 0x56, 0x6a, 0x90, 0x27, 0xe9, 0x3f,
    0xc2, 0xfe, 0xe7, 0x80, 0xef, 0x3e, 0x2f, 0x8c, 0xb2, 0x27, 0x51, 0xbf, 0xd7, 0x57, 0xab, 0x57,
];
/// NEP-148 metadata spec string every launch token declares.
const FT_METADATA_SPEC: &str = "ft-1.0.0";
/// Launch-token decimals. 24 matches NEAR's native scale so curve math (token amount × wNEAR yocto)
/// stays in the u256-widened range the curve is overflow-tested against.
const TOKEN_DECIMALS: u8 = 24;
/// Fixed total supply per launch: 1,000,000,000 tokens at `TOKEN_DECIMALS`. The whole supply goes
/// on the curve (router-held); there is no team/presale allocation and no post-mint inflation.
const LAUNCH_SUPPLY: u128 = 1_000_000_000 * 1_000_000_000_000_000_000_000_000; // 1e9 × 1e24 = 1e33
// Virtual ("shadow") quote reserve, in wNEAR yocto, is NO LONGER a contract constant. Because 100%
// of supply sits on the curve, the opening market cap (FDV) equals `virtual_near` exactly — so the
// OPENING MCAP is set PER LAUNCH via the `virtual_near` argument the caller passes into
// `create_launch`/`register_launch`. The frontend converts the tier's target USD FDV (Basic $4k /
// Express $10k) to wNEAR at the live NEAR price and passes it in; the contract stays USD-agnostic.
/// Real-wNEAR threshold that opens `graduate`, in yocto. TIER-INDEPENDENT (identical for Basic and
/// Express). Documented in ECONOMICS.md; a contract-level constant (not a per-call arg) so
/// economics can't be invented ad hoc.
const LAUNCH_GRADUATION_NEAR: u128 = 5_000 * 1_000_000_000_000_000_000_000_000; // 5000 wNEAR
/// Tiered launch fee, attached by the creator on `create_launch`. The creator attaches ONLY the
/// tier fee (no separate deploy deposit on top); the router funds the token deploy out of it and
/// forwards the margin (`tier_fee − FT_DEPLOY_DEPOSIT`) to the treasury on success. Set by
/// Fee model - supersedes the earlier flat-0.18 fee and the stale 0.167/0.5 fee. Both tiers share ONE root, ONE factory, the same curve/graduation; the tier is a
/// FEE+PERKS flag, not a namespace. Express buys a bigger opening MCAP + priority queue + badge.
const BASIC_LAUNCH_FEE: NearToken = NearToken::from_millinear(180); // 0.18 NEAR
const EXPRESS_LAUNCH_FEE: NearToken = NearToken::from_millinear(350); // 0.35 NEAR
/// NEAR the router moves into the new subaccount to back the launch-token STATE storage staking,
/// funded out of the creator's tier fee (NOT attached separately by the creator). With
/// `use_global_contract` the subaccount stores only its own state (metadata + the single router
/// balance entry) — the 320 KB of code lives in the global contract, paid once at global deploy —
/// so the real floor is a few thousandths of a NEAR (sandbox-measured ~0.00318, icon-free).
///
/// TESTNET-GATED: this is the single knob. It stays **0.03 NEAR on testnet**;
/// only after a REAL testnet icon-free storage measurement confirms the floor do we tighten it to
/// the ~0.006 mainnet target (≈1.7× the measured floor). Sandbox floor ≠ testnet-confirmed, and
/// under-provisioning makes launches FAIL (not just lose margin), so the pad stays until measured.
/// Icon is off-chain (`icon:None`), so the icon-free floor is the correct basis. Any unused portion
/// stays as the (keyless, non-reclaimable) token account's own balance — no reclaim path by design.
const FT_DEPLOY_DEPOSIT: NearToken = NearToken::from_millinear(30); // 0.03 testnet; mainnet target 0.006 after measure
/// Gas for the launch-token `new` init inside the create+deploy batch.
const GAS_FOR_FT_INIT: Gas = Gas::from_tgas(50);
/// Gas for the post-deploy callback (records the launch, fires the fee transfer, seeds the curve).
const GAS_FOR_DEPLOYED_CB: Gas = Gas::from_tgas(40);

// ---- Rhea/Ref DCL (discretized concentrated liquidity) — graduation pool seeding ----
// ABI verified 2026-09-23 against the LIVE deployed contract dclv2.ref-labs.near (v2.3.13), by
// reading its WASM exports + probing arg names/types via read-only calls:
//   create_pool(token_a: AccountId, token_b: AccountId, fee: u32, init_point: i32)
//   add_liquidity(pool_id: String, left_point: i32, right_point: i32,
//                 amount_x: U128, amount_y: U128, min_amount_x: U128, min_amount_y: U128) -> lpt_id
//   pool_id string == "{token_x}|{token_y}|{fee}"; storage_balance_bounds.min == 0.5 NEAR.
// Tokens enter DCL via NEP-141 ft_transfer_call (empty msg == plain deposit to the caller's DCL
// internal balance); add_liquidity then draws from that balance and mints an LP position NFT to
// the caller. The router keeps that NFT and exposes NO remove/transfer path → liquidity is locked.
//
// STORAGE MODEL (verified 2026-09-26 against the live deployed WASM, dclv2 v2.3.13): DCL is
// slot-based. `storage_balance_bounds.min` = 0.5 NEAR (bare account registration), each deposited
// token asset costs `storage_for_asset` = 0.1 NEAR, and each liquidity position consumes one
// liquidity slot at `slot_price` = 0.01 NEAR. CRITICAL: the storage_deposit MUST NOT pass
// `registration_only: true` — that refunds everything above the 0.5 min, leaving no headroom to
// open the launch-token asset entry, so the empty-msg token deposit silently refunds and the
// internal balance stays 0 (the 26d failure). We register with a plain storage_deposit sized to
// cover base + one token asset + one liquidity slot. `add_liquidity` is NON-payable (attaching any
// deposit aborts with "Method add_liquidity doesn't accept deposit"); its position storage is drawn
// from this pre-paid balance, so the add leg must attach 0.
/// DCL contract (testnet id; mainnet is dclv2.ref-labs.near). USD/DEX-agnostic elsewhere in code.
const DCL: &str = "dclv2.ref-dev.testnet";
/// DCL fee tier for the graduated pool. 2000 == 0.2% (point_delta 40) — a live-confirmed tier.
const DCL_FEE: u32 = 2000;
/// Point spacing for DCL_FEE. Every init/left/right point must be a multiple of this.
const DCL_POINT_DELTA: i32 = 40;
/// Width (in DCL points) of the single-sided seed position, measured from one `point_delta` off spot
/// outward. Multiple of `DCL_POINT_DELTA`. Fixed on-chain (was implicitly caller-chosen pre-MEDIUM-1);
/// only the CENTER (`init_point`) is derived from the graduation price. 4000 points ≈ a 1.49× span,
/// matching the geometry proven live on testnet (2026-09-26e).
const DCL_RANGE_WIDTH: i32 = 4000;
/// Headroom divisor for the `add_liquidity` request: we DEPOSIT the full reserve but REQUEST only
/// `reserve − reserve/DCL_ADD_HEADROOM_DIVISOR`. DCL's single-sided liquidity math rounds the token
/// it charges UP, so `paid_token` can slightly exceed the requested `amount`; if we request the full
/// deposited reserve there is no surplus on our DCL internal balance for that round-up to draw from,
/// and DCL's internal-balance subtraction underflows (dcl_liquidity_api.rs:208 — the leg-7 blocker
/// found in the 2026-09-27 live E2E). Leaving `reserve/1e9` unrequested feeds the round-up from the
/// surplus instead. The live overflow was ~6.996e11 yocto on a ~7.27e32 reserve (~1e-21 relative);
/// a 1/1e9 buffer (~1e-9 relative) clears it by ~12 orders of magnitude yet is sub-token dust — and
/// since a graduated reserve is always a large fraction of LAUNCH_SUPPLY (1e33), the proportional
/// term is always ≫ any plausible round-up. The unused dust (buffer − round-up) stays on the DCL
/// internal balance, recoverable via `withdraw_asset`. Proportional (not flat) so the margin holds
/// across every reserve/price scale.
const DCL_ADD_HEADROOM_DIVISOR: u128 = 1_000_000_000;
/// Plain storage_deposit (NO registration_only) sized to cover DCL's 0.5 NEAR base registration +
/// one token asset (0.1) + one liquidity slot (0.01) with buffer. Sent as leg 1 of every seed_pool;
/// on repeat calls the surplus accumulates as usable DCL storage balance for future positions.
const DCL_STORAGE_DEPOSIT: NearToken = NearToken::from_yoctonear(620_000_000_000_000_000_000_000);
/// NEAR attached to create_pool (pool storage; DCL refunds excess). Conservative — verify on testnet.
const DCL_CREATE_POOL_DEPOSIT: NearToken = NearToken::from_yoctonear(100_000_000_000_000_000_000_000);
/// add_liquidity is NON-payable on live DCL — attaching ANY deposit (even 1 yocto) aborts with
/// "Method add_liquidity doesn't accept deposit". Position storage is drawn from the pre-paid
/// NEP-145 storage balance provisioned by the DCL_STORAGE_DEPOSIT leg, so this leg attaches 0.
const DCL_ADD_LIQUIDITY_DEPOSIT: NearToken = NearToken::from_yoctonear(0);
/// NEP-145 registration deposit for DCL on the launch token's FT (our FT's storage_balance_bounds
/// min == max == 0.00125 NEAR). `ft_transfer_call(token → DCL)` transfers into DCL's balance ON the
/// FT first, which panics ("receiver not registered") unless DCL is registered there — so the saga
/// registers DCL on the launch FT before depositing (the 26d/first-26e deposit trap).
const FT_STORAGE_DEPOSIT: NearToken = NearToken::from_yoctonear(1_250_000_000_000_000_000_000);
/// Gas for each leg of the pool-seeding saga (sequential .then chain, must sum well under 300 TGas).
const GAS_FOR_FT_STORAGE: Gas = Gas::from_tgas(5);
/// Gas for `add_excluded` on the launch FT (excludes the DCL pool from holder rewards at seed time).
const GAS_FOR_FT_EXCLUDE: Gas = Gas::from_tgas(10);
const GAS_FOR_DCL_STORAGE: Gas = Gas::from_tgas(10);
const GAS_FOR_DCL_CREATE_POOL: Gas = Gas::from_tgas(30);
const GAS_FOR_DCL_DEPOSIT: Gas = Gas::from_tgas(60);
const GAS_FOR_DCL_ADD_LIQUIDITY: Gas = Gas::from_tgas(80);
const GAS_FOR_POOL_SEEDED_CB: Gas = Gas::from_tgas(20);
/// Gas for the tiny per-leg progress callbacks (`on_seed_create`/`on_seed_deposit`) that record the
/// resumable `SeedStage` so a failed saga can be retried without re-running its non-idempotent legs.
const GAS_FOR_SEED_PROGRESS_CB: Gas = Gas::from_tgas(10);

// ---- Holder rewards (forwarded to the reward-bearing launch FT) ----
/// Gas for the wNEAR `storage_deposit` registering the FT as a wNEAR holder (lazy, first distribute).
const GAS_FOR_FT_WNEAR_REG: Gas = Gas::from_tgas(10);
/// Gas for the wNEAR `ft_transfer_call(token_id, amount, "reward")` that forwards the holder bucket
/// to the launch FT (must cover the FT's `ft_on_transfer` reward-intake accounting) + its callback.
const GAS_FOR_REWARD_FORWARD: Gas = Gas::from_tgas(60);
const GAS_FOR_FORWARD_CB: Gas = Gas::from_tgas(10);
/// Deposit to register the FT on wNEAR (NEP-145 storage). Same as other FT registrations.
const WNEAR_REG_DEPOSIT: NearToken = NearToken::from_millinear(2);

// ---- Express Premium Board (on-chain FIFO featured queue) ----
/// How many express entries are `Active` (featured on the Premium Board) at once. FIFO.
const EXPRESS_BOARD_SLOTS: u64 = 3;
/// How long an express entry stays `Active`, in nanoseconds. 30 minutes.
const EXPRESS_ACTIVE_DURATION_NS: u64 = 30 * 60 * 1_000_000_000;


/// Community links the creator enters at launch. Any leg may be unset.
#[near(serializers = [borsh, json])]
#[derive(Clone, Default)]
pub struct Socials {
    pub x: Option<String>,
    pub telegram: Option<String>,
    pub discord: Option<String>,
    pub website: Option<String>,
}

/// Split of the creator's 90% pool. creator_bps + buyback_bps + holder_bps == 10000.
/// Set ONCE at launch and immutable thereafter — buyers can trust it.
#[near(serializers = [borsh, json])]
#[derive(Clone)]
pub struct FeeSplit {
    pub creator_bps: u16,
    pub buyback_bps: u16,
    pub holder_bps: u16,
}

/// Booked-but-unspent fees for one launch, in wNEAR yocto. The 90% pool cut of every buy is
/// split here by the launch's immutable FeeSplit. `creator` is claimable as cash (1C);
/// `buyback` funds buyback&burn (1D); `holder` funds holder rewards (1E).
#[near(serializers = [borsh, json])]
#[derive(Clone)]
pub struct Accrual {
    pub creator: U128,
    pub buyback: U128,
    pub holder: U128,
}

impl Accrual {
    fn zero() -> Self {
        Self { creator: U128(0), buyback: U128(0), holder: U128(0) }
    }
}

/// State of an entry on the Express Premium Board FIFO.
#[near(serializers = [borsh, json])]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ExpressState {
    /// Waiting in line for a free Active slot.
    Queued,
    /// Currently featured on the Premium Board (for `EXPRESS_ACTIVE_DURATION_NS`).
    Active,
    /// Its 30-minute Active window elapsed.
    Expired,
    /// Creator/owner pulled it before it was ever activated.
    Cancelled,
}

/// One Express launch's slot on the Premium Board FIFO. Lives in contract storage, so the whole
/// queue survives contract restarts/redeploys. `position` is a monotonic enqueue index — the
/// promotion order is strictly by `position` (true FIFO).
#[near(serializers = [borsh, json])]
#[derive(Clone)]
pub struct ExpressEntry {
    pub token_id: TokenId,
    pub state: ExpressState,
    pub enqueued_at: u64,
    pub activated_at: Option<u64>,
    pub position: u64,
}

/// A King-of-the-Hill leaderboard row: a Live launch ranked by bonding progress. View-only.
#[near(serializers = [json])]
#[derive(Clone)]
pub struct KingEntry {
    pub token_id: TokenId,
    pub name: String,
    pub symbol: String,
    pub real_near: U128,
    pub graduation_near: U128,
    /// Bonding progress in basis points: `real_near * 10000 / graduation_near`, capped at 10000.
    pub progress_bps: u32,
    pub created_at: u64,
}


/// Launch tier. Sets the starting virtual reserve + launch price (Express > Economy) and which
/// featured surface the token shows on. The wNEAR value of each tier's virtual reserve is passed
/// in at registration (the contract stays USD-agnostic); Economy ≈ $4k, Express ≈ $10k.
#[near(serializers = [borsh, json])]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tier {
    Economy,
    Express,
}

/// Where a launch is in its lifecycle. Trading is only open in `Live`.
#[near(serializers = [borsh, json])]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    /// Registered; supply being deposited + optional pre-launch dev buy. Public trading closed.
    Seeding,
    /// Public buys and sells run the curve.
    Live,
    /// Threshold reached — the curve is permanently closed (no more buys/sells). The leftover
    /// on-curve tokens + all accumulated real wNEAR are held by the router, earmarked as the seed
    /// for the permanent locked DCL pool. Awaiting `seed_pool` (the DCL migration step).
    Graduated,
    /// The DCL pool-seeding saga (storage_deposit → create_pool → deposit → add_liquidity) is in
    /// flight. Acts as a re-entry lock; `on_pool_seeded` moves this to `Pooled` on success or back
    /// to `Graduated` (retryable) on failure.
    Pooling,
    /// Liquidity seeded: the launch's leftover tokens are a locked single-sided DCL position (LP
    /// NFT held by the router, which exposes no remove/transfer path). Terminal.
    Pooled,
}

/// Resumable progress within the `seed_pool` DCL migration saga. The saga's early legs
/// (`create_pool`, token `deposit`) have side effects on DCL that a naive retry cannot repeat —
/// `create_pool` traps on an existing pool and a second `deposit` would double-move the tokens.
/// So each successful leg is recorded here (via its own callback) and a retry of `seed_pool` fires
/// ONLY the legs not yet done, resuming at `add_liquidity`. Ordered: a stage only advances by one
/// on a confirmed leg, and `Deposited` is never recorded unless `PoolCreated` already is.
#[near(serializers = [borsh, json])]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum SeedStage {
    /// Nothing done — a first attempt runs the full saga.
    NotStarted,
    /// `create_pool` confirmed: the DCL pool exists. A retry must NOT re-create it.
    PoolCreated,
    /// The launch tokens are confirmed in the router's DCL internal balance. A retry must NOT
    /// re-transfer them; it resumes directly at `add_liquidity`.
    Deposited,
}

#[near(serializers = [borsh, json])]
#[derive(Clone)]
pub struct LaunchInfo {
    pub token_id: TokenId,
    pub creator: AccountId,
    pub name: String,
    pub symbol: String,
    pub tier: Tier,
    pub phase: Phase,
    pub split: FeeSplit,
    pub socials: Socials,
    /// Virtual quote reserve in wNEAR yocto — the "shadow" liquidity that anchors the starting
    /// price + depth. Never real, never withdrawable.
    pub virtual_near: U128,
    /// Real wNEAR accumulated on the curve (buys add, sells remove), yocto. Sell payouts are
    /// capped by this: the virtual reserve is not real liquidity.
    pub real_near: U128,
    /// On-curve tokens still unsold (the curve's base reserve). Starts at the seeded supply.
    pub token_reserve: U128,
    /// Total token supply placed on the curve at seeding. Basis for the dev-buy cap + graduation.
    pub on_curve_supply: U128,
    /// Real-wNEAR threshold that opens graduation. Once `real_near` >= this, anyone may call
    /// `graduate` to permanently close the curve and migrate to the locked DCL pool. Tier-dependent
    /// (Express graduates higher than Economy), set once at registration.
    pub graduation_near: U128,
    /// Tokens the creator has taken via the pre-launch dev buy so far (≤ 4% of on_curve_supply).
    pub dev_bought: U128,
    /// Lifetime trading fees routed to this launch's pool, in wNEAR yocto.
    pub fees_collected: U128,
    /// Cumulative REAL wNEAR traded (gross buys + gross sells), yocto. 24h windows derived
    /// off-chain — never counts the virtual/shadow reserve.
    pub volume: U128,
    /// Launch tokens bought back off the curve and permanently LOCKED in the router (no move
    /// path). Effectively burned — removed from circulation, never re-enters trading or the DCL seed.
    pub burned: U128,
    /// Resumable progress of the `seed_pool` DCL migration. `NotStarted` until the first attempt;
    /// advanced by the saga's per-leg callbacks so a retry skips the non-idempotent early legs.
    pub seed_stage: SeedStage,
    pub created_at: u64,
}

/// Buy intent the router expects in the `msg` of a wNEAR `ft_transfer_call`. `dev_buy` marks the
/// creator's pre-launch buy (Seeding phase, cap-checked); a normal buy leaves it false.
#[near(serializers = [json])]
pub struct WnearMsg {
    pub token_id: TokenId,
    pub min_out: U128,
    pub dev_buy: bool,
}

/// Intent the router expects in the `msg` of a launch-token `ft_transfer_call`. `action` is
/// "seed" (deposit the on-curve supply, Seeding phase) or "sell" (sell tokens back for wNEAR).
#[near(serializers = [json])]
pub struct TokenMsg {
    pub action: String,
    pub min_out: U128,
}

#[near(serializers = [borsh])]
#[derive(BorshStorageKey)]
enum StorageKey {
    Launches,
    Accruals,
    ExpressQueue,
    TakenIds,
}

#[near(contract_state)]
#[derive(PanicOnDefault)]
pub struct Contract {
    /// Protocol owner (admin: registers legacy launches, operates seed_pool, claims protocol fees).
    pub owner_id: AccountId,
    /// FastLaunchpad Developer/Protocol Treasury — receives every launch fee via a real transfer.
    pub treasury_id: AccountId,
    /// Flat protocol cut of the 1% trading fee, in bps. 1000 = 10% (the "90/10").
    pub protocol_bps: u16,
    /// Protocol's accrued share of trading fees (wNEAR yocto), claimable by the owner.
    pub protocol_fees: U128,
    /// Monotonic counter used to mint a unique token subaccount id per `create_launch`.
    pub launch_seq: u64,
    /// Emergency circuit breaker. When true, buys/sells (and buyback) are blocked; claims,
    /// holder-reward distribution and every fund-returning path stay open. Owner-toggled; grants NO fund access.
    pub paused: bool,
    /// Monotonic enqueue index for the Express Premium Board (defines FIFO order).
    pub express_seq: u64,
    pub launches: IterableMap<TokenId, LaunchInfo>,
    /// Per-launch booked-but-unspent fees, split into creator/buyback/holder buckets.
    pub accruals: IterableMap<TokenId, Accrual>,
    /// The Express Premium Board FIFO (all entries ever enqueued; state machine per entry).
    pub express_queue: Vector<ExpressEntry>,
    /// Every token account id ever reserved by `create_launch` (across both tiers — one factory,
    /// one namespace). Drives the FCFS `-N` collision suffix: an id is reserved the instant it is
    /// minted and only freed if the deploy fails. Account ids are globally unique here; the
    /// displayed ticker/symbol is decoupled and may duplicate freely.
    pub taken_ids: LookupSet<AccountId>,
}

/// Emit a structured NEP-297-style event log. The off-chain read model derives 24h volume and
/// 24h fees claimed from these `buy`/`sell`/`claim` events (real amounts only, never the virtual
/// reserve), alongside the per-launch cumulative counters.
fn emit(event: &str, data: near_sdk::serde_json::Value) {
    env::log_str(
        &json!({ "standard": "fastlaunch", "version": "1.0.0", "event": event, "data": data })
            .to_string(),
    );
}

/// Derive the DCL opening tick (`init_point`) from the graduation price ON-CHAIN, deterministically —
/// no owner discretion (MEDIUM-1). The pool must open at exactly the curve's closing price so there is
/// no arb gap: `price = wNEAR per launch token = real_near / token_reserve`. Both sides are 24-decimal
/// (launch FT `TOKEN_DECIMALS` == wNEAR's 24), so the raw yocto ratio IS the human price — no decimal
/// rescale (if that ever diverges, a `10^(dec_x - dec_y)` factor would be needed here).
///
/// DCL defines `price(token_y per token_x) = 1.0001^point`, so `point = log_1.0001(price_y_per_x)`.
/// `token_x` is the lexicographically smaller account id, so the price inverts when the launch token
/// sorts as `token_y` — handled by swapping numerator/denominator, not by negating. Aligned to
/// `DCL_POINT_DELTA` by rounding to the nearest multiple. Float `ln` is libm software float in wasm —
/// bit-deterministic across nodes — and the 40-point granularity needs only ~3 significant figures,
/// far inside f64 precision. Pinned by a unit test.
fn derive_init_point(real_near: u128, token_reserve: u128, launch_is_x: bool) -> i32 {
    // price of token_x expressed in token_y. token_x is whichever token sorts smaller.
    let (num, den) = if launch_is_x {
        (real_near, token_reserve) // x = launch, y = wNEAR -> wNEAR per launch token
    } else {
        (token_reserve, real_near) // x = wNEAR, y = launch -> launch tokens per wNEAR
    };
    let price = num as f64 / den as f64;
    let raw = price.ln() / 1.0001_f64.ln();
    ((raw / DCL_POINT_DELTA as f64).round() as i32) * DCL_POINT_DELTA
}

/// Token amount to REQUEST from DCL `add_liquidity`, given the full `amount` we deposited. We ask for
/// slightly less (`amount − amount/DCL_ADD_HEADROOM_DIVISOR`) so DCL's round-up on the charged token
/// is served from the on-balance surplus instead of underflowing our internal balance. The unused
/// dust stays recoverable via DCL `withdraw_asset`. See DCL_ADD_HEADROOM_DIVISOR for the sizing proof.
fn add_liquidity_request(amount: u128) -> u128 {
    amount.saturating_sub(amount / DCL_ADD_HEADROOM_DIVISOR)
}

#[near]
impl Contract {
    #[init]
    pub fn new(owner_id: AccountId, treasury_id: AccountId) -> Self {
        Self {
            owner_id,
            treasury_id,
            protocol_bps: 1000,
            protocol_fees: U128(0),
            launch_seq: 0,
            paused: false,
            express_seq: 0,
            launches: IterableMap::new(StorageKey::Launches),
            accruals: IterableMap::new(StorageKey::Accruals),
            express_queue: Vector::new(StorageKey::ExpressQueue),
            taken_ids: LookupSet::new(StorageKey::TakenIds),
        }
    }

    /// Record a launch's immutable parameters. **Owner-gated:** only the protocol owner (the
    /// launch orchestrator) may register, so nobody can squat a `token_id` and assign themselves
    /// as `creator` to siphon its routed fees. The fee split, tier, virtual reserve and on-curve
    /// supply are fixed here and can never be changed — there is deliberately no setter. A token
    /// can only be registered once. The launch opens in `Seeding`: the supply must still be
    /// deposited (msg "seed") and `go_live` called before public trading. `graduation_near` is the
    /// real-wNEAR threshold that later unlocks `graduate` (tier-dependent).
    pub fn register_launch(
        &mut self,
        token_id: TokenId,
        creator: AccountId,
        name: String,
        symbol: String,
        tier: Tier,
        virtual_near: U128,
        on_curve_supply: U128,
        graduation_near: U128,
        creator_bps: u16,
        buyback_bps: u16,
        holder_bps: u16,
        socials: Option<Socials>,
    ) {
        require!(
            env::predecessor_account_id() == self.owner_id,
            "only the owner can register launches"
        );
        self.record_launch(
            token_id, creator, name, symbol, tier, virtual_near, on_curve_supply, graduation_near,
            creator_bps, buyback_bps, holder_bps, socials,
        );
    }

    /// Shared launch-record writer used by both the owner-gated `register_launch` and the public
    /// factory `create_launch` (after it has minted the token subaccount). Enforces the immutable
    /// invariants — unique id, fee split == 100%, positive curve params — and opens the launch in
    /// `Seeding`. Callers are responsible for the trust boundary (owner check / factory-minted id).
    fn record_launch(
        &mut self,
        token_id: TokenId,
        creator: AccountId,
        name: String,
        symbol: String,
        tier: Tier,
        virtual_near: U128,
        on_curve_supply: U128,
        graduation_near: U128,
        creator_bps: u16,
        buyback_bps: u16,
        holder_bps: u16,
        socials: Option<Socials>,
    ) {
        assert!(
            !self.launches.contains_key(&token_id),
            "launch already registered"
        );
        assert_eq!(
            creator_bps as u32 + buyback_bps as u32 + holder_bps as u32,
            10_000,
            "fee split must total 100%"
        );
        require!(virtual_near.0 > 0, "virtual reserve must be positive");
        require!(on_curve_supply.0 > 0, "on-curve supply must be positive");
        require!(graduation_near.0 > 0, "graduation threshold must be positive");
        let info = LaunchInfo {
            token_id: token_id.clone(),
            creator,
            name,
            symbol,
            tier,
            phase: Phase::Seeding,
            split: FeeSplit { creator_bps, buyback_bps, holder_bps },
            socials: socials.unwrap_or_default(),
            virtual_near,
            real_near: U128(0),
            token_reserve: U128(0),
            on_curve_supply,
            graduation_near,
            dev_bought: U128(0),
            fees_collected: U128(0),
            volume: U128(0),
            burned: U128(0),
            seed_stage: SeedStage::NotStarted,
            created_at: env::block_timestamp(),
        };
        self.launches.insert(token_id, info);
    }

    // ---------- factory: public, fee-charging launch creation ----------

    /// The launch fee for a tier, as a real NEAR amount. The creator attaches exactly this on
    /// `create_launch`; the router funds the deploy out of it and forwards the margin to treasury.
    /// Basic 0.18 / Express 0.35.
    fn tier_fee(tier: Tier) -> NearToken {
        match tier {
            Tier::Economy => BASIC_LAUNCH_FEE,
            Tier::Express => EXPRESS_LAUNCH_FEE,
        }
    }

    /// Sanitize a user ticker into a NEAR-legal account label: lowercase, keep only `[a-z0-9-_]`.
    /// The ticker is already validated 3–7 ASCII-alphanumeric, so this is a defensive lowercasing
    /// that always yields a non-empty legal label. Used to derive the token ACCOUNT id from the
    /// ticker (the on-chain symbol keeps the user's exact casing and may duplicate freely).
    fn label_from_ticker(symbol: &str) -> String {
        symbol
            .chars()
            .filter_map(|c| {
                let c = c.to_ascii_lowercase();
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' { Some(c) } else { None }
            })
            .collect()
    }

    /// Reserve a globally-unique token account id under the single brand root
    /// (`<label>.<root>`), FCFS. If the base id is taken, append `-2`, `-3`, … to the ACCOUNT ID
    /// ONLY until a free id is found, then reserve it. Never rejects for a name/ticker clash — the
    /// displayed symbol is decoupled from the id and may duplicate freely.
    fn reserve_token_id(&mut self, label: &str) -> TokenId {
        let root = env::current_account_id();
        let mut n: u32 = 1;
        loop {
            let candidate = if n == 1 {
                format!("{}.{}", label, root)
            } else {
                format!("{}-{}.{}", label, n, root)
            };
            let id: TokenId = candidate.parse().expect("derived token id is not a valid account id");
            if !self.taken_ids.contains(&id) {
                self.taken_ids.insert(id.clone());
                return id;
            }
            n += 1;
        }
    }

    /// Create a launch end-to-end, signed and paid for by the creator (no owner involvement):
    /// deploys a fresh NEP-141 token subaccount minting the whole `LAUNCH_SUPPLY` to the router and
    /// seeds the bonding curve from that owned supply. The caller becomes the immutable `creator`.
    ///
    /// The token account id is `<label>.<root>` under the single brand root, where `label` is the
    /// lowercased ticker; on collision an `-N` suffix is appended to the ACCOUNT ID only (FCFS). The
    /// on-chain symbol keeps the user's exact casing and may duplicate freely. `icon` is intentionally
    /// off-chain — the token metadata is minted with `icon:None` (a keyless token can't be updated,
    /// so the mutable logo lives off-chain and is added via the dashboard / Ref token-list).
    ///
    /// `virtual_near` sets the launch's OPENING MCAP/FDV (100% of supply is on the curve, so opening
    /// FDV == virtual_near exactly). The frontend converts the tier's target USD FDV (Basic $4k /
    /// Express $10k) to wNEAR at the live NEAR price and passes it here; the contract stays
    /// USD-agnostic. Express additionally gets a priority queue spot + the Premium Board badge.
    ///
    /// Attach exactly the tier fee (Basic 0.18 / Express 0.35); any excess is refunded. The router
    /// funds the token deploy from that fee and, on deploy success (see `on_ft_deployed`), transfers
    /// the margin (`tier_fee − FT_DEPLOY_DEPOSIT`) to the treasury and records the launch. A failed
    /// deploy charges nothing (the full fee is refunded, the reserved id is freed) and leaves no
    /// half-created launch. The launch opens in `Seeding`; the creator then calls `go_live`.
    #[payable]
    pub fn create_launch(
        &mut self,
        name: String,
        symbol: String,
        tier: Tier,
        virtual_near: U128,
        creator_bps: u16,
        buyback_bps: u16,
        holder_bps: u16,
        socials: Option<Socials>,
    ) -> Promise {
        assert_eq!(
            creator_bps as u32 + buyback_bps as u32 + holder_bps as u32,
            10_000,
            "fee split must total 100%"
        );
        require!(!name.trim().is_empty() && name.len() <= 16, "name must be 1..=16 chars");
        require!(
            (3..=7).contains(&symbol.len()) && symbol.chars().all(|c| c.is_ascii_alphanumeric()),
            "ticker must be 3..=7 alphanumeric chars"
        );
        require!(virtual_near.0 > 0, "virtual_near (opening MCAP) must be > 0");

        let fee = Self::tier_fee(tier);
        let attached = env::attached_deposit();
        require!(attached >= fee, "attach the tier launch fee (Basic 0.18 / Express 0.35 NEAR)");
        let creator = env::predecessor_account_id();

        // Refund any overpayment above the tier fee up front (independent of the deploy outcome).
        // The creator attaches ONLY the fee — the deploy deposit is funded by the router from it.
        let refund = attached.checked_sub(fee).unwrap_or(NearToken::from_yoctonear(0));
        if refund > NearToken::from_yoctonear(0) {
            // Detached: near-sdk schedules a Promise as an independent receipt on drop.
            let _ = Promise::new(creator.clone()).transfer(refund);
        }

        // Reserve a globally-unique account id `<label>.<root>` (FCFS, `-N` on collision). The
        // launch_seq counter still advances as a monotonic launch tally.
        self.launch_seq += 1;
        let label = Self::label_from_ticker(&symbol);
        let token_id: TokenId = self.reserve_token_id(&label);

        // Icon is OFF-CHAIN by design: mint with `icon:null`. A keyless token
        // has no metadata-update method, so the logo must live off-chain to stay mutable post-launch.
        let init_args = json!({
            "owner_id": env::current_account_id(),
            "total_supply": U128(LAUNCH_SUPPLY),
            "metadata": {
                "spec": FT_METADATA_SPEC,
                "name": name,
                "symbol": symbol,
                "icon": null,
                "reference": null,
                "reference_hash": null,
                "decimals": TOKEN_DECIMALS,
            }
        })
        .to_string()
        .into_bytes();

        emit(
            "create_launch",
            json!({ "token_id": token_id, "creator": creator, "tier": tier,
                    "fee": U128(fee.as_yoctonear()), "virtual_near": virtual_near }),
        );

        // Batch: create the subaccount, fund its state storage (out of the fee), point it at the
        // global launch-token code, run its init. A batch is atomic — if init panics the whole
        // batch reverts and FT_DEPLOY_DEPOSIT returns to the router, which `on_ft_deployed` then
        // refunds (as the full fee) to the creator. `use_global_contract` references the code by
        // hash instead of shipping 320 KB per launch, so the subaccount only stakes for its state.
        Promise::new(token_id.clone())
            .create_account()
            .transfer(FT_DEPLOY_DEPOSIT)
            .use_global_contract(FT_GLOBAL_CODE_HASH)
            .function_call(
                "new".to_string(),
                init_args,
                NearToken::from_yoctonear(0),
                GAS_FOR_FT_INIT,
            )
            .then(
                Self::ext(env::current_account_id())
                    .with_static_gas(GAS_FOR_DEPLOYED_CB)
                    .on_ft_deployed(
                        token_id, creator, name, symbol, tier, virtual_near, creator_bps,
                        buyback_bps, holder_bps, socials, U128(fee.as_yoctonear()),
                    ),
            )
    }

    /// Resolve a `create_launch` token deploy. On success: record the launch (Seeding), transfer
    /// the protocol margin (`fee − FT_DEPLOY_DEPOSIT`) to the treasury, and seed the curve from the
    /// router-owned supply. On failure: the deploy batch already reverted (returning
    /// FT_DEPLOY_DEPOSIT to the router), so refund the creator the full `fee` and record nothing.
    #[private]
    pub fn on_ft_deployed(
        &mut self,
        token_id: TokenId,
        creator: AccountId,
        name: String,
        symbol: String,
        tier: Tier,
        virtual_near: U128,
        creator_bps: u16,
        buyback_bps: u16,
        holder_bps: u16,
        socials: Option<Socials>,
        fee: U128,
        #[callback_result] res: Result<(), PromiseError>,
    ) -> Option<AccountId> {
        if res.is_err() {
            // The deploy batch reverted, returning FT_DEPLOY_DEPOSIT to the router — so the router
            // holds the whole fee again. Refund the creator exactly the fee they attached, and free
            // the reserved account id so the ticker can be retried (FCFS reservation released).
            let _ = Promise::new(creator).transfer(NearToken::from_yoctonear(fee.0));
            self.taken_ids.remove(&token_id);
            emit("create_launch_failed", json!({ "token_id": token_id }));
            return None;
        }
        // Deploy succeeded: the router owns the whole supply on the new token contract.
        self.record_launch(
            token_id.clone(),
            creator,
            name,
            symbol,
            tier,
            virtual_near,
            U128(LAUNCH_SUPPLY),
            U128(LAUNCH_GRADUATION_NEAR),
            creator_bps,
            buyback_bps,
            holder_bps,
            socials,
        );
        // Seed the curve directly: the router already custodies `LAUNCH_SUPPLY` from the mint, so no
        // transfer is needed — this is the owned-supply counterpart of the `ft_on_transfer` "seed".
        let launch = self.launches.get_mut(&token_id).unwrap();
        launch.token_reserve = U128(LAUNCH_SUPPLY);
        // Protocol margin (fee minus the deploy deposit the router just spent) reaches the treasury
        // only now that the token exists (detached receipt, scheduled on drop). The deploy deposit
        // stayed with the (keyless, non-reclaimable) token account — no reclaim path by design.
        let margin = fee.0.saturating_sub(FT_DEPLOY_DEPOSIT.as_yoctonear());
        if margin > 0 {
            let _ = Promise::new(self.treasury_id.clone())
                .transfer(NearToken::from_yoctonear(margin));
        }
        emit("seed", json!({ "token_id": token_id, "supply": U128(LAUNCH_SUPPLY) }));
        // Express tier buys a spot on the on-chain Premium Board FIFO (visibility only — the curve,
        // price and supply are untouched). Enqueue on successful deploy and try an immediate promote.
        if tier == Tier::Express {
            self.enqueue_express(&token_id);
        }
        Some(token_id)
    }

    /// Open public trading once the curve is seeded. Owner or creator only; Seeding → Live.
    pub fn go_live(&mut self, token_id: TokenId) {
        let info = self.launches.get(&token_id).expect("unknown launch token");
        let caller = env::predecessor_account_id();
        require!(
            caller == self.owner_id || caller == info.creator,
            "only owner or creator can open trading"
        );
        require!(info.phase == Phase::Seeding, "launch is not in the seeding phase");
        require!(info.token_reserve.0 > 0, "seed the on-curve supply first");
        let launch = self.launches.get_mut(&token_id).unwrap();
        launch.phase = Phase::Live;
        emit("go_live", json!({ "token_id": token_id }));
    }

    /// Owner-only test-enablement knob: adjust a specific launch's graduation threshold before it
    /// graduates. The mainnet economics are unchanged — `create_launch` still seeds every launch at
    /// `LAUNCH_GRADUATION_NEAR` (5000 wNEAR); this only lets the owner retune a single launch (e.g.
    /// to exercise the full graduate → seed_pool path on testnet with faucet funds). Rejected once
    /// the launch has left the curve (Graduated/Pooling/Pooled), so it can never move the goalposts
    /// on an already-closed curve.
    pub fn set_graduation_near(&mut self, token_id: TokenId, graduation_near: U128) {
        require!(
            env::predecessor_account_id() == self.owner_id,
            "only the owner can set the graduation threshold"
        );
        require!(graduation_near.0 > 0, "graduation threshold must be positive");
        let launch = self.launches.get_mut(&token_id).expect("unknown launch token");
        require!(
            matches!(launch.phase, Phase::Seeding | Phase::Live),
            "launch has already left the curve"
        );
        launch.graduation_near = graduation_near;
        emit(
            "set_graduation",
            json!({ "token_id": token_id, "graduation_near": graduation_near }),
        );
    }

    // ---------- graduation (stage 1F) ----------

    /// Permanently close a sold-through curve. **Permissionless once the threshold is met:** any
    /// caller may trigger it the moment `real_near` reaches the launch's `graduation_near` (the
    /// frontend/indexer normally fires it) — the threshold gate is the only guard, so it can never
    /// close a curve that hasn't hit its target. Live → Graduated freezes all buys/sells (both
    /// require `Live`). The leftover on-curve tokens + all accumulated real wNEAR stay held by the
    /// router as the earmarked seed for the permanent locked DCL pool, and are reported in the
    /// `graduate` event as `pool_tokens` / `pool_near`.
    ///
    /// NOTE: `graduate` only closes the curve — it does NOT move any funds. The permanent locked
    /// DCL pool is seeded by the separate owner-operated `seed_pool` step (needs off-chain-computed
    /// price points), so the trustless, permissionless threshold-close is never coupled to the
    /// fund-moving migration. Funds remain safe in the router until `seed_pool` succeeds.
    pub fn graduate(&mut self, token_id: TokenId) -> LaunchInfo {
        let info = self.launches.get(&token_id).expect("unknown launch token").clone();
        require!(info.phase == Phase::Live, "launch is not live");
        require!(
            info.real_near.0 >= info.graduation_near.0,
            "graduation threshold not reached"
        );
        let launch = self.launches.get_mut(&token_id).unwrap();
        launch.phase = Phase::Graduated;
        emit(
            "graduate",
            json!({ "token_id": token_id, "pool_near": launch.real_near,
                    "pool_tokens": launch.token_reserve, "on_curve_supply": launch.on_curve_supply }),
        );
        self.launches.get(&token_id).unwrap().clone()
    }

    /// Migrate a graduated launch's leftover tokens into a permanent, LOCKED single-sided DCL
    /// position. **PERMISSIONLESS (MEDIUM-1):** any caller — the frontend, an indexer, a keeper, or a
    /// stranded holder — may seed a `Graduated` launch. The only guard is `phase == Graduated`, so it
    /// can never touch a live curve, and it moves the launch's own earmarked reserve (never the
    /// caller's funds). Removing the owner-only gate removes the single point of failure: a negligent
    /// or lost-key owner can no longer strand holders and lock `real_near` in `Graduated` forever. The
    /// LP position NFT is still minted to the router, which exposes NO remove/transfer path, so the
    /// liquidity can never be pulled by anyone.
    ///
    /// Single-sided per design — the entire remaining on-curve token supply is placed as the launch
    /// token, with 0 wNEAR. DCL canonicalizes the pair so `token_x` is the lexicographically smaller
    /// account id; the launch token is `token_x` when it sorts below wNEAR (range strictly ABOVE
    /// spot) or `token_y` when it sorts above (range strictly BELOW spot). Either way the position is
    /// 100% launch token and the accumulated real wNEAR stays with the router.
    ///
    /// The price points are DERIVED ON-CHAIN (MEDIUM-1), not caller-supplied: `init_point` encodes the
    /// exact graduation price (`real_near / token_reserve`) via [`derive_init_point`] so the pool opens
    /// with no arb gap, and the fixed-width single-sided range (`DCL_RANGE_WIDTH`) is placed one
    /// `point_delta` off spot on the correct side. No owner discretion is legitimate here.
    ///
    /// Fires the saga: storage_deposit on the launch FT (register DCL so it can receive the token) →
    /// storage_deposit on DCL (register + pre-fund the router's slot-based DCL storage) → create_pool
    /// → ft_transfer_call (deposit the tokens to the router's DCL internal balance, msg "Deposit") →
    /// add_liquidity (non-payable). It is idempotent/resumable via the MEDIUM-2 `SeedStage`, so a
    /// keeper retry after a partial failure reuses the created pool + parked deposit rather than
    /// re-running those legs. `on_pool_seeded` finalizes to `Pooled` on success or reverts to
    /// `Graduated` (retryable) on failure; any tokens parked in the DCL internal balance after a
    /// failed add are recoverable via DCL `withdraw_asset`.
    ///
    /// The full saga is PROVEN end-to-end on the live `dclv2.ref-dev.testnet` (2026-09-26e): a real
    /// graduate → seed_pool run created pool `star…|wrap.testnet|2000`, credited the token deposit,
    /// and minted a locked single-sided LP position (`get_pool.total_x` == the graduated reserve,
    /// `total_y` == 0). The attached deposits are conservative (DCL refunds excess); the mainnet DCL
    /// (`dclv2.ref-labs.near`, same code family) should behave identically but re-confirm the storage
    /// bounds there, as they are a live parameter.
    pub fn seed_pool(&mut self, token_id: TokenId) -> Promise {
        let info = self.launches.get(&token_id).expect("unknown launch token").clone();
        require!(
            info.phase == Phase::Graduated,
            "launch has not graduated (or is already pooling/pooled)"
        );
        let amount = info.token_reserve.0;
        require!(amount > 0, "no tokens left to seed");
        let real_near = info.real_near.0;
        require!(real_near > 0, "no graduation price to derive the pool's init_point from");

        // DCL canonicalizes token order: token_x is the lexicographically smaller account id
        // (verified across every live pool). A single-sided all-token position therefore sits on a
        // different side of spot depending on whether the launch token sorts below or above wNEAR:
        //   launch == token_x (launch < wNEAR): range strictly ABOVE spot -> init < left < right
        //   launch == token_y (launch > wNEAR): range strictly BELOW spot -> left < right < init
        let launch_is_x = token_id.as_str() < WNEAR;
        // Derive the opening tick from the graduation price on-chain (no owner input), then place the
        // fixed-width range one point_delta off spot on the single-token side. All offsets are
        // multiples of DCL_POINT_DELTA and init is aligned, so every point stays aligned by construction.
        let init_point = derive_init_point(real_near, amount, launch_is_x);
        let (left_point, right_point) = if launch_is_x {
            let left = init_point + DCL_POINT_DELTA; // strictly above spot -> position holds only token_x
            (left, left + DCL_RANGE_WIDTH)
        } else {
            let right = init_point - DCL_POINT_DELTA; // strictly below spot -> position holds only token_y
            (right - DCL_RANGE_WIDTH, right)
        };

        let stage = info.seed_stage;
        // Deposit the FULL reserve, but REQUEST slightly less so DCL's charged-token round-up draws
        // from the on-balance surplus instead of underflowing (the leg-7 blocker). See below.
        let requested = add_liquidity_request(amount);
        let launch = self.launches.get_mut(&token_id).unwrap();
        launch.phase = Phase::Pooling;
        emit(
            "seed_pool",
            json!({ "token_id": token_id, "amount": U128(amount), "requested": U128(requested),
                    "headroom": U128(amount - requested), "fee": DCL_FEE,
                    "init_point": init_point, "left_point": left_point, "right_point": right_point,
                    "resume_from": stage }),
        );

        let dcl: AccountId = DCL.parse().unwrap();
        // Canonical DCL order (token_x < token_y). The launch token sits on whichever side it sorts
        // to; the full launch-token reserve funds that side and the wNEAR side is 0 (single-sided).
        let (token_x, token_y) = if launch_is_x {
            (token_id.to_string(), WNEAR.to_string())
        } else {
            (WNEAR.to_string(), token_id.to_string())
        };
        // We deposit `amount` (below) but request `requested` (= amount − headroom) so DCL's round-up
        // stays within the deposited balance. The leftover dust is recoverable via `withdraw_asset`.
        let (amount_x, amount_y) = if launch_is_x {
            (U128(requested), U128(0))
        } else {
            (U128(0), U128(requested))
        };
        let pool_id = format!("{}|{}|{}", token_x, token_y, DCL_FEE);
        let me = env::current_account_id();

        // Register DCL on the launch token's FT. `ft_transfer_call` below moves the tokens into
        // DCL's balance ON the FT first; a standard NEP-141 aborts that transfer if the receiver is
        // unregistered ("receiver not registered" — the trap that failed the first 26e run). The
        // router owns the FT, so it pays this NEP-145 min (0.00125 NEAR) to open DCL's slot.
        // Idempotent (a retry just tops up an already-open slot; excess is refunded), so it always runs.
        let register_dcl = Promise::new(token_id.clone()).function_call(
            "storage_deposit".to_string(),
            json!({ "account_id": DCL }).to_string().into_bytes(),
            FT_STORAGE_DEPOSIT,
            GAS_FOR_FT_STORAGE,
        );
        // Register the router on DCL and pre-fund its slot-based storage. MUST NOT pass
        // `registration_only` (that refunds all headroom above the 0.5 base, so the token deposit
        // below silently refunds and never credits — the 26d failure). account_id defaults to the
        // predecessor (this router), so `{}` funds our own balance. Also idempotent — always runs.
        let storage = Promise::new(dcl.clone()).function_call(
            "storage_deposit".to_string(),
            json!({}).to_string().into_bytes(),
            DCL_STORAGE_DEPOSIT,
            GAS_FOR_DCL_STORAGE,
        );

        // Resumable saga: `create_pool` and the token `deposit` are NOT idempotent (create traps on
        // an existing pool; a second deposit double-moves the reserve), so a retry fires them ONLY if
        // its stage hasn't recorded them yet. Each is followed by a tiny callback that advances the
        // stage on confirmed success, so the NEXT retry resumes exactly where this one stopped. The
        // `add_liquidity` finalize always runs — it is where a resumed saga completes.
        // Exclude the DCL pool from FT holder rewards BEFORE the reserve is deposited into it, so
        // DCL's large pool balance never dilutes real holders (BRIEF gates 1 + 3). The router is the
        // FT's authority; `add_excluded` is idempotent (no-op if already excluded), so it always runs
        // and is safe on a resumed saga. DCL holds 0 launch tokens on the FT at this point, so the
        // exclusion removes nothing from `eligible_supply`; the later deposit (router→DCL, both
        // excluded) then nets zero on it.
        let exclude_dcl = Promise::new(token_id.clone()).function_call(
            "add_excluded".to_string(),
            json!({ "account": DCL }).to_string().into_bytes(),
            NearToken::from_yoctonear(0),
            GAS_FOR_FT_EXCLUDE,
        );
        let mut chain = exclude_dcl.then(register_dcl).then(storage);

        if stage < SeedStage::PoolCreated {
            let create = Promise::new(dcl.clone()).function_call(
                "create_pool".to_string(),
                json!({ "token_a": token_x, "token_b": token_y, "fee": DCL_FEE, "init_point": init_point })
                    .to_string()
                    .into_bytes(),
                DCL_CREATE_POOL_DEPOSIT,
                GAS_FOR_DCL_CREATE_POOL,
            );
            chain = chain.then(create).then(
                Self::ext(me.clone())
                    .with_static_gas(GAS_FOR_SEED_PROGRESS_CB)
                    .on_seed_create(token_id.clone()),
            );
        }

        if stage < SeedStage::Deposited {
            // Deposit the tokens to the router's DCL internal balance. DCL's ft_on_transfer parses
            // `msg` as its TokenReceiverMessage enum; the plain-deposit variant is the JSON string
            // "Deposit" (an empty msg fails "E600: invalid msg" and refunds — the 26e refund bug).
            let deposit = Promise::new(token_id.clone()).function_call(
                "ft_transfer_call".to_string(),
                json!({ "receiver_id": DCL, "amount": U128(amount), "msg": "\"Deposit\"" })
                    .to_string()
                    .into_bytes(),
                NearToken::from_yoctonear(1),
                GAS_FOR_DCL_DEPOSIT,
            );
            chain = chain.then(deposit).then(
                Self::ext(me.clone())
                    .with_static_gas(GAS_FOR_SEED_PROGRESS_CB)
                    .on_seed_deposit(token_id.clone(), U128(amount)),
            );
        }

        let add = Promise::new(dcl).function_call(
            "add_liquidity".to_string(),
            json!({ "pool_id": pool_id, "left_point": left_point, "right_point": right_point,
                    "amount_x": amount_x, "amount_y": amount_y,
                    "min_amount_x": U128(0), "min_amount_y": U128(0) })
                .to_string()
                .into_bytes(),
            DCL_ADD_LIQUIDITY_DEPOSIT,
            GAS_FOR_DCL_ADD_LIQUIDITY,
        );
        chain.then(add).then(
            Self::ext(me)
                .with_static_gas(GAS_FOR_POOL_SEEDED_CB)
                .on_pool_seeded(token_id, U128(amount)),
        )
    }

    /// Record that `create_pool` confirmed (the DCL pool now exists) so a retry skips it — real DCL
    /// traps on a duplicate `create_pool`. Advances `NotStarted → PoolCreated` only on success.
    #[private]
    pub fn on_seed_create(
        &mut self,
        token_id: TokenId,
        #[callback_result] res: Result<near_sdk::serde_json::Value, PromiseError>,
    ) {
        if res.is_ok() {
            if let Some(l) = self.launches.get_mut(&token_id) {
                if l.seed_stage < SeedStage::PoolCreated {
                    l.seed_stage = SeedStage::PoolCreated;
                }
            }
        }
    }

    /// Record that the token `deposit` landed in the router's DCL internal balance so a retry does
    /// NOT re-transfer (which would double-move the reserve). `ft_transfer_call` resolves to the
    /// amount the receiver USED; only the full amount landing counts, and only after `PoolCreated`
    /// (the order invariant), advancing `PoolCreated → Deposited`.
    #[private]
    pub fn on_seed_deposit(
        &mut self,
        token_id: TokenId,
        amount: U128,
        #[callback_result] res: Result<U128, PromiseError>,
    ) {
        if let Ok(used) = res {
            if used.0 == amount.0 {
                if let Some(l) = self.launches.get_mut(&token_id) {
                    if l.seed_stage == SeedStage::PoolCreated {
                        l.seed_stage = SeedStage::Deposited;
                    }
                }
            }
        }
    }

    /// Resolve the pool-seeding saga. On success the launch token now lives in a locked DCL
    /// position, so we mark the launch `Pooled` and clear the migrated tokens off the router's
    /// curve reserve. On failure we revert `Pooling → Graduated` so the owner can retry (the
    /// deposited tokens, if any, are recoverable via DCL `withdraw_asset`). `add_liquidity`'s
    /// return (the lpt id) is captured as a raw JSON value so any return shape resolves without
    /// panicking, and is echoed in the `pool_seeded` event for off-chain verification of the locked
    /// position.
    #[private]
    pub fn on_pool_seeded(
        &mut self,
        token_id: TokenId,
        amount: U128,
        #[callback_result] res: Result<near_sdk::serde_json::Value, PromiseError>,
    ) -> bool {
        match res {
            Ok(lpt) => {
                if let Some(l) = self.launches.get_mut(&token_id) {
                    l.phase = Phase::Pooled;
                    l.token_reserve = U128(l.token_reserve.0.saturating_sub(amount.0));
                }
                emit(
                    "pool_seeded",
                    json!({ "token_id": token_id, "amount": amount, "lpt": lpt }),
                );
                true
            }
            Err(_) => {
                if let Some(l) = self.launches.get_mut(&token_id) {
                    l.phase = Phase::Graduated;
                }
                emit("pool_seed_failed", json!({ "token_id": token_id }));
                false
            }
        }
    }


    // ---------- trading (stage 1B) ----------

    /// NEP-141 receiver hook — the single entry point for every curve interaction. The token that
    /// called it (the predecessor) selects the path: wNEAR in is a buy; a registered launch token
    /// in is either the one-time supply "seed" or a "sell". The returned promise resolves to the
    /// amount of the *incoming* token to refund the sender (wNEAR on a buy, launch token on a
    /// sell), so a failed delivery is always made whole and the curve is never left inconsistent.
    pub fn ft_on_transfer(
        &mut self,
        sender_id: AccountId,
        amount: U128,
        msg: String,
    ) -> PromiseOrValue<U128> {
        let token = env::predecessor_account_id();
        if token.as_str() == WNEAR {
            let m: WnearMsg = near_sdk::serde_json::from_str(&msg).expect("invalid buy msg");
            require!(self.launches.contains_key(&m.token_id), "unknown launch token");
            self.handle_buy(m.token_id, sender_id, amount, m.min_out.0, m.dev_buy)
        } else {
            require!(self.launches.contains_key(&token), "unknown launch token");
            let m: TokenMsg = near_sdk::serde_json::from_str(&msg).expect("invalid token msg");
            match m.action.as_str() {
                "seed" => self.handle_seed(token, sender_id, amount),
                "sell" => self.handle_sell(token, sender_id, amount, m.min_out.0),
                _ => env::panic_str("unknown token action"),
            }
        }
    }


    /// Deposit the on-curve token supply into the router (Seeding phase, one time). The launch
    /// token calls this via `ft_transfer_call` with msg `{"action":"seed"}`; only the creator or
    /// owner may seed, and the amount must equal the registered `on_curve_supply`. Accepts all
    /// (returns 0 to refund).
    fn handle_seed(
        &mut self,
        token_id: TokenId,
        sender_id: AccountId,
        amount: U128,
    ) -> PromiseOrValue<U128> {
        let info = self.launches.get(&token_id).unwrap();
        require!(info.phase == Phase::Seeding, "launch is not in the seeding phase");
        require!(
            sender_id == info.creator || sender_id == self.owner_id,
            "only creator or owner can seed the curve"
        );
        require!(info.token_reserve.0 == 0, "curve already seeded");
        require!(amount.0 == info.on_curve_supply.0, "seed must equal the on-curve supply");
        let launch = self.launches.get_mut(&token_id).unwrap();
        launch.token_reserve = amount;
        emit("seed", json!({ "token_id": token_id, "supply": amount }));
        PromiseOrValue::Value(U128(0))
    }

    /// Book one trade's 1% fee: 10% to the protocol, 90% to the launch pool, the pool cut split
    /// into the three immutable creator/buyback/holder accrual buckets (holder takes the rounding
    /// remainder so nothing leaks). Shared by buys and sells. Reversed exactly by `unbook_fee`.
    fn book_fee(&mut self, token_id: &TokenId, fee: u128) {
        let protocol_cut = fee * self.protocol_bps as u128 / 10_000;
        let pool_cut = fee - protocol_cut;
        self.protocol_fees = U128(self.protocol_fees.0 + protocol_cut);
        let split = self.launches.get(token_id).unwrap().split.clone();
        let launch = self.launches.get_mut(token_id).unwrap();
        launch.fees_collected = U128(launch.fees_collected.0 + pool_cut);
        let creator_part = pool_cut * split.creator_bps as u128 / 10_000;
        let buyback_part = pool_cut * split.buyback_bps as u128 / 10_000;
        let holder_part = pool_cut - creator_part - buyback_part;
        let mut accr = self.accruals.get(token_id).cloned().unwrap_or_else(Accrual::zero);
        accr.creator = U128(accr.creator.0 + creator_part);
        accr.buyback = U128(accr.buyback.0 + buyback_part);
        accr.holder = U128(accr.holder.0 + holder_part);
        self.accruals.insert(token_id.clone(), accr);
    }


    /// Execute a buy against the curve. Skims the 1% fee off the incoming wNEAR, prices the
    /// remainder on the constant-product curve, books the fee, advances the reserves, and delivers
    /// the tokens straight from the router's own supply. A normal buy needs `Live`; a `dev_buy`
    /// needs `Seeding` + the creator, and is capped so cumulative dev tokens never exceed 4% of
    /// supply. If the curve can't fill the buy (too little out, below `min_out`, over the cap),
    /// the whole wNEAR is refunded synchronously with no state change.
    fn handle_buy(
        &mut self,
        token_id: TokenId,
        buyer: AccountId,
        amount: U128,
        min_out: u128,
        is_dev: bool,
    ) -> PromiseOrValue<U128> {
        let info = self.launches.get(&token_id).unwrap().clone();
        require!(!self.paused, "trading is paused");
        if is_dev {
            require!(info.phase == Phase::Seeding, "dev buy is only allowed before launch");
            require!(buyer == info.creator, "only the creator can dev buy");
        } else {
            require!(info.phase == Phase::Live, "trading is not live");
        }
        require!(info.token_reserve.0 > 0, "curve is not seeded");

        let amount_in = amount.0;
        let fee = amount_in * FEE_BPS / 10_000;
        let near_in = amount_in - fee;
        let current_near = info.virtual_near.0 + info.real_near.0;
        let out = curve::tokens_out(info.token_reserve.0, current_near, near_in);
        // Reject (full refund, no state change) if the curve can't fill it or slippage guard trips.
        if out == 0 || out < min_out || out > info.token_reserve.0 {
            return PromiseOrValue::Value(amount);
        }
        if is_dev {
            let cap = info.on_curve_supply.0 * DEV_BUY_CAP_BPS / 10_000;
            if info.dev_bought.0 + out > cap {
                return PromiseOrValue::Value(amount); // would breach the 4% anti-rug cap
            }
        }

        // Optimistic state: book the fee and advance the reserves, then deliver. `on_buy_deliver`
        // rolls all of this back and refunds the wNEAR if the token transfer fails.
        self.book_fee(&token_id, fee);
        let launch = self.launches.get_mut(&token_id).unwrap();
        launch.real_near = U128(launch.real_near.0 + near_in);
        launch.token_reserve = U128(launch.token_reserve.0 - out);
        launch.volume = U128(launch.volume.0 + amount_in);
        if is_dev {
            launch.dev_bought = U128(launch.dev_bought.0 + out);
        }
        emit(
            "buy",
            json!({ "token_id": token_id, "buyer": buyer, "near_in": U128(near_in),
                    "tokens_out": U128(out), "fee": U128(fee), "dev": is_dev,
                    "real_near": launch.real_near, "token_reserve": launch.token_reserve }),
        );

        let deliver = Self::ft_transfer_token(&token_id, &buyer, out);
        PromiseOrValue::Promise(deliver.then(
            Self::ext(env::current_account_id())
                .with_static_gas(GAS_FOR_DELIVER_CB)
                .on_buy_deliver(token_id, amount, U128(near_in), U128(out), U128(fee), is_dev),
        ))
    }


    /// Resolve token delivery for a buy. On success the buyer has the tokens, so we keep the wNEAR
    /// (refund 0) and the fee stays booked. On failure (e.g. the buyer isn't storage-registered on
    /// the token) we roll the curve back and refund the buyer's full wNEAR — a failed buy is never
    /// charged and never strands funds. Saturating math is defensive; nothing spends this state
    /// between the buy and its callback within one receipt chain.
    #[private]
    pub fn on_buy_deliver(
        &mut self,
        token_id: TokenId,
        amount: U128,
        near_in: U128,
        tokens_out: U128,
        fee: U128,
        is_dev: bool,
        #[callback_result] res: Result<(), PromiseError>,
    ) -> U128 {
        match res {
            Ok(_) => U128(0),
            Err(_) => {
                self.unbook_fee(&token_id, fee.0);
                if let Some(l) = self.launches.get_mut(&token_id) {
                    l.real_near = U128(l.real_near.0.saturating_sub(near_in.0));
                    l.token_reserve = U128(l.token_reserve.0 + tokens_out.0);
                    l.volume = U128(l.volume.0.saturating_sub(amount.0));
                    if is_dev {
                        l.dev_bought = U128(l.dev_bought.0.saturating_sub(tokens_out.0));
                    }
                }
                amount
            }
        }
    }


    /// Execute a sell against the curve. The seller sends launch tokens in; the curve prices the
    /// gross wNEAR out (capped at the real wNEAR on the curve — the virtual reserve is never
    /// withdrawable), the 1% fee is skimmed from that, and the seller is paid the remainder in
    /// wNEAR. `on_sell_payout` returns the seller's tokens and rolls the curve back if the wNEAR
    /// payout fails. A zero quote or a payout below `min_out` refunds all tokens with no state
    /// change.
    fn handle_sell(
        &mut self,
        token_id: TokenId,
        seller: AccountId,
        token_in: U128,
        min_out: u128,
    ) -> PromiseOrValue<U128> {
        let info = self.launches.get(&token_id).unwrap().clone();
        require!(!self.paused, "trading is paused");
        require!(info.phase == Phase::Live, "trading is not live");
        let tokens = token_in.0;
        let current_near = info.virtual_near.0 + info.real_near.0;
        let gross = curve::near_out(current_near, info.token_reserve.0, tokens, info.real_near.0);
        if gross == 0 {
            return PromiseOrValue::Value(token_in); // nothing to pay — refund the tokens
        }
        let fee = gross * FEE_BPS / 10_000;
        let seller_near = gross - fee;
        if seller_near < min_out {
            return PromiseOrValue::Value(token_in); // slippage guard — refund the tokens
        }

        // Optimistic state: gross wNEAR leaves the curve (fee retained, remainder paid out), the
        // sold tokens return to the reserve. `on_sell_payout` reverses this if the payout fails.
        self.book_fee(&token_id, fee);
        let launch = self.launches.get_mut(&token_id).unwrap();
        launch.real_near = U128(launch.real_near.0 - gross);
        launch.token_reserve = U128(launch.token_reserve.0 + tokens);
        launch.volume = U128(launch.volume.0 + gross);
        emit(
            "sell",
            json!({ "token_id": token_id, "seller": seller, "tokens_in": token_in,
                    "near_out": U128(seller_near), "fee": U128(fee),
                    "real_near": launch.real_near, "token_reserve": launch.token_reserve }),
        );

        let pay = Self::ft_transfer_wnear(&seller, seller_near);
        PromiseOrValue::Promise(pay.then(
            Self::ext(env::current_account_id())
                .with_static_gas(GAS_FOR_DELIVER_CB)
                .on_sell_payout(token_id, token_in, U128(gross), U128(fee)),
        ))
    }

    /// Resolve a sell's wNEAR payout. On success the seller is paid, so we keep their tokens
    /// (refund 0). On failure we unbook the fee, restore the curve, and refund all tokens.
    #[private]
    pub fn on_sell_payout(
        &mut self,
        token_id: TokenId,
        token_in: U128,
        gross: U128,
        fee: U128,
        #[callback_result] res: Result<(), PromiseError>,
    ) -> U128 {
        match res {
            Ok(_) => U128(0),
            Err(_) => {
                self.unbook_fee(&token_id, fee.0);
                if let Some(l) = self.launches.get_mut(&token_id) {
                    l.real_near = U128(l.real_near.0 + gross.0);
                    l.token_reserve = U128(l.token_reserve.0.saturating_sub(token_in.0));
                    l.volume = U128(l.volume.0.saturating_sub(gross.0));
                }
                token_in
            }
        }
    }

    /// Reverse one trade's booked fee (protocol cut + the launch's three accrual buckets),
    /// recomputed with the same formulas used at booking so the reversal is exact. Called from
    /// `on_buy_deliver`/`on_sell_payout` when the delivery leg failed. Saturating subtraction is
    /// defensive: within a single transaction nothing can spend the just-booked fee before this runs.
    fn unbook_fee(&mut self, token_id: &TokenId, fee: u128) {
        let protocol_cut = fee * self.protocol_bps as u128 / 10_000;
        let pool_cut = fee - protocol_cut;
        let split = match self.launches.get(token_id) {
            Some(l) => l.split.clone(),
            None => return,
        };
        self.protocol_fees = U128(self.protocol_fees.0.saturating_sub(protocol_cut));
        if let Some(launch) = self.launches.get_mut(token_id) {
            launch.fees_collected = U128(launch.fees_collected.0.saturating_sub(pool_cut));
        }
        let creator_part = pool_cut * split.creator_bps as u128 / 10_000;
        let buyback_part = pool_cut * split.buyback_bps as u128 / 10_000;
        let holder_part = pool_cut - creator_part - buyback_part;
        if let Some(accr) = self.accruals.get_mut(token_id) {
            accr.creator = U128(accr.creator.0.saturating_sub(creator_part));
            accr.buyback = U128(accr.buyback.0.saturating_sub(buyback_part));
            accr.holder = U128(accr.holder.0.saturating_sub(holder_part));
        }
    }

    // ---------- claims (stage 1C) ----------

    /// Pay the creator their accrued cash share (the `creator` bucket) in wNEAR. Only the
    /// launch's creator can call this. The accrual is zeroed optimistically and restored by
    /// the callback if the wNEAR transfer fails, so a failed payout never burns the balance.
    pub fn claim_creator_fees(&mut self, token_id: TokenId) -> Promise {
        let creator = self
            .launches
            .get(&token_id)
            .expect("unknown launch token")
            .creator
            .clone();
        require!(
            env::predecessor_account_id() == creator,
            "only the launch creator can claim"
        );
        let accr = self.accruals.get_mut(&token_id).expect("nothing accrued");
        let amount = accr.creator.0;
        require!(amount > 0, "nothing to claim");
        accr.creator = U128(0);
        emit("claim", json!({ "token_id": token_id, "kind": "creator", "amount": U128(amount) }));

        Self::ft_transfer_wnear(&creator, amount).then(
            Self::ext(env::current_account_id())
                .with_static_gas(GAS_FOR_CLAIM_CB)
                .on_creator_claim(token_id, U128(amount)),
        )
    }

    /// Pay the protocol owner the accrued protocol fees in wNEAR. Owner-only, same
    /// zero-then-restore-on-failure pattern as the creator claim.
    pub fn claim_protocol_fees(&mut self) -> Promise {
        require!(
            env::predecessor_account_id() == self.owner_id,
            "only the owner can claim protocol fees"
        );
        let amount = self.protocol_fees.0;
        require!(amount > 0, "nothing to claim");
        self.protocol_fees = U128(0);
        let owner = self.owner_id.clone();
        emit("claim", json!({ "kind": "protocol", "amount": U128(amount) }));

        Self::ft_transfer_wnear(&owner, amount).then(
            Self::ext(env::current_account_id())
                .with_static_gas(GAS_FOR_CLAIM_CB)
                .on_protocol_claim(U128(amount)),
        )
    }

    #[private]
    pub fn on_creator_claim(
        &mut self,
        token_id: TokenId,
        amount: U128,
        #[callback_result] res: Result<(), PromiseError>,
    ) {
        if res.is_err() {
            if let Some(accr) = self.accruals.get_mut(&token_id) {
                accr.creator = U128(accr.creator.0 + amount.0);
            }
        }
    }

    #[private]
    pub fn on_protocol_claim(
        &mut self,
        amount: U128,
        #[callback_result] res: Result<(), PromiseError>,
    ) {
        if res.is_err() {
            self.protocol_fees = U128(self.protocol_fees.0 + amount.0);
        }
    }

    /// wNEAR ft_transfer payout leg shared by the claim methods. Attaches the required 1
    /// yoctoNEAR; the receiver must already be storage-registered on wrap.testnet.
    fn ft_transfer_wnear(receiver: &AccountId, amount: u128) -> Promise {
        let args = json!({ "receiver_id": receiver, "amount": U128(amount) })
            .to_string()
            .into_bytes();
        Promise::new(WNEAR.parse().unwrap()).function_call(
            "ft_transfer".to_string(),
            args,
            NearToken::from_yoctonear(1),
            GAS_FOR_FT_TRANSFER,
        )
    }

    /// Deliver launch tokens from the router's own held supply to a buyer/seller. Attaches the
    /// required 1 yoctoNEAR; the receiver must be storage-registered on the token.
    fn ft_transfer_token(token_id: &TokenId, receiver: &AccountId, amount: u128) -> Promise {
        let args = json!({ "receiver_id": receiver, "amount": U128(amount) })
            .to_string()
            .into_bytes();
        Promise::new(token_id.clone()).function_call(
            "ft_transfer".to_string(),
            args,
            NearToken::from_yoctonear(1),
            GAS_FOR_FT_TRANSFER,
        )
    }

    // ---------- holder rewards (forwarded to the reward-bearing launch FT) ----------

    /// Flush a launch's accrued `holder` bucket into the launch FT's on-chain reward accumulator.
    /// **Permissionless** (deterministic — no discretion): it forwards the already-booked bucket as
    /// a wNEAR reward via `ft_transfer_call(token_id, amount, "reward")`; the FT then credits every
    /// holder pro-rata by held balance (hold-to-earn — there is NO stake). The router registers the
    /// FT on wNEAR first (lazy + idempotent: wNEAR refunds a duplicate `storage_deposit`). The
    /// `holder` bucket is zeroed OPTIMISTICALLY and restored by `on_holder_forwarded` if the forward
    /// fails or the FT refunds part of it (same unwind discipline as the fee claims), so the bucket
    /// is never lost and never double-forwarded.
    pub fn distribute_holder_rewards(&mut self, token_id: TokenId) -> Promise {
        require!(self.launches.contains_key(&token_id), "unknown launch token");
        let amount = self.accruals.get(&token_id).map(|a| a.holder.0).unwrap_or(0);
        require!(amount > 0, "nothing to distribute");
        if let Some(accr) = self.accruals.get_mut(&token_id) {
            accr.holder = U128(0);
        }
        emit(
            "holder_distribute",
            json!({ "token_id": token_id, "amount": U128(amount) }),
        );
        let wnear: AccountId = WNEAR.parse().unwrap();
        // 1) ensure the FT is a registered wNEAR holder so it can custody the reward.
        let register = Promise::new(wnear.clone()).function_call(
            "storage_deposit".to_string(),
            json!({ "account_id": token_id }).to_string().into_bytes(),
            WNEAR_REG_DEPOSIT,
            GAS_FOR_FT_WNEAR_REG,
        );
        // 2) forward the bucket as a "reward" the FT distributes across all holders.
        let forward = Promise::new(wnear).function_call(
            "ft_transfer_call".to_string(),
            json!({ "receiver_id": token_id, "amount": U128(amount), "msg": "reward" })
                .to_string()
                .into_bytes(),
            NearToken::from_yoctonear(1),
            GAS_FOR_REWARD_FORWARD,
        );
        register.then(forward).then(
            Self::ext(env::current_account_id())
                .with_static_gas(GAS_FOR_FORWARD_CB)
                .on_holder_forwarded(token_id, U128(amount)),
        )
    }

    /// Restore the `holder` bucket if the reward forward failed or the FT refunded part of it. The
    /// wNEAR `ft_transfer_call` resolves to the amount the FT ACCEPTED; the router re-books the
    /// unaccepted remainder (full amount on an outright failure) so the next `distribute` retries it.
    #[private]
    pub fn on_holder_forwarded(
        &mut self,
        token_id: TokenId,
        amount: U128,
        #[callback_result] res: Result<U128, PromiseError>,
    ) {
        let refunded = match res {
            // FT returned the amount it USED (accepted); anything not used is refunded to the router.
            Ok(used) => amount.0.saturating_sub(used.0),
            // Outright failure: the whole amount bounced back to the router's wNEAR balance.
            Err(_) => amount.0,
        };
        if refunded > 0 {
            if let Some(accr) = self.accruals.get_mut(&token_id) {
                accr.holder = U128(accr.holder.0 + refunded);
            }
            emit(
                "holder_distribute_reverted",
                json!({ "token_id": token_id, "restored": U128(refunded) }),
            );
        }
    }

    // ---------- buyback (buy-from-curve + lock) ----------

    /// Spend a launch's entire accrued `buyback` bucket buying its OWN token off the curve,
    /// fee-free, and permanently LOCK the bought tokens in the router (added to `burned`, no move
    /// path anywhere). **Permissionless + deterministic** — it spends a pre-accrued bucket at the
    /// current curve price; there is no admin discretion and no external transfer, so it is fully
    /// synchronous with no rollback. Requires `Live` (the curve must be open to buy from). The
    /// bought tokens are equivalent to burned: the audited FT has no burn method, so we lock
    /// rather than mint a burn authority — supply integrity is preserved.
    pub fn execute_buyback(&mut self, token_id: TokenId) -> U128 {
        require!(!self.paused, "trading is paused");
        let info = self.launches.get(&token_id).expect("unknown launch token").clone();
        require!(info.phase == Phase::Live, "launch is not live");
        let amount = self.accruals.get(&token_id).map(|a| a.buyback.0).unwrap_or(0);
        require!(amount > 0, "nothing to buy back");
        let current_near = info.virtual_near.0 + info.real_near.0;
        let out = curve::tokens_out(info.token_reserve.0, current_near, amount);
        require!(out > 0 && out < info.token_reserve.0, "buyback amount does not fill on the curve");
        // Spend the bucket, advance the curve exactly like a buy, and lock the tokens.
        if let Some(accr) = self.accruals.get_mut(&token_id) {
            accr.buyback = U128(0);
        }
        let launch = self.launches.get_mut(&token_id).unwrap();
        launch.real_near = U128(launch.real_near.0 + amount);
        launch.token_reserve = U128(launch.token_reserve.0 - out);
        launch.burned = U128(launch.burned.0 + out);
        launch.volume = U128(launch.volume.0 + amount);
        emit(
            "buyback",
            json!({ "token_id": token_id, "near_spent": U128(amount), "tokens_locked": U128(out),
                    "burned_total": launch.burned, "real_near": launch.real_near,
                    "token_reserve": launch.token_reserve }),
        );
        U128(out)
    }

    // ---------- views ----------

    /// Preview a buy: tokens the curve would deliver for `near_in` wNEAR *after* the 1% fee is
    /// skimmed (pass the gross wNEAR the buyer would send). Read-only; ignores phase + cap.
    pub fn quote_buy(&self, token_id: TokenId, amount: U128) -> U128 {
        let info = match self.launches.get(&token_id) {
            Some(i) => i,
            None => return U128(0),
        };
        let near_in = amount.0 - amount.0 * FEE_BPS / 10_000;
        let current_near = info.virtual_near.0 + info.real_near.0;
        U128(curve::tokens_out(info.token_reserve.0, current_near, near_in))
    }

    /// Preview a sell: net wNEAR the seller would receive for `token_in` tokens, after the 1% fee
    /// and the real-wNEAR cap. Read-only.
    pub fn quote_sell(&self, token_id: TokenId, token_in: U128) -> U128 {
        let info = match self.launches.get(&token_id) {
            Some(i) => i,
            None => return U128(0),
        };
        let current_near = info.virtual_near.0 + info.real_near.0;
        let gross =
            curve::near_out(current_near, info.token_reserve.0, token_in.0, info.real_near.0);
        U128(gross - gross * FEE_BPS / 10_000)
    }



    /// Booked-but-unspent fee buckets for a launch (creator claimable, buyback pending,
    /// holder pending). `None` until the launch has taken its first routed buy.
    pub fn get_accrual(&self, token_id: TokenId) -> Option<Accrual> {
        self.accruals.get(&token_id).cloned()
    }

    pub fn get_launch(&self, token_id: TokenId) -> Option<LaunchInfo> {
        self.launches.get(&token_id).cloned()
    }

    pub fn get_launches(&self, from_index: Option<u32>, limit: Option<u32>) -> Vec<LaunchInfo> {
        let from = from_index.unwrap_or(0) as usize;
        let limit = limit.unwrap_or(50) as usize;
        self.launches.values().skip(from).take(limit).cloned().collect()
    }

    pub fn get_num_launches(&self) -> u32 {
        self.launches.len()
    }

    pub fn get_owner(&self) -> AccountId {
        self.owner_id.clone()
    }

    /// Treasury that receives every launch fee. Immutable after `new` (no setter).
    pub fn get_treasury(&self) -> AccountId {
        self.treasury_id.clone()
    }

    /// The launch fee for a tier, in yocto — the exact amount `create_launch` requires the creator
    /// to attach (Basic 0.18 / Express 0.35). The router funds the token deploy out of it and
    /// forwards the margin to the treasury on success.
    pub fn get_launch_fee(&self, tier: Tier) -> U128 {
        U128(Self::tier_fee(tier).as_yoctonear())
    }

    pub fn get_protocol_bps(&self) -> u16 {
        self.protocol_bps
    }

    pub fn get_protocol_fees(&self) -> U128 {
        self.protocol_fees
    }

    // ---------- Express Premium Board (on-chain FIFO) ----------

    /// Enqueue an Express launch onto the Premium Board FIFO (called internally on a successful
    /// Express deploy). Assigns a monotonic `position` (defines FIFO order) and immediately tries to
    /// promote, so a free slot is filled at once.
    fn enqueue_express(&mut self, token_id: &TokenId) {
        let position = self.express_seq;
        self.express_seq += 1;
        let now = env::block_timestamp();
        self.express_queue.push(ExpressEntry {
            token_id: token_id.clone(),
            state: ExpressState::Queued,
            enqueued_at: now,
            activated_at: None,
            position,
        });
        emit(
            "express_enqueue",
            json!({ "token_id": token_id, "position": position }),
        );
        self.promote_express_inner();
    }

    /// Expire timed-out Active entries and promote the oldest Queued entries into any free slots,
    /// strictly by enqueue order (true FIFO). **Permissionless + deterministic** (pure time/queue
    /// math, no discretion). Also called internally on enqueue; the frontend/indexer fires it to
    /// keep the board current. Returns the number of currently Active entries.
    pub fn promote_express_queue(&mut self) -> u32 {
        self.promote_express_inner()
    }

    fn promote_express_inner(&mut self) -> u32 {
        let now = env::block_timestamp();
        // 1) Expire Active entries whose 30-minute window elapsed.
        let mut active_count: u64 = 0;
        for i in 0..self.express_queue.len() {
            let e = self.express_queue.get(i).unwrap();
            if e.state == ExpressState::Active {
                let activated = e.activated_at.unwrap_or(e.enqueued_at);
                if now.saturating_sub(activated) >= EXPRESS_ACTIVE_DURATION_NS {
                    let tok = e.token_id.clone();
                    let entry = self.express_queue.get_mut(i).unwrap();
                    entry.state = ExpressState::Expired;
                    emit("express_expire", json!({ "token_id": tok }));
                } else {
                    active_count += 1;
                }
            }
        }
        // 2) Promote the oldest Queued entries (by position) until the board is full.
        while active_count < EXPRESS_BOARD_SLOTS {
            // Find the Queued entry with the smallest position.
            let mut best: Option<(u32, u64)> = None;
            for i in 0..self.express_queue.len() {
                let e = self.express_queue.get(i).unwrap();
                if e.state == ExpressState::Queued {
                    match best {
                        Some((_, p)) if e.position >= p => {}
                        _ => best = Some((i, e.position)),
                    }
                }
            }
            match best {
                Some((i, _)) => {
                    let tok = self.express_queue.get(i).unwrap().token_id.clone();
                    let entry = self.express_queue.get_mut(i).unwrap();
                    entry.state = ExpressState::Active;
                    entry.activated_at = Some(now);
                    emit("express_activate", json!({ "token_id": tok }));
                    active_count += 1;
                }
                None => break, // nothing left to promote
            }
        }
        active_count as u32
    }

    /// Cancel a still-`Queued` Express entry (creator of that launch or owner only). Active,
    /// Expired and already-Cancelled entries cannot be cancelled. Frees nothing to promote directly
    /// (promotion only fills from Active expiry), but removes the entry from the pending line.
    pub fn cancel_express(&mut self, token_id: TokenId) {
        let caller = env::predecessor_account_id();
        let is_owner = caller == self.owner_id;
        let creator = self.launches.get(&token_id).map(|l| l.creator.clone());
        let mut done = false;
        for i in 0..self.express_queue.len() {
            let e = self.express_queue.get(i).unwrap();
            if e.token_id == token_id && e.state == ExpressState::Queued {
                require!(
                    is_owner || creator.as_ref() == Some(&caller),
                    "only the launch creator or owner can cancel"
                );
                let entry = self.express_queue.get_mut(i).unwrap();
                entry.state = ExpressState::Cancelled;
                done = true;
                break;
            }
        }
        require!(done, "no queued express entry for this token");
        emit("express_cancel", json!({ "token_id": token_id }));
    }

    /// The currently `Active` Premium Board entries, in enqueue (FIFO) order. Read-only; reflects
    /// state as of the last promotion (call `promote_express_queue` first to settle expiries).
    pub fn get_express_board(&self) -> Vec<ExpressEntry> {
        let mut rows: Vec<ExpressEntry> = self
            .express_queue
            .iter()
            .filter(|e| e.state == ExpressState::Active)
            .cloned()
            .collect();
        rows.sort_by_key(|e| e.position);
        rows
    }

    /// The full Express queue (all states), paginated, in enqueue order — for the indexer/UI.
    pub fn get_express_queue(&self, from_index: Option<u32>, limit: Option<u32>) -> Vec<ExpressEntry> {
        let from = from_index.unwrap_or(0) as usize;
        let limit = limit.unwrap_or(50) as usize;
        let mut rows: Vec<ExpressEntry> = self.express_queue.iter().cloned().collect();
        rows.sort_by_key(|e| e.position);
        rows.into_iter().skip(from).take(limit).collect()
    }

    // ---------- emergency pause (blocks trading only) ----------

    /// Toggle the emergency circuit breaker. **Owner-only.** When paused, `buy`/`sell`/dev-buy and
    /// `execute_buyback` are blocked; claims, `distribute_holder_rewards`, `graduate` and every
    /// fund-returning path stay open. Pause grants the owner NO access to any funds.
    pub fn set_paused(&mut self, paused: bool) {
        require!(
            env::predecessor_account_id() == self.owner_id,
            "only the owner can pause"
        );
        self.paused = paused;
        emit("set_paused", json!({ "paused": paused }));
    }

    pub fn is_paused(&self) -> bool {
        self.paused
    }

    // ---------- King of the Hill (deterministic bonding-progress ranking) ----------

    /// Leaderboard of `Live` launches ranked by bonding progress `real_near / graduation_near`
    /// (basis points, capped at 10000). Deterministic: ties broken by earlier `created_at`, then
    /// by `token_id`. Read-only — no gas cost to traders; the frontend/indexer consumes it.
    pub fn get_king_of_the_hill(&self, limit: Option<u32>) -> Vec<KingEntry> {
        let mut rows: Vec<KingEntry> = self
            .launches
            .values()
            .filter(|l| l.phase == Phase::Live)
            .map(|l| {
                let progress = if l.graduation_near.0 == 0 {
                    0
                } else {
                    curve::mul_div(l.real_near.0, 10_000, l.graduation_near.0).min(10_000) as u32
                };
                KingEntry {
                    token_id: l.token_id.clone(),
                    name: l.name.clone(),
                    symbol: l.symbol.clone(),
                    real_near: l.real_near,
                    graduation_near: l.graduation_near,
                    progress_bps: progress,
                    created_at: l.created_at,
                }
            })
            .collect();
        rows.sort_by(|a, b| {
            b.progress_bps
                .cmp(&a.progress_bps)
                .then(a.created_at.cmp(&b.created_at))
                .then(a.token_id.cmp(&b.token_id))
        });
        rows.truncate(limit.unwrap_or(20) as usize);
        rows
    }

    /// The single current King: the top row of `get_king_of_the_hill`, or `None` if no Live launch.
    pub fn get_king(&self) -> Option<KingEntry> {
        self.get_king_of_the_hill(Some(1)).into_iter().next()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use near_sdk::test_utils::VMContextBuilder;
    use near_sdk::testing_env;

    fn acct(s: &str) -> AccountId {
        s.parse().unwrap()
    }

    /// Set the mocked VM predecessor for the next contract call.
    fn set_predecessor(who: &str) {
        let ctx = VMContextBuilder::new()
            .predecessor_account_id(acct(who))
            .build();
        testing_env!(ctx);
    }

    /// Enter the context of a `#[private]` self-callback (predecessor == current account).
    fn set_self_callback() {
        let ctx = VMContextBuilder::new()
            .current_account_id(acct("router.testnet"))
            .predecessor_account_id(acct("router.testnet"))
            .build();
        testing_env!(ctx);
    }

    fn new_contract(owner: &str) -> Contract {
        set_predecessor(owner);
        Contract::new(acct(owner), acct("treasury"))
    }

    /// Register a 70/20/10 Economy launch: virtual reserve 1e6, on-curve supply 1e6, graduation
    /// threshold 500_000 real wNEAR (small round numbers so both the fee split and the curve output
    /// are exactly predictable, and one 1e6 buy — real_near 990_000 — crosses the threshold).
    fn with_launch(owner: &str, tok: &str, creator: &str) -> Contract {
        let mut c = new_contract(owner);
        set_predecessor(owner);
        c.register_launch(
            acct(tok),
            acct(creator),
            "Test".into(),
            "TST".into(),
            Tier::Economy,
            U128(1_000_000),
            U128(1_000_000),
            U128(500_000),
            7000,
            2000,
            1000,
            None,
        );
        c
    }

    /// Deposit the supply (msg "seed") so `token_reserve` == on-curve supply. Still Seeding.
    fn seed(c: &mut Contract, tok: &str, who: &str) {
        set_predecessor(tok); // the launch token contract calls ft_on_transfer
        let _ = c.ft_on_transfer(acct(who), U128(1_000_000), r#"{"action":"seed","min_out":"0"}"#.into());
    }

    /// A seeded + live launch ready for public buys/sells.
    fn live_launch(owner: &str, tok: &str, creator: &str) -> Contract {
        let mut c = with_launch(owner, tok, creator);
        seed(&mut c, tok, creator);
        set_predecessor(owner);
        c.go_live(acct(tok));
        c
    }

    fn buy_msg(tok: &str, min_out: u128, dev: bool) -> String {
        json!({ "token_id": tok, "min_out": U128(min_out), "dev_buy": dev }).to_string()
    }

    fn sell_msg(min_out: u128) -> String {
        json!({ "action": "sell", "min_out": U128(min_out) }).to_string()
    }

    /// Run a public buy of `amount` wNEAR; assert it filled (returned a delivery promise, not a
    /// refund) and return the contract.
    fn assert_filled(res: PromiseOrValue<U128>) {
        match res {
            PromiseOrValue::Promise(_) => {}
            PromiseOrValue::Value(v) => panic!("expected a fill, got refund of {}", v.0),
        }
    }

    /// Assert the buy/sell was rejected with a full refund of `expected`.
    fn assert_refunded(res: PromiseOrValue<U128>, expected: u128) {
        match res {
            PromiseOrValue::Value(v) => assert_eq!(v.0, expected),
            PromiseOrValue::Promise(_) => panic!("expected a refund, got a fill"),
        }
    }



    #[test]
    fn new_sets_owner_and_default_protocol_bps() {
        let c = new_contract("owner.testnet");
        assert_eq!(c.get_owner(), acct("owner.testnet"));
        assert_eq!(c.get_protocol_bps(), 1000); // 10%
        assert_eq!(c.get_protocol_fees(), U128(0));
        assert_eq!(c.get_num_launches(), 0);
    }

    #[test]
    fn register_launch_stores_immutable_params() {
        let c = with_launch("owner.testnet", "tok.testnet", "creator.testnet");
        assert_eq!(c.get_num_launches(), 1);
        let info = c.get_launch(acct("tok.testnet")).unwrap();
        assert_eq!(info.creator, acct("creator.testnet"));
        assert_eq!(info.tier, Tier::Economy);
        assert_eq!(info.phase, Phase::Seeding);
        assert_eq!(info.virtual_near, U128(1_000_000));
        assert_eq!(info.on_curve_supply, U128(1_000_000));
        assert_eq!(info.graduation_near, U128(500_000));
        assert_eq!(info.token_reserve, U128(0)); // not seeded yet
        assert_eq!(info.real_near, U128(0));
        assert_eq!(info.split.creator_bps, 7000);
        assert_eq!(info.split.buyback_bps, 2000);
        assert_eq!(info.split.holder_bps, 1000);
        assert_eq!(info.fees_collected, U128(0));
        assert_eq!(info.volume, U128(0));
        // no accrual until the first buy
        assert!(c.get_accrual(acct("tok.testnet")).is_none());
    }

    #[test]
    #[should_panic(expected = "launch already registered")]
    fn register_launch_rejects_duplicate() {
        let mut c = with_launch("owner.testnet", "tok.testnet", "creator.testnet");
        set_predecessor("owner.testnet");
        c.register_launch(
            acct("tok.testnet"),
            acct("creator.testnet"),
            "Test".into(),
            "TST".into(),
            Tier::Economy,
            U128(1_000_000),
            U128(1_000_000),
            U128(500_000),
            10000,
            0,
            0,
            None,
        );
    }

    #[test]
    #[should_panic(expected = "fee split must total 100%")]
    fn register_launch_rejects_bad_split() {
        let mut c = new_contract("owner.testnet");
        c.register_launch(
            acct("tok.testnet"),
            acct("creator.testnet"),
            "Test".into(),
            "TST".into(),
            Tier::Economy,
            U128(1_000_000),
            U128(1_000_000),
            U128(500_000),
            7000,
            2000,
            500, // sums to 9500, not 10000
            None,
        );
    }

    #[test]
    #[should_panic(expected = "only the owner can register launches")]
    fn register_launch_rejects_non_owner() {
        let mut c = new_contract("owner.testnet");
        set_predecessor("attacker.testnet"); // squatter trying to claim a token_id
        c.register_launch(
            acct("tok.testnet"),
            acct("attacker.testnet"),
            "Test".into(),
            "TST".into(),
            Tier::Express,
            U128(1_000_000),
            U128(1_000_000),
            U128(500_000),
            10000,
            0,
            0,
            None,
        );
    }

    // ---------- seeding + go-live ----------

    #[test]
    fn seed_sets_the_token_reserve() {
        let mut c = with_launch("owner.testnet", "tok.testnet", "creator.testnet");
        seed(&mut c, "tok.testnet", "creator.testnet");
        let info = c.get_launch(acct("tok.testnet")).unwrap();
        assert_eq!(info.token_reserve, U128(1_000_000));
        assert_eq!(info.phase, Phase::Seeding); // still seeding until go_live
    }

    #[test]
    #[should_panic(expected = "seed must equal the on-curve supply")]
    fn seed_rejects_wrong_amount() {
        let mut c = with_launch("owner.testnet", "tok.testnet", "creator.testnet");
        set_predecessor("tok.testnet");
        let _ = c.ft_on_transfer(acct("creator.testnet"), U128(999), sell_msg(0).replace("sell", "seed"));
    }

    #[test]
    #[should_panic(expected = "only creator or owner can seed")]
    fn seed_rejects_stranger() {
        let mut c = with_launch("owner.testnet", "tok.testnet", "creator.testnet");
        set_predecessor("tok.testnet");
        let _ = c.ft_on_transfer(acct("stranger.testnet"), U128(1_000_000), r#"{"action":"seed","min_out":"0"}"#.into());
    }

    #[test]
    #[should_panic(expected = "curve already seeded")]
    fn seed_rejects_double_seed() {
        let mut c = with_launch("owner.testnet", "tok.testnet", "creator.testnet");
        seed(&mut c, "tok.testnet", "creator.testnet");
        seed(&mut c, "tok.testnet", "creator.testnet");
    }

    #[test]
    #[should_panic(expected = "seed the on-curve supply first")]
    fn go_live_rejects_unseeded() {
        let mut c = with_launch("owner.testnet", "tok.testnet", "creator.testnet");
        set_predecessor("owner.testnet");
        c.go_live(acct("tok.testnet"));
    }

    #[test]
    fn go_live_opens_trading() {
        let c = live_launch("owner.testnet", "tok.testnet", "creator.testnet");
        assert_eq!(c.get_launch(acct("tok.testnet")).unwrap().phase, Phase::Live);
    }

    // ---------- buys ----------

    #[test]
    fn buy_books_fee_and_advances_the_curve() {
        let mut c = live_launch("owner.testnet", "tok.testnet", "creator.testnet");
        set_predecessor(WNEAR);
        assert_filled(c.ft_on_transfer(
            acct("buyer.testnet"),
            U128(1_000_000),
            buy_msg("tok.testnet", 0, false),
        ));
        // fee = 1% of 1_000_000 = 10_000; protocol 10% = 1_000; pool 90% = 9_000 (70/20/10 split)
        assert_eq!(c.get_protocol_fees(), U128(1_000));
        let a = c.get_accrual(acct("tok.testnet")).unwrap();
        assert_eq!(a.creator, U128(6_300));
        assert_eq!(a.buyback, U128(1_800));
        assert_eq!(a.holder, U128(900));
        // curve: near_in = 990_000, tokens_out = 990_000*1_000_000/1_990_000 = 497_487
        let info = c.get_launch(acct("tok.testnet")).unwrap();
        assert_eq!(info.fees_collected, U128(9_000));
        assert_eq!(info.real_near, U128(990_000));
        assert_eq!(info.token_reserve, U128(1_000_000 - 497_487));
        assert_eq!(info.volume, U128(1_000_000)); // real wNEAR traded
    }

    #[test]
    #[should_panic(expected = "trading is not live")]
    fn buy_rejected_before_go_live() {
        let mut c = with_launch("owner.testnet", "tok.testnet", "creator.testnet");
        seed(&mut c, "tok.testnet", "creator.testnet"); // seeded but not live
        set_predecessor(WNEAR);
        let _ = c.ft_on_transfer(acct("buyer.testnet"), U128(1_000_000), buy_msg("tok.testnet", 0, false));
    }

    #[test]
    fn buy_below_min_out_refunds_in_full() {
        let mut c = live_launch("owner.testnet", "tok.testnet", "creator.testnet");
        set_predecessor(WNEAR);
        // demand more tokens than the curve can deliver → full refund, no state change
        assert_refunded(
            c.ft_on_transfer(acct("buyer.testnet"), U128(1_000_000), buy_msg("tok.testnet", 999_999_999, false)),
            1_000_000,
        );
        assert_eq!(c.get_protocol_fees(), U128(0));
        assert_eq!(c.get_launch(acct("tok.testnet")).unwrap().real_near, U128(0));
    }

    #[test]
    fn buy_delivery_failure_rolls_back_and_refunds() {
        let mut c = live_launch("owner.testnet", "tok.testnet", "creator.testnet");
        set_predecessor(WNEAR);
        assert_filled(c.ft_on_transfer(acct("buyer.testnet"), U128(1_000_000), buy_msg("tok.testnet", 0, false)));
        // the token delivery leg fails → callback unwinds everything and refunds the wNEAR
        set_self_callback();
        let refund = c.on_buy_deliver(
            acct("tok.testnet"),
            U128(1_000_000),
            U128(990_000),
            U128(497_487),
            U128(10_000),
            false,
            Err(PromiseError::Failed),
        );
        assert_eq!(refund, U128(1_000_000));
        assert_eq!(c.get_protocol_fees(), U128(0));
        let info = c.get_launch(acct("tok.testnet")).unwrap();
        assert_eq!(info.real_near, U128(0));
        assert_eq!(info.token_reserve, U128(1_000_000)); // reserve restored
        assert_eq!(info.volume, U128(0));
    }

    #[test]
    fn buy_delivery_success_keeps_the_wnear() {
        let mut c = live_launch("owner.testnet", "tok.testnet", "creator.testnet");
        set_predecessor(WNEAR);
        assert_filled(c.ft_on_transfer(acct("buyer.testnet"), U128(1_000_000), buy_msg("tok.testnet", 0, false)));
        set_self_callback();
        let refund = c.on_buy_deliver(
            acct("tok.testnet"),
            U128(1_000_000),
            U128(990_000),
            U128(497_487),
            U128(10_000),
            false,
            Ok(()),
        );
        assert_eq!(refund, U128(0)); // delivered — no wNEAR refunded
        assert_eq!(c.get_protocol_fees(), U128(1_000)); // fee kept
    }

    // ---------- dev buy (anti-rug 4% cap) ----------

    #[test]
    fn dev_buy_within_cap_succeeds() {
        let mut c = with_launch("owner.testnet", "tok.testnet", "creator.testnet");
        seed(&mut c, "tok.testnet", "creator.testnet"); // dev buy happens pre-launch (Seeding)
        set_predecessor(WNEAR);
        // amount 10_000 → near_in 9_900 → out = 1e6*9_900/1_009_900 = 9_802 (< 40_000 cap)
        assert_filled(c.ft_on_transfer(acct("creator.testnet"), U128(10_000), buy_msg("tok.testnet", 0, true)));
        let info = c.get_launch(acct("tok.testnet")).unwrap();
        assert_eq!(info.dev_bought, U128(9_802));
    }

    #[test]
    fn dev_buy_over_4pct_cap_is_refunded() {
        let mut c = with_launch("owner.testnet", "tok.testnet", "creator.testnet");
        seed(&mut c, "tok.testnet", "creator.testnet");
        set_predecessor(WNEAR);
        // a 1_000_000 buy would yield 497_487 tokens ≫ 40_000 (4% of 1e6) → refused
        assert_refunded(
            c.ft_on_transfer(acct("creator.testnet"), U128(1_000_000), buy_msg("tok.testnet", 0, true)),
            1_000_000,
        );
        assert_eq!(c.get_launch(acct("tok.testnet")).unwrap().dev_bought, U128(0));
    }

    #[test]
    #[should_panic(expected = "only the creator can dev buy")]
    fn dev_buy_rejects_non_creator() {
        let mut c = with_launch("owner.testnet", "tok.testnet", "creator.testnet");
        seed(&mut c, "tok.testnet", "creator.testnet");
        set_predecessor(WNEAR);
        let _ = c.ft_on_transfer(acct("someone.testnet"), U128(10_000), buy_msg("tok.testnet", 0, true));
    }

    #[test]
    #[should_panic(expected = "dev buy is only allowed before launch")]
    fn dev_buy_rejected_after_go_live() {
        let mut c = live_launch("owner.testnet", "tok.testnet", "creator.testnet");
        set_predecessor(WNEAR);
        let _ = c.ft_on_transfer(acct("creator.testnet"), U128(10_000), buy_msg("tok.testnet", 0, true));
    }

    // ---------- sells ----------

    #[test]
    fn sell_pays_wnear_and_reverses_the_curve() {
        let mut c = live_launch("owner.testnet", "tok.testnet", "creator.testnet");
        set_predecessor(WNEAR);
        assert_filled(c.ft_on_transfer(acct("buyer.testnet"), U128(1_000_000), buy_msg("tok.testnet", 0, false)));
        // now real_near=990_000, reserve=502_513; sell the 497_487 tokens back
        set_predecessor("tok.testnet");
        assert_filled(c.ft_on_transfer(acct("buyer.testnet"), U128(497_487), sell_msg(0)));
        let info = c.get_launch(acct("tok.testnet")).unwrap();
        // gross out = 1_990_000*497_487/1_000_000 = 989_999 (< real_near cap); real_near left = 1
        assert_eq!(info.real_near, U128(1));
        assert_eq!(info.token_reserve, U128(1_000_000)); // tokens returned to the curve
        assert!(info.volume.0 > 1_000_000); // buy + sell gross both counted
    }

    #[test]
    fn sell_can_never_drain_more_than_real_wnear() {
        let mut c = live_launch("owner.testnet", "tok.testnet", "creator.testnet");
        set_predecessor(WNEAR);
        assert_filled(c.ft_on_transfer(acct("buyer.testnet"), U128(1_000_000), buy_msg("tok.testnet", 0, false)));
        // dump a huge amount of tokens — payout is capped at the 990_000 real wNEAR on the curve
        set_predecessor("tok.testnet");
        assert_filled(c.ft_on_transfer(acct("whale.testnet"), U128(10_000_000), sell_msg(0)));
        assert_eq!(c.get_launch(acct("tok.testnet")).unwrap().real_near, U128(0));
    }

    #[test]
    fn sell_payout_failure_returns_tokens_and_restores_curve() {
        let mut c = live_launch("owner.testnet", "tok.testnet", "creator.testnet");
        set_predecessor(WNEAR);
        assert_filled(c.ft_on_transfer(acct("buyer.testnet"), U128(1_000_000), buy_msg("tok.testnet", 0, false)));
        set_predecessor("tok.testnet");
        assert_filled(c.ft_on_transfer(acct("buyer.testnet"), U128(497_487), sell_msg(0)));
        let before = c.get_launch(acct("tok.testnet")).unwrap();
        assert_eq!(before.token_reserve, U128(1_000_000));
        // the wNEAR payout leg fails → seller's tokens are returned, curve rolled back
        set_self_callback();
        let refund = c.on_sell_payout(
            acct("tok.testnet"),
            U128(497_487),
            U128(989_999),
            U128(9_899),
            Err(PromiseError::Failed),
        );
        assert_eq!(refund, U128(497_487)); // all tokens returned to the seller
        let after = c.get_launch(acct("tok.testnet")).unwrap();
        assert_eq!(after.real_near, U128(990_000)); // restored
        assert_eq!(after.token_reserve, U128(502_513)); // restored
    }

    #[test]
    #[should_panic(expected = "trading is not live")]
    fn sell_rejected_before_go_live() {
        let mut c = with_launch("owner.testnet", "tok.testnet", "creator.testnet");
        seed(&mut c, "tok.testnet", "creator.testnet");
        set_predecessor("tok.testnet");
        let _ = c.ft_on_transfer(acct("seller.testnet"), U128(1_000), sell_msg(0));
    }

    // ---------- graduation ----------

    /// Drive real_near over the 500_000 threshold with one 1_000_000 buy (real_near → 990_000).
    fn ready_to_graduate() -> Contract {
        let mut c = live_launch("owner.testnet", "tok.testnet", "creator.testnet");
        set_predecessor(WNEAR);
        assert_filled(c.ft_on_transfer(
            acct("buyer.testnet"),
            U128(1_000_000),
            buy_msg("tok.testnet", 0, false),
        ));
        c
    }

    #[test]
    fn graduate_closes_the_curve_once_threshold_is_met() {
        let mut c = ready_to_graduate();
        set_predecessor("anyone.testnet"); // permissionless once the threshold is reached
        let info = c.graduate(acct("tok.testnet"));
        assert_eq!(info.phase, Phase::Graduated);
        // leftover tokens + real wNEAR stay held by the router as the earmarked pool seed
        assert_eq!(info.real_near, U128(990_000));
        assert_eq!(info.token_reserve, U128(502_513));
    }

    #[test]
    #[should_panic(expected = "graduation threshold not reached")]
    fn graduate_rejected_below_threshold() {
        // small buy: real_near = 100_000*0.99 = 99_000 < 500_000 threshold
        let mut c = live_launch("owner.testnet", "tok.testnet", "creator.testnet");
        set_predecessor(WNEAR);
        assert_filled(c.ft_on_transfer(acct("buyer.testnet"), U128(100_000), buy_msg("tok.testnet", 0, false)));
        set_predecessor("anyone.testnet");
        let _ = c.graduate(acct("tok.testnet"));
    }

    #[test]
    #[should_panic(expected = "launch is not live")]
    fn graduate_rejected_before_go_live() {
        let mut c = with_launch("owner.testnet", "tok.testnet", "creator.testnet");
        seed(&mut c, "tok.testnet", "creator.testnet");
        set_predecessor("anyone.testnet");
        let _ = c.graduate(acct("tok.testnet")); // still Seeding
    }

    #[test]
    #[should_panic(expected = "launch is not live")]
    fn graduate_is_not_repeatable() {
        let mut c = ready_to_graduate();
        set_predecessor("anyone.testnet");
        let _ = c.graduate(acct("tok.testnet"));
        let _ = c.graduate(acct("tok.testnet")); // already Graduated -> rejected
    }

    #[test]
    #[should_panic(expected = "trading is not live")]
    fn buy_rejected_after_graduation() {
        let mut c = ready_to_graduate();
        set_predecessor("anyone.testnet");
        let _ = c.graduate(acct("tok.testnet"));
        set_predecessor(WNEAR);
        let _ = c.ft_on_transfer(acct("buyer.testnet"), U128(1_000), buy_msg("tok.testnet", 0, false));
    }

    #[test]
    #[should_panic(expected = "trading is not live")]
    fn sell_rejected_after_graduation() {
        let mut c = ready_to_graduate();
        set_predecessor("anyone.testnet");
        let _ = c.graduate(acct("tok.testnet"));
        set_predecessor("tok.testnet");
        let _ = c.ft_on_transfer(acct("buyer.testnet"), U128(1_000), sell_msg(0));
    }

    // ---------- graduation threshold setter (test-enablement knob) ----------

    #[test]
    fn set_graduation_near_lets_owner_retune_a_live_launch() {
        // Default threshold is 500_000. A 100_000 buy (real_near 99_000) is below it, so graduate
        // would reject. The owner lowers the threshold, and now the same curve state graduates.
        let mut c = live_launch("owner.testnet", "tok.testnet", "creator.testnet");
        set_predecessor(WNEAR);
        assert_filled(c.ft_on_transfer(acct("buyer.testnet"), U128(100_000), buy_msg("tok.testnet", 0, false)));
        set_predecessor("owner.testnet");
        c.set_graduation_near(acct("tok.testnet"), U128(50_000));
        assert_eq!(c.get_launch(acct("tok.testnet")).unwrap().graduation_near, U128(50_000));
        set_predecessor("anyone.testnet");
        let info = c.graduate(acct("tok.testnet")); // real_near 99_000 >= 50_000 now
        assert_eq!(info.phase, Phase::Graduated);
    }

    #[test]
    #[should_panic(expected = "only the owner can set the graduation threshold")]
    fn set_graduation_near_rejects_non_owner() {
        let mut c = live_launch("owner.testnet", "tok.testnet", "creator.testnet");
        set_predecessor("attacker.testnet");
        c.set_graduation_near(acct("tok.testnet"), U128(1));
    }

    #[test]
    #[should_panic(expected = "graduation threshold must be positive")]
    fn set_graduation_near_rejects_zero() {
        let mut c = live_launch("owner.testnet", "tok.testnet", "creator.testnet");
        set_predecessor("owner.testnet");
        c.set_graduation_near(acct("tok.testnet"), U128(0));
    }

    #[test]
    #[should_panic(expected = "launch has already left the curve")]
    fn set_graduation_near_rejects_after_graduation() {
        let mut c = graduated(); // phase == Graduated
        set_predecessor("owner.testnet");
        c.set_graduation_near(acct("tok.testnet"), U128(1));
    }

    // ---------- pool seeding (DCL migration) ----------

    /// A graduated launch (curve closed): leftover token_reserve 502_513, real_near 990_000.
    fn graduated() -> Contract {
        let mut c = ready_to_graduate();
        set_predecessor("anyone.testnet");
        let _ = c.graduate(acct("tok.testnet"));
        c
    }

    /// Owner context with ample balance so the promise batch's attached deposits are covered.
    fn set_owner_with_balance() {
        let ctx = VMContextBuilder::new()
            .current_account_id(acct("router.testnet"))
            .predecessor_account_id(acct("owner.testnet"))
            .account_balance(NearToken::from_near(10))
            .build();
        testing_env!(ctx);
    }

    #[test]
    fn seed_pool_moves_graduated_launch_into_pooling() {
        let mut c = graduated();
        set_owner_with_balance();
        let _ = c.seed_pool(acct("tok.testnet")); // points now derived on-chain
        // in-flight lock: phase is Pooling until on_pool_seeded resolves
        assert_eq!(c.get_launch(acct("tok.testnet")).unwrap().phase, Phase::Pooling);
    }

    /// MEDIUM-1: seed_pool is permissionless — any caller (keeper, indexer, stranded holder), not
    /// just the owner, can migrate a Graduated launch, so a negligent/lost-key owner can never
    /// strand holders in Graduated forever.
    #[test]
    fn seed_pool_permissionless_any_caller_can_seed() {
        let mut c = graduated();
        // A random non-owner with balance to cover the saga's attached deposits.
        let ctx = VMContextBuilder::new()
            .current_account_id(acct("router.testnet"))
            .predecessor_account_id(acct("keeper.testnet"))
            .account_balance(NearToken::from_near(10))
            .build();
        testing_env!(ctx);
        let _ = c.seed_pool(acct("tok.testnet"));
        assert_eq!(c.get_launch(acct("tok.testnet")).unwrap().phase, Phase::Pooling);
    }

    #[test]
    #[should_panic(expected = "launch has not graduated")]
    fn seed_pool_rejects_before_graduation() {
        let mut c = ready_to_graduate(); // Live, not graduated
        set_owner_with_balance();
        let _ = c.seed_pool(acct("tok.testnet"));
    }

    /// MEDIUM-1 pin: `init_point` is derived from the graduation price (`real_near / token_reserve`),
    /// aligned to `point_delta` 40, and inverts when the launch sorts as `token_y`. graduated()'s
    /// fixture is `real_near = 990_000`, `token_reserve = 502_513` -> price 1.9701 -> 6781.17 raw ->
    /// 6800 aligned. Deterministic, no owner discretion.
    #[test]
    fn seed_pool_derives_init_point_from_graduation_price() {
        // launch == token_x (launch < wNEAR): point = log_1.0001(wNEAR per launch token).
        assert_eq!(derive_init_point(990_000, 502_513, true), 6800);
        // launch == token_y (launch > wNEAR): the pool price inverts, so the point negates.
        assert_eq!(derive_init_point(990_000, 502_513, false), -6800);
        // A launch-token price below 1 wNEAR yields a negative x-point (matches the live testnet run).
        assert_eq!(
            derive_init_point(1_485 * 10u128.pow(21), 870_700 * 10u128.pow(27), true),
            -201_920
        );
    }

    #[test]
    fn seed_pool_add_liquidity_request_leaves_headroom() {
        // Live reserve scale (the exact 2026-09-27 leg-7 deposit): request = reserve − reserve/1e9,
        // and the headroom must clear DCL's observed round-up (~6.996e11 yocto) by wide margin so
        // paid_token ≤ deposited holds and DCL never underflows its internal balance.
        let amount = 726_572_861_521_315_129_562_552_242_964_616u128;
        let requested = add_liquidity_request(amount);
        let headroom = amount - requested;
        assert_eq!(requested, amount - amount / 1_000_000_000, "request = deposit − deposit/1e9");
        assert_eq!(headroom, amount / 1_000_000_000, "headroom = deposit/1e9");
        assert!(headroom > 699_614_115_686, "headroom must exceed the observed live round-up");
        assert!(requested < amount, "must request strictly less than deposited");
        // Test-scale (sub-1e9) amounts: integer division floors headroom to 0, so the mock-DCL
        // integration path is unchanged (it deposits and requests the same small amount).
        assert_eq!(add_liquidity_request(1_000_000), 1_000_000, "tiny amounts: no headroom carved");
        assert_eq!(add_liquidity_request(0), 0);
    }

    #[test]
    #[should_panic(expected = "launch has not graduated (or is already pooling/pooled)")]
    fn seed_pool_is_not_repeatable_while_pooling() {
        let mut c = graduated();
        set_owner_with_balance();
        let _ = c.seed_pool(acct("tok.testnet")); // → Pooling
        let _ = c.seed_pool(acct("tok.testnet")); // re-fire blocked
    }

    #[test]
    fn on_pool_seeded_success_marks_pooled_and_clears_reserve() {
        let mut c = graduated();
        set_owner_with_balance();
        let _ = c.seed_pool(acct("tok.testnet"));
        set_self_callback();
        let ok = c.on_pool_seeded(
            acct("tok.testnet"),
            U128(502_513), // the migrated token_reserve
            Ok(json!("tok.testnet|wrap.testnet|2000#0")), // lpt id from add_liquidity
        );
        assert!(ok);
        let info = c.get_launch(acct("tok.testnet")).unwrap();
        assert_eq!(info.phase, Phase::Pooled);
        assert_eq!(info.token_reserve, U128(0)); // tokens now in the locked DCL position
        assert_eq!(info.real_near, U128(990_000)); // real wNEAR stays with the router
    }

    #[test]
    fn on_pool_seeded_failure_reverts_to_graduated_for_retry() {
        let mut c = graduated();
        set_owner_with_balance();
        let _ = c.seed_pool(acct("tok.testnet"));
        set_self_callback();
        let ok = c.on_pool_seeded(acct("tok.testnet"), U128(502_513), Err(PromiseError::Failed));
        assert!(!ok);
        let info = c.get_launch(acct("tok.testnet")).unwrap();
        assert_eq!(info.phase, Phase::Graduated); // reverted — retryable
        assert_eq!(info.token_reserve, U128(502_513)); // untouched on failure
    }

    // ---------- MEDIUM-2: resumable / idempotent seed_pool retry ----------

    /// The per-leg progress callbacks advance `seed_stage` only on confirmed success, and only in
    /// order (deposit cannot record before the pool exists). This is the state a retry resumes from.
    #[test]
    fn seed_progress_callbacks_advance_stage_in_order() {
        let mut c = graduated();
        set_owner_with_balance();
        let _ = c.seed_pool(acct("tok.testnet"));
        set_self_callback();

        // A deposit callback BEFORE create is confirmed must NOT skip ahead (order invariant).
        c.on_seed_deposit(acct("tok.testnet"), U128(502_513), Ok(U128(502_513)));
        assert_eq!(c.get_launch(acct("tok.testnet")).unwrap().seed_stage, SeedStage::NotStarted);

        // create confirmed -> PoolCreated.
        c.on_seed_create(acct("tok.testnet"), Ok(json!("tok.testnet|wrap.testnet|2000")));
        assert_eq!(c.get_launch(acct("tok.testnet")).unwrap().seed_stage, SeedStage::PoolCreated);

        // A partial/refunded deposit (used < amount, e.g. E600) must NOT record Deposited.
        c.on_seed_deposit(acct("tok.testnet"), U128(502_513), Ok(U128(0)));
        assert_eq!(c.get_launch(acct("tok.testnet")).unwrap().seed_stage, SeedStage::PoolCreated);

        // Full deposit landed -> Deposited.
        c.on_seed_deposit(acct("tok.testnet"), U128(502_513), Ok(U128(502_513)));
        assert_eq!(c.get_launch(acct("tok.testnet")).unwrap().seed_stage, SeedStage::Deposited);

        // A failed create callback must never regress a recorded stage.
        c.on_seed_create(acct("tok.testnet"), Err(PromiseError::Failed));
        assert_eq!(c.get_launch(acct("tok.testnet")).unwrap().seed_stage, SeedStage::Deposited);
    }

    /// End-to-end MEDIUM-2 fix at the unit level: create+deposit succeed, add_liquidity fails, the
    /// launch reverts to Graduated with `seed_stage` PRESERVED, then a retry resumes from Deposited
    /// and completes to Pooled with the reserve cleared. Previously the retry re-ran the whole saga
    /// (re-create traps, re-deposit double-moves) so it could never complete — the KNOWN-BROKEN pin.
    #[test]
    fn seed_pool_partial_fail_then_retry_resumes_to_pooled() {
        let mut c = graduated();
        set_owner_with_balance();

        // First attempt: the saga runs create + deposit (both confirm) then add_liquidity FAILS.
        let _ = c.seed_pool(acct("tok.testnet"));
        set_self_callback();
        c.on_seed_create(acct("tok.testnet"), Ok(json!("tok.testnet|wrap.testnet|2000")));
        c.on_seed_deposit(acct("tok.testnet"), U128(502_513), Ok(U128(502_513)));
        let ok = c.on_pool_seeded(acct("tok.testnet"), U128(502_513), Err(PromiseError::Failed));
        assert!(!ok);
        let info = c.get_launch(acct("tok.testnet")).unwrap();
        assert_eq!(info.phase, Phase::Graduated); // retryable
        assert_eq!(info.token_reserve, U128(502_513)); // reserve intact — tokens parked in DCL
        assert_eq!(info.seed_stage, SeedStage::Deposited); // progress PRESERVED across the failure

        // Retry: seed_pool is called again on the same (Graduated) launch. It resumes from Deposited
        // — the guard still passes and the stage is unchanged, so create/deposit are skipped.
        set_owner_with_balance();
        let _ = c.seed_pool(acct("tok.testnet"));
        let info = c.get_launch(acct("tok.testnet")).unwrap();
        assert_eq!(info.phase, Phase::Pooling);
        assert_eq!(info.seed_stage, SeedStage::Deposited); // still Deposited; nothing re-run

        // This time add_liquidity succeeds -> Pooled, reserve cleared into the locked position.
        set_self_callback();
        let ok = c.on_pool_seeded(
            acct("tok.testnet"),
            U128(502_513),
            Ok(json!("tok.testnet|wrap.testnet|2000#0")),
        );
        assert!(ok);
        let info = c.get_launch(acct("tok.testnet")).unwrap();
        assert_eq!(info.phase, Phase::Pooled);
        assert_eq!(info.token_reserve, U128(0));
    }

    /// Graduate an arbitrary token id (so we can exercise the token_y ordering branch).
    fn graduated_token(tok: &str) -> Contract {
        let mut c = live_launch("owner.testnet", tok, "creator.testnet");
        set_predecessor(WNEAR);
        assert_filled(c.ft_on_transfer(acct("buyer.testnet"), U128(1_000_000), buy_msg(tok, 0, false)));
        set_predecessor("anyone.testnet");
        let _ = c.graduate(acct(tok));
        c
    }

    #[test]
    fn seed_pool_token_y_derives_below_spot_range() {
        // "zap.testnet" > "wrap.testnet" -> launch is token_y -> derived range sits strictly BELOW spot.
        let mut c = graduated_token("zap.testnet");
        set_owner_with_balance();
        let _ = c.seed_pool(acct("zap.testnet"));
        assert_eq!(c.get_launch(acct("zap.testnet")).unwrap().phase, Phase::Pooling);
    }

    // ---------- dispatch guards ----------

    #[test]
    #[should_panic(expected = "unknown launch token")]
    fn buy_rejects_unknown_token() {
        let mut c = live_launch("owner.testnet", "tok.testnet", "creator.testnet");
        set_predecessor(WNEAR);
        let _ = c.ft_on_transfer(acct("buyer.testnet"), U128(1_000_000), buy_msg("ghost.testnet", 0, false));
    }

    #[test]
    #[should_panic(expected = "unknown launch token")]
    fn token_hook_rejects_unregistered_token() {
        let mut c = live_launch("owner.testnet", "tok.testnet", "creator.testnet");
        set_predecessor("random-token.testnet"); // not a registered launch
        let _ = c.ft_on_transfer(acct("seller.testnet"), U128(1_000), sell_msg(0));
    }

    #[test]
    fn quotes_match_the_curve() {
        let c = live_launch("owner.testnet", "tok.testnet", "creator.testnet");
        // buy quote: near_in 990_000 → 497_487 tokens
        assert_eq!(c.quote_buy(acct("tok.testnet"), U128(1_000_000)), U128(497_487));
        // sell quote on an empty real reserve is 0 (nothing real to pay out)
        assert_eq!(c.quote_sell(acct("tok.testnet"), U128(100_000)), U128(0));
    }

    // ---------- claims ----------

    /// register + seed + go-live + one 1_000_000 buy, leaving creator=6_300, protocol=1_000.
    fn funded() -> Contract {
        let mut c = live_launch("owner.testnet", "tok.testnet", "creator.testnet");
        set_predecessor(WNEAR);
        assert_filled(c.ft_on_transfer(
            acct("buyer.testnet"),
            U128(1_000_000),
            buy_msg("tok.testnet", 0, false),
        ));
        c
    }

    #[test]
    fn claim_creator_fees_zeroes_the_creator_bucket() {
        let mut c = funded();
        set_predecessor("creator.testnet");
        let _ = c.claim_creator_fees(acct("tok.testnet"));
        // optimistically zeroed; buyback/holder untouched
        let a = c.get_accrual(acct("tok.testnet")).unwrap();
        assert_eq!(a.creator, U128(0));
        assert_eq!(a.buyback, U128(1_800));
        assert_eq!(a.holder, U128(900));
    }

    #[test]
    #[should_panic(expected = "only the launch creator can claim")]
    fn claim_creator_fees_rejects_non_creator() {
        let mut c = funded();
        set_predecessor("someone.testnet");
        let _ = c.claim_creator_fees(acct("tok.testnet"));
    }

    #[test]
    #[should_panic(expected = "nothing to claim")]
    fn claim_creator_fees_rejects_empty() {
        let mut c = funded();
        set_predecessor("creator.testnet");
        let _ = c.claim_creator_fees(acct("tok.testnet")); // drains creator bucket
        let _ = c.claim_creator_fees(acct("tok.testnet")); // now empty -> panics
    }

    #[test]
    fn failed_creator_payout_restores_accrual() {
        let mut c = funded();
        set_predecessor("creator.testnet");
        let _ = c.claim_creator_fees(acct("tok.testnet")); // zeroes to 0
        set_self_callback();
        c.on_creator_claim(acct("tok.testnet"), U128(6_300), Err(PromiseError::Failed));
        assert_eq!(c.get_accrual(acct("tok.testnet")).unwrap().creator, U128(6_300));
    }

    #[test]
    fn claim_protocol_fees_owner_only_and_zeroes() {
        let mut c = funded();
        set_predecessor("creator.testnet");
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            c.claim_protocol_fees()
        }));
        assert!(caught.is_err(), "non-owner must not claim protocol fees");
        set_predecessor("owner.testnet");
        let _ = c.claim_protocol_fees();
        assert_eq!(c.get_protocol_fees(), U128(0));
    }

    // ==================== STRESS TESTS (user-safety hardening) ====================
    // Three risk classes, hardened one by one:
    //   1) arithmetic / overflow / rounding   2) access control   3) reentrancy / rollback

    /// 1 token/wNEAR with 24 decimals — launchpad-scale magnitudes.
    const E24: u128 = 1_000_000_000_000_000_000_000_000;

    /// Register a launch with fully custom magnitudes + split (no hardcoded 1e6 like `with_launch`).
    fn reg_custom(
        c: &mut Contract, owner: &str, tok: &str, creator: &str,
        vnear: u128, supply: u128, grad: u128, cr: u16, bb: u16, hd: u16,
    ) {
        set_predecessor(owner);
        c.register_launch(
            acct(tok), acct(creator), "S".into(), "S".into(), Tier::Economy,
            U128(vnear), U128(supply), U128(grad), cr, bb, hd, None,
        );
    }

    /// A seeded + live launch at custom magnitudes, ready for public buys/sells.
    fn live_custom(
        owner: &str, tok: &str, creator: &str,
        vnear: u128, supply: u128, grad: u128, cr: u16, bb: u16, hd: u16,
    ) -> Contract {
        let mut c = new_contract(owner);
        reg_custom(&mut c, owner, tok, creator, vnear, supply, grad, cr, bb, hd);
        set_predecessor(tok);
        let _ = c.ft_on_transfer(
            acct(creator), U128(supply), r#"{"action":"seed","min_out":"0"}"#.into(),
        );
        set_predecessor(owner);
        c.go_live(acct(tok));
        c
    }

    // ---------- POINT 1: arithmetic / overflow / rounding ----------

    #[test]
    fn stress_buy_at_launchpad_magnitude_no_overflow() {
        // 1B tokens (24 dec) = 1e33 on-curve, 5000 wNEAR virtual = 5e27, buy 1000 wNEAR gross.
        // The u128 product token_reserve * near_in is ~1e60 — this only survives via U256 mul_div.
        let supply = 1_000_000_000u128 * E24;
        let mut c = live_custom(
            "owner.testnet", "tok.testnet", "creator.testnet",
            5_000 * E24, supply, 3_000 * E24, 7000, 2000, 1000,
        );
        set_predecessor(WNEAR);
        assert_filled(c.ft_on_transfer(
            acct("buyer.testnet"), U128(1_000 * E24), buy_msg("tok.testnet", 0, false),
        ));
        let info = c.get_launch(acct("tok.testnet")).unwrap();
        let fee = 1_000 * E24 / 100; // FEE_BPS = 100 => exactly 1%
        // real_near is the 99% that stays on the curve; tokens delivered but never the whole reserve.
        assert_eq!(info.real_near, U128(1_000 * E24 - fee));
        assert!(info.token_reserve.0 > 0 && info.token_reserve.0 < supply);
        // Value conservation: protocol + pool == fee, and the three buckets == pool (no leak/mint).
        let a = c.get_accrual(acct("tok.testnet")).unwrap();
        assert_eq!(
            c.get_protocol_fees().0 + info.fees_collected.0, fee,
            "protocol cut + pool cut must equal the total fee",
        );
        assert_eq!(
            a.creator.0 + a.buyback.0 + a.holder.0, info.fees_collected.0,
            "creator+buyback+holder must equal the pool cut exactly",
        );
    }

    #[test]
    fn stress_fee_split_never_creates_or_destroys_value() {
        // Awkward splits + awkward amounts: buckets + protocol must always re-sum to the exact fee,
        // protocol always takes floor(fee/10), and the holder bucket absorbs every rounding remainder.
        // (Unique token id per case: the mocked VM storage persists across contracts in one test.)
        let mut n = 0u32;
        for (cr, bb, hd) in [(3333u16, 3333, 3334), (1, 1, 9998), (10000, 0, 0), (0, 0, 10000)] {
            for gross in [100u128, 101, 199, 999_999, 123_456_789, 1_000 * E24 + 7] {
                n += 1;
                let tok = format!("r{}.testnet", n);
                let mut c = live_custom(
                    "owner.testnet", &tok, "creator.testnet",
                    5_000 * E24, 1_000_000_000u128 * E24, 3_000 * E24, cr, bb, hd,
                );
                set_predecessor(WNEAR);
                let _ = c.ft_on_transfer(acct("b.testnet"), U128(gross), buy_msg(&tok, 0, false));
                let info = c.get_launch(acct(&tok)).unwrap();
                let (bc, bbk, bh) = c
                    .get_accrual(acct(&tok))
                    .map(|a| (a.creator.0, a.buyback.0, a.holder.0))
                    .unwrap_or((0, 0, 0));
                let fee = gross / 100;
                let protocol = c.get_protocol_fees().0;
                assert_eq!(protocol, fee / 10, "protocol takes floor(fee/10), rounding favors pool");
                assert_eq!(protocol + info.fees_collected.0, fee, "protocol + pool == fee");
                assert_eq!(bc + bbk + bh, info.fees_collected.0, "buckets == pool (no value leak)");
            }
        }
    }

    // ---------- POINT 2: access control ----------
    // The 5 `#[private]` callbacks (on_pool_seeded/on_buy_deliver/on_sell_payout/on_creator_claim/
    // on_protocol_claim) are guarded by near-sdk's caller check, which the macro emits ONLY into the
    // wasm extern wrapper — so it is verified statically (grep confirms all 5 carry #[private]),
    // not here (a unit test calls the inner fn directly and would bypass the wasm-only guard).
    // The privileged entrypoints below use explicit runtime require!()s, so they ARE tested here.

    #[test]
    #[should_panic(expected = "only owner or creator can open trading")]
    fn stress_go_live_rejects_stranger() {
        let mut c = with_launch("owner.testnet", "tok.testnet", "creator.testnet");
        seed(&mut c, "tok.testnet", "creator.testnet");
        set_predecessor("attacker.testnet");
        c.go_live(acct("tok.testnet"));
    }

    // ---------- POINT 3: reentrancy / async rollback ----------

    #[test]
    fn stress_interleaved_buy_rollback_removes_only_its_own_footprint() {
        // Two buys advance the curve; then buy #1's delivery fails. The optimistic-state unwind is
        // additive (unbook_fee + saturating restore), so it must strip EXACTLY buy #1's footprint
        // and leave buy #2 fully intact — even though buy #2 was priced against buy #1's state.
        let mut c = live_launch("owner.testnet", "tok.testnet", "creator.testnet");
        set_predecessor(WNEAR);
        assert_filled(c.ft_on_transfer(acct("b1.testnet"), U128(200_000), buy_msg("tok.testnet", 0, false)));
        set_predecessor(WNEAR);
        assert_filled(c.ft_on_transfer(acct("b2.testnet"), U128(300_000), buy_msg("tok.testnet", 0, false)));
        // Fail buy #1's callback (gross 200_000 -> fee 2_000, near_in 198_000, out 165_275).
        set_self_callback();
        let refund = c.on_buy_deliver(
            acct("tok.testnet"), U128(200_000), U128(198_000), U128(165_275), U128(2_000),
            false, Err(PromiseError::Failed),
        );
        assert_eq!(refund, U128(200_000), "failed buy refunds the full wNEAR sent");
        let info = c.get_launch(acct("tok.testnet")).unwrap();
        let a = c.get_accrual(acct("tok.testnet")).unwrap();
        // Only buy #2 (gross 300_000 -> fee 3_000, near_in 297_000, out 165_828) survives.
        assert_eq!(info.real_near, U128(297_000));
        assert_eq!(info.token_reserve, U128(1_000_000 - 165_828));
        assert_eq!(info.fees_collected, U128(2_700));
        assert_eq!(info.volume, U128(300_000));
        assert_eq!(c.get_protocol_fees(), U128(300));
        assert_eq!((a.creator.0, a.buyback.0, a.holder.0), (1_890, 540, 270));
    }

    #[test]
    #[should_panic(expected = "nothing to claim")]
    fn stress_no_double_claim_after_successful_payout() {
        // Claim zeroes the bucket optimistically; a SUCCESSFUL callback keeps it zeroed. A second
        // claim must then find nothing — the success path leaves no window to double-spend fees.
        let mut c = funded();
        set_predecessor("creator.testnet");
        let _ = c.claim_creator_fees(acct("tok.testnet"));
        set_self_callback();
        c.on_creator_claim(acct("tok.testnet"), U128(6_300), Ok(())); // success: no restore
        set_predecessor("creator.testnet");
        let _ = c.claim_creator_fees(acct("tok.testnet")); // empty -> panics
    }

    #[test]
    fn stress_failed_buy_fully_unwinds_at_magnitude() {
        // A big buy with an awkward 3333/3333/3334 split, then a failed delivery. Every bucket must
        // return to exactly zero — no dust survives the unbook at launchpad magnitude.
        let supply = 1_000_000_000u128 * E24;
        let mut c = live_custom(
            "owner.testnet", "tok.testnet", "creator.testnet",
            5_000 * E24, supply, 3_000 * E24, 3333, 3333, 3334,
        );
        set_predecessor(WNEAR);
        let gross = 777 * E24 + 7;
        assert_filled(c.ft_on_transfer(acct("b.testnet"), U128(gross), buy_msg("tok.testnet", 0, false)));
        let info = c.get_launch(acct("tok.testnet")).unwrap();
        let near_in = gross - gross / 100;
        let out = supply - info.token_reserve.0;
        set_self_callback();
        let refund = c.on_buy_deliver(
            acct("tok.testnet"), U128(gross), U128(near_in), U128(out), U128(gross / 100),
            false, Err(PromiseError::Failed),
        );
        assert_eq!(refund, U128(gross));
        let after = c.get_launch(acct("tok.testnet")).unwrap();
        assert_eq!(after.real_near, U128(0));
        assert_eq!(after.token_reserve.0, supply);
        assert_eq!(after.fees_collected, U128(0));
        assert_eq!(after.volume, U128(0));
        assert_eq!(c.get_protocol_fees(), U128(0));
        let a = c.get_accrual(acct("tok.testnet")).unwrap();
        assert_eq!(a.creator.0 + a.buyback.0 + a.holder.0, 0);
    }

    #[test]
    fn stress_mass_dump_zeroes_real_near_without_underflow() {
        // Sellers can never extract more wNEAR than the curve holds: near_out caps payout at
        // real_near, so dumping 10x the circulating supply floors real_near at 0 — never underflows.
        let supply = 1_000_000_000u128 * E24;
        let mut c = live_custom(
            "owner.testnet", "tok.testnet", "creator.testnet",
            5_000 * E24, supply, 3_000 * E24, 7000, 2000, 1000,
        );
        set_predecessor(WNEAR);
        assert_filled(c.ft_on_transfer(acct("whale.testnet"), U128(2_000 * E24), buy_msg("tok.testnet", 0, false)));
        let bought = supply - c.get_launch(acct("tok.testnet")).unwrap().token_reserve.0;
        set_predecessor("tok.testnet");
        assert_filled(c.ft_on_transfer(acct("whale.testnet"), U128(bought.saturating_mul(10)), sell_msg(0)));
        assert_eq!(c.get_launch(acct("tok.testnet")).unwrap().real_near, U128(0));
    }

    // ---------- ATTACKER PLAYBOOK: concrete exploits that MUST fail ----------

    #[test]
    fn hack_round_trip_cannot_extract_profit() {
        // Attacker buys then instantly sells everything back. After 1% each way + the real-wNEAR
        // cap, the realizable payout is strictly LESS than what they put in — no free money.
        let mut n = 0u32;
        for spend in [10_000u128, 100_000, 250_000, 400_000] {
            n += 1;
            let tok = format!("h{}.testnet", n);
            let mut c = live_custom(
                "owner.testnet", &tok, "attacker.testnet",
                1_000_000, 1_000_000, 500_000, 7000, 2000, 1000,
            );
            set_predecessor(WNEAR);
            assert_filled(c.ft_on_transfer(acct("attacker.testnet"), U128(spend), buy_msg(&tok, 0, false)));
            let got = 1_000_000 - c.get_launch(acct(&tok)).unwrap().token_reserve.0;
            let realizable = c.quote_sell(acct(&tok), U128(got)).0;
            assert!(realizable < spend, "round-trip must lose to fees+cap: got {} back for {}", realizable, spend);
        }
    }

    #[test]
    fn hack_sell_into_empty_curve_pays_zero() {
        // real_near = 0 (nobody ever bought). An attacker dumping tokens must get the tokens back
        // and ZERO wNEAR — the virtual/shadow reserve is never a withdrawable pot.
        let mut c = live_launch("owner.testnet", "tok.testnet", "creator.testnet");
        set_predecessor("tok.testnet");
        assert_refunded(
            c.ft_on_transfer(acct("attacker.testnet"), U128(500_000), sell_msg(0)),
            500_000,
        );
        assert_eq!(c.get_launch(acct("tok.testnet")).unwrap().real_near, U128(0));
    }

    #[test]
    #[should_panic(expected = "only the launch creator can claim")]
    fn hack_creator_cannot_claim_another_launchs_fees() {
        // Mallory legitimately owns launch tok2, then tries to drain the DIFFERENT launch tok's fees.
        let mut c = funded(); // tok.testnet has accrued creator fees, creator = creator.testnet
        set_predecessor("owner.testnet");
        c.register_launch(
            acct("tok2.testnet"), acct("mallory.testnet"), "M".into(), "M".into(),
            Tier::Economy, U128(1_000_000), U128(1_000_000), U128(500_000), 7000, 2000, 1000, None,
        );
        set_predecessor("mallory.testnet");
        let _ = c.claim_creator_fees(acct("tok.testnet")); // not her launch -> panic
    }

    #[test]
    #[should_panic(expected = "nothing to claim")]
    fn hack_reentrant_double_claim_before_callback() {
        // Simulate a reentrancy window: claim zeroes the bucket optimistically, then the attacker
        // "re-enters" and claims AGAIN before the payout callback resolves. Bucket is already 0 -> no
        // second payout is ever booked.
        let mut c = funded();
        set_predecessor("creator.testnet");
        let _ = c.claim_creator_fees(acct("tok.testnet")); // optimistic zero
        let _ = c.claim_creator_fees(acct("tok.testnet")); // re-entry -> nothing to claim
    }

    #[test]
    fn hack_adversarial_sequence_never_pays_out_more_than_paid_in() {
        // The global safety invariant: across an arbitrary attacker-driven mix of buys and sells,
        // real_near (the only pot sells draw from) NEVER exceeds the cumulative gross wNEAR paid in,
        // and never underflows (an underflow would panic and fail the test).
        let mut c = live_launch("owner.testnet", "tok.testnet", "creator.testnet");
        let mut deposited = 0u128;
        for (who, amt) in [("a", 50_000u128), ("b", 120_000), ("c", 30_000), ("d", 200_000)] {
            set_predecessor(WNEAR);
            assert_filled(c.ft_on_transfer(acct(&format!("{}.testnet", who)), U128(amt), buy_msg("tok.testnet", 0, false)));
            deposited += amt;
            assert!(c.get_launch(acct("tok.testnet")).unwrap().real_near.0 <= deposited, "real_near must never exceed wNEAR paid in");
        }
        // Sells (last one absurdly large) can only ever drain down to 0 — never past it.
        for amt in [40_000u128, 15_000, 10_000_000_000] {
            set_predecessor("tok.testnet");
            let _ = c.ft_on_transfer(acct("dumper.testnet"), U128(amt), sell_msg(0));
            assert!(c.get_launch(acct("tok.testnet")).unwrap().real_near.0 <= deposited, "sells cannot mint wNEAR");
        }
        assert_eq!(c.get_launch(acct("tok.testnet")).unwrap().real_near, U128(0), "the mega-dump floors the pot at 0, no underflow");
    }

    // ==================== END STRESS TESTS ====================

    // ==================== FACTORY / LAUNCH-FEE TESTS ====================

    /// Context for a `create_launch` call: creator predecessor + attached deposit, under a valid
    /// router account so the derived `t<seq>.router.testnet` id parses.
    fn ctx_create(creator: &str, deposit: NearToken) {
        let ctx = VMContextBuilder::new()
            .current_account_id(acct("router.testnet"))
            .predecessor_account_id(acct(creator))
            .attached_deposit(deposit)
            .build();
        testing_env!(ctx);
    }

    /// The Basic-tier launch fee the creator attaches on `create_launch` (deploy deposit funded from it).
    fn launch_deposit() -> NearToken {
        BASIC_LAUNCH_FEE
    }

    /// A test opening MCAP (virtual_near). Distinct from `LAUNCH_SUPPLY`/graduation so the recorded
    /// value is unambiguously the per-launch argument.
    fn test_virtual_near() -> U128 {
        U128(890 * 1_000_000_000_000_000_000_000_000) // ~$4k @ NEAR≈$4.5 (Basic tier reference)
    }

    #[test]
    fn launch_fee_and_treasury_are_exposed() {
        let c = new_contract("owner.testnet");
        assert_eq!(c.get_treasury(), acct("treasury"));
        // Tiered: Basic 0.18 / Express 0.35.
        assert_eq!(c.get_launch_fee(Tier::Economy), U128(NearToken::from_millinear(180).as_yoctonear()));
        assert_eq!(c.get_launch_fee(Tier::Express), U128(NearToken::from_millinear(350).as_yoctonear()));
    }

    #[test]
    fn create_launch_defers_record_until_deploy_callback() {
        let mut c = new_contract("owner.testnet");
        ctx_create("dev.testnet", launch_deposit());
        let _p = c.create_launch("Cat Coin".into(), "CAT".into(), Tier::Economy, test_virtual_near(), 7000, 2000, 1000, None);
        // The subaccount id is minted (and reserved) and the counter advances, but nothing is
        // recorded until the async token deploy resolves in `on_ft_deployed`.
        assert_eq!(c.launch_seq, 1);
        assert_eq!(c.get_num_launches(), 0);
        // Account id is derived from the lowercased ticker under the single brand root.
        assert!(c.taken_ids.contains(&acct("cat.router.testnet")), "base id reserved from ticker");
    }

    #[test]
    fn create_launch_collision_appends_dash_n_to_account_id_only() {
        let mut c = new_contract("owner.testnet");
        // Three launches all ticker "PEPE": symbols may duplicate freely; only the ACCOUNT ID gets
        // an FCFS `-N` suffix.
        ctx_create("a.testnet", launch_deposit());
        c.create_launch("Pepe One".into(), "PEPE".into(), Tier::Economy, test_virtual_near(), 7000, 2000, 1000, None);
        ctx_create("b.testnet", launch_deposit());
        c.create_launch("Pepe Two".into(), "PEPE".into(), Tier::Economy, test_virtual_near(), 7000, 2000, 1000, None);
        ctx_create("d.testnet", EXPRESS_LAUNCH_FEE);
        c.create_launch("Pepe Three".into(), "PEPE".into(), Tier::Express, test_virtual_near(), 7000, 2000, 1000, None);
        assert!(c.taken_ids.contains(&acct("pepe.router.testnet")));
        assert!(c.taken_ids.contains(&acct("pepe-2.router.testnet")));
        assert!(c.taken_ids.contains(&acct("pepe-3.router.testnet")));
    }

    #[test]
    #[should_panic(expected = "fee split must total 100%")]
    fn create_launch_rejects_bad_split() {
        let mut c = new_contract("owner.testnet");
        ctx_create("dev.testnet", launch_deposit());
        c.create_launch("Cat".into(), "CAT".into(), Tier::Economy, test_virtual_near(), 5000, 5000, 2000, None);
    }

    #[test]
    #[should_panic(expected = "attach the tier launch fee")]
    fn create_launch_rejects_insufficient_deposit() {
        let mut c = new_contract("owner.testnet");
        ctx_create("dev.testnet", NearToken::from_millinear(100)); // below the 0.18 Basic fee
        c.create_launch("Cat".into(), "CAT".into(), Tier::Economy, test_virtual_near(), 7000, 2000, 1000, None);
    }

    #[test]
    #[should_panic(expected = "attach the tier launch fee")]
    fn create_launch_express_rejects_basic_deposit() {
        let mut c = new_contract("owner.testnet");
        // 0.18 covers Basic but NOT the 0.35 Express fee.
        ctx_create("dev.testnet", BASIC_LAUNCH_FEE);
        c.create_launch("Cat".into(), "CAT".into(), Tier::Express, test_virtual_near(), 7000, 2000, 1000, None);
    }

    #[test]
    #[should_panic(expected = "name must be 1..=16 chars")]
    fn create_launch_rejects_long_name() {
        let mut c = new_contract("owner.testnet");
        ctx_create("dev.testnet", launch_deposit());
        c.create_launch("This name is far too long".into(), "CAT".into(), Tier::Economy, test_virtual_near(), 7000, 2000, 1000, None);
    }

    #[test]
    #[should_panic(expected = "ticker must be 3..=7 alphanumeric chars")]
    fn create_launch_rejects_short_ticker() {
        let mut c = new_contract("owner.testnet");
        ctx_create("dev.testnet", launch_deposit());
        c.create_launch("Cat".into(), "CA".into(), Tier::Economy, test_virtual_near(), 7000, 2000, 1000, None);
    }

    #[test]
    #[should_panic(expected = "ticker must be 3..=7 alphanumeric chars")]
    fn create_launch_rejects_bad_symbol() {
        let mut c = new_contract("owner.testnet");
        ctx_create("dev.testnet", launch_deposit());
        c.create_launch("Cat".into(), "C A T".into(), Tier::Economy, test_virtual_near(), 7000, 2000, 1000, None);
    }

    #[test]
    fn on_ft_deployed_success_records_and_seeds_the_curve() {
        let mut c = new_contract("owner.testnet");
        set_self_callback();
        let tok = acct("pepe.router.testnet");
        let out = c.on_ft_deployed(
            tok.clone(), acct("dev.testnet"), "Cat Coin".into(), "CAT".into(), Tier::Express,
            test_virtual_near(), 5000, 2500, 2500, None, U128(EXPRESS_LAUNCH_FEE.as_yoctonear()), Ok(()),
        );
        assert_eq!(out, Some(tok.clone()));
        let info = c.get_launch(tok).expect("launch recorded on deploy success");
        assert_eq!(info.creator, acct("dev.testnet"));
        assert_eq!(info.phase, Phase::Seeding);
        assert_eq!(info.tier, Tier::Express);
        // Opening MCAP is now set per-launch by the caller's `virtual_near` argument.
        assert_eq!(info.virtual_near, test_virtual_near());
        assert_eq!(info.on_curve_supply, U128(LAUNCH_SUPPLY));
        assert_eq!(info.token_reserve, U128(LAUNCH_SUPPLY), "curve seeded from owned supply");
        assert_eq!(info.graduation_near, U128(LAUNCH_GRADUATION_NEAR));
        assert_eq!(info.split.creator_bps, 5000);
    }

    #[test]
    fn on_ft_deployed_failure_frees_id_and_records_nothing() {
        let mut c = new_contract("owner.testnet");
        set_self_callback();
        let tok = acct("cat.router.testnet");
        c.taken_ids.insert(tok.clone()); // simulate the reservation made during create_launch
        let out = c.on_ft_deployed(
            tok.clone(), acct("dev.testnet"), "Cat".into(), "CAT".into(),
            Tier::Economy, test_virtual_near(), 7000, 2000, 1000, None, U128(BASIC_LAUNCH_FEE.as_yoctonear()),
            Err(near_sdk::PromiseError::Failed),
        );
        assert_eq!(out, None);
        assert_eq!(c.get_num_launches(), 0, "a failed token deploy leaves no launch behind");
        assert!(!c.taken_ids.contains(&tok), "failed deploy frees the reserved id for retry");
    }

    // ==================== END FACTORY TESTS ====================

    // ==================== STEP 2 FEATURE TESTS ====================

    /// Set predecessor + block timestamp for the next call (Express queue timing).
    fn set_ctx_ts(who: &str, ts: u64) {
        let ctx = VMContextBuilder::new()
            .predecessor_account_id(acct(who))
            .block_timestamp(ts)
            .build();
        testing_env!(ctx);
    }

    // ---- holder rewards (forwarded to the reward-bearing FT) ----

    #[test]
    fn distribute_forwards_holder_bucket_and_zeroes_it() {
        let mut c = live_launch("owner.testnet", "tok.testnet", "creator.testnet");
        // One 1e6 buy: fee 10_000 → protocol 1_000, pool 9_000; holder = 9_000*1000/10000 = 900.
        set_predecessor("wrap.testnet");
        assert_filled(c.ft_on_transfer(acct("buyer.testnet"), U128(1_000_000), buy_msg("tok.testnet", 0, false)));
        assert_eq!(c.get_accrual(acct("tok.testnet")).unwrap().holder, U128(900));
        // Permissionless keeper forwards the whole bucket to the FT and zeroes it optimistically.
        set_predecessor("keeper.testnet");
        let _ = c.distribute_holder_rewards(acct("tok.testnet"));
        assert_eq!(c.get_accrual(acct("tok.testnet")).unwrap().holder, U128(0));
    }

    #[test]
    #[should_panic(expected = "nothing to distribute")]
    fn distribute_with_empty_holder_bucket_rejected() {
        let mut c = live_launch("owner.testnet", "tok.testnet", "creator.testnet");
        c.distribute_holder_rewards(acct("tok.testnet"));
    }

    #[test]
    fn distribute_rebooks_full_bucket_on_forward_failure() {
        let mut c = live_launch("owner.testnet", "tok.testnet", "creator.testnet");
        set_predecessor("wrap.testnet");
        assert_filled(c.ft_on_transfer(acct("buyer.testnet"), U128(1_000_000), buy_msg("tok.testnet", 0, false)));
        let _ = c.distribute_holder_rewards(acct("tok.testnet"));
        assert_eq!(c.get_accrual(acct("tok.testnet")).unwrap().holder, U128(0));
        // The FT forward fails entirely → the full bucket is re-booked for a retry (never lost).
        c.on_holder_forwarded(acct("tok.testnet"), U128(900), Err(PromiseError::Failed));
        assert_eq!(c.get_accrual(acct("tok.testnet")).unwrap().holder, U128(900));
    }

    #[test]
    fn distribute_full_accept_keeps_bucket_zero() {
        let mut c = live_launch("owner.testnet", "tok.testnet", "creator.testnet");
        set_predecessor("wrap.testnet");
        assert_filled(c.ft_on_transfer(acct("buyer.testnet"), U128(1_000_000), buy_msg("tok.testnet", 0, false)));
        let _ = c.distribute_holder_rewards(acct("tok.testnet"));
        // FT accepted all 900 (ft_on_transfer returned used == amount) → nothing re-booked.
        c.on_holder_forwarded(acct("tok.testnet"), U128(900), Ok(U128(900)));
        assert_eq!(c.get_accrual(acct("tok.testnet")).unwrap().holder, U128(0));
    }

    #[test]
    fn distribute_rebooks_only_the_refunded_remainder() {
        let mut c = live_launch("owner.testnet", "tok.testnet", "creator.testnet");
        set_predecessor("wrap.testnet");
        assert_filled(c.ft_on_transfer(acct("buyer.testnet"), U128(1_000_000), buy_msg("tok.testnet", 0, false)));
        let _ = c.distribute_holder_rewards(acct("tok.testnet"));
        // FT accepted 600 of 900 → the 300 refunded to the router is re-booked for the next round.
        c.on_holder_forwarded(acct("tok.testnet"), U128(900), Ok(U128(600)));
        assert_eq!(c.get_accrual(acct("tok.testnet")).unwrap().holder, U128(300));
    }

    // ---- buyback ----

    #[test]
    fn execute_buyback_locks_tokens_and_spends_bucket() {
        let mut c = live_launch("owner.testnet", "tok.testnet", "creator.testnet");
        set_predecessor("wrap.testnet");
        assert_filled(c.ft_on_transfer(acct("buyer.testnet"), U128(1_000_000), buy_msg("tok.testnet", 0, false)));
        // buyback bucket = 9000 * 2000/10000 = 1800.
        assert_eq!(c.get_accrual(acct("tok.testnet")).unwrap().buyback, U128(1800));
        let before = c.get_launch(acct("tok.testnet")).unwrap();
        let out = c.execute_buyback(acct("tok.testnet"));
        assert!(out.0 > 0, "buyback bought some tokens");
        let after = c.get_launch(acct("tok.testnet")).unwrap();
        assert_eq!(c.get_accrual(acct("tok.testnet")).unwrap().buyback, U128(0), "bucket spent");
        assert_eq!(after.burned.0, before.burned.0 + out.0, "bought tokens are locked/burned");
        assert_eq!(after.real_near.0, before.real_near.0 + 1800, "spent wNEAR added to the curve");
        assert_eq!(after.token_reserve.0, before.token_reserve.0 - out.0, "reserve reduced by locked tokens");
        // value conservation: router token balance = reserve + burned is preserved
        // (nothing minted; the bought tokens simply moved from reserve → burned).
        assert_eq!(
            after.token_reserve.0 + after.burned.0,
            before.token_reserve.0 + before.burned.0,
        );
    }

    #[test]
    #[should_panic(expected = "nothing to buy back")]
    fn buyback_with_empty_bucket_rejected() {
        let mut c = live_launch("owner.testnet", "tok.testnet", "creator.testnet");
        c.execute_buyback(acct("tok.testnet"));
    }

    #[test]
    #[should_panic(expected = "launch is not live")]
    fn buyback_requires_live_phase() {
        let mut c = with_launch("owner.testnet", "tok.testnet", "creator.testnet");
        seed(&mut c, "tok.testnet", "creator.testnet"); // still Seeding
        c.execute_buyback(acct("tok.testnet"));
    }

    // ---- King of the Hill ----

    #[test]
    fn king_ranks_live_launches_by_bonding_progress() {
        let mut c = live_launch("owner.testnet", "a.testnet", "creator.testnet");
        // second launch, live
        set_predecessor("owner.testnet");
        c.register_launch(
            acct("b.testnet"), acct("creator.testnet"), "B".into(), "B".into(),
            Tier::Economy, U128(1_000_000), U128(1_000_000), U128(500_000), 7000, 2000, 1000, None,
        );
        seed(&mut c, "b.testnet", "creator.testnet");
        set_predecessor("owner.testnet");
        c.go_live(acct("b.testnet"));
        // Buy more on b than a → b has higher progress → b is King.
        set_predecessor("wrap.testnet");
        assert_filled(c.ft_on_transfer(acct("x.testnet"), U128(100_000), buy_msg("a.testnet", 0, false)));
        set_predecessor("wrap.testnet");
        assert_filled(c.ft_on_transfer(acct("y.testnet"), U128(400_000), buy_msg("b.testnet", 0, false)));
        let board = c.get_king_of_the_hill(Some(10));
        assert_eq!(board.len(), 2);
        assert_eq!(board[0].token_id, acct("b.testnet"), "higher progress ranks first");
        assert!(board[0].progress_bps > board[1].progress_bps);
        assert_eq!(c.get_king().unwrap().token_id, acct("b.testnet"));
    }

    #[test]
    fn king_excludes_non_live_launches() {
        let c = with_launch("owner.testnet", "tok.testnet", "creator.testnet"); // Seeding only
        assert!(c.get_king_of_the_hill(Some(10)).is_empty());
        assert!(c.get_king().is_none());
    }

    // ---- Express Premium Board ----

    #[test]
    fn express_enqueue_activates_within_slots() {
        let mut c = new_contract("owner.testnet");
        for i in 0..2 {
            set_ctx_ts("owner.testnet", 1_000 + i);
            c.enqueue_express(&acct(&format!("t{i}.router.testnet")));
        }
        let board = c.get_express_board();
        assert_eq!(board.len(), 2, "both within the {EXPRESS_BOARD_SLOTS}-slot board");
        assert!(board.iter().all(|e| e.state == ExpressState::Active));
    }

    #[test]
    fn express_overflow_queues_then_promotes_fifo_on_expiry() {
        let mut c = new_contract("owner.testnet");
        // Enqueue 4 with EXPRESS_BOARD_SLOTS=3 → 3 Active, 1 Queued.
        for i in 0..4 {
            set_ctx_ts("owner.testnet", 1_000 + i);
            c.enqueue_express(&acct(&format!("t{i}.router.testnet")));
        }
        assert_eq!(c.get_express_board().len(), EXPRESS_BOARD_SLOTS as usize);
        let queued: Vec<_> = c
            .get_express_queue(None, None)
            .into_iter()
            .filter(|e| e.state == ExpressState::Queued)
            .collect();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].token_id, acct("t3.router.testnet"));
        // Advance past the 30-min window → actives expire, the queued one promotes (FIFO).
        set_ctx_ts("cron.testnet", 1_000 + EXPRESS_ACTIVE_DURATION_NS + 10);
        let active = c.promote_express_queue();
        assert_eq!(active, 1, "the single remaining queued entry is now the only active one");
        let board = c.get_express_board();
        assert_eq!(board.len(), 1);
        assert_eq!(board[0].token_id, acct("t3.router.testnet"), "promoted strictly by FIFO order");
    }

    #[test]
    fn express_cancel_queued_entry() {
        let mut c = new_contract("owner.testnet");
        // Register a launch so the creator lookup for cancel authorization works.
        set_predecessor("owner.testnet");
        c.register_launch(
            acct("t3.router.testnet"), acct("creator.testnet"), "Q".into(), "Q".into(),
            Tier::Express, U128(1_000_000), U128(1_000_000), U128(500_000), 7000, 2000, 1000, None,
        );
        for i in 0..4 {
            set_ctx_ts("owner.testnet", 1_000 + i);
            c.enqueue_express(&acct(&format!("t{i}.router.testnet")));
        }
        // t3 is Queued; its creator cancels it.
        set_predecessor("creator.testnet");
        c.cancel_express(acct("t3.router.testnet"));
        let entry = c
            .get_express_queue(None, None)
            .into_iter()
            .find(|e| e.token_id == acct("t3.router.testnet"))
            .unwrap();
        assert_eq!(entry.state, ExpressState::Cancelled);
    }

    #[test]
    #[should_panic(expected = "no queued express entry")]
    fn express_cancel_active_entry_rejected() {
        let mut c = new_contract("owner.testnet");
        set_ctx_ts("owner.testnet", 1_000);
        c.enqueue_express(&acct("t0.router.testnet")); // immediately Active
        set_predecessor("owner.testnet");
        c.cancel_express(acct("t0.router.testnet"));
    }

    #[test]
    fn express_survives_via_state_backed_queue() {
        // The queue lives in contract storage (Vector), so a re-read after re-fetching state sees
        // the same entries — restart-survival is a property of on-chain storage, asserted by the
        // board persisting across separate view calls without any in-memory cache.
        let mut c = new_contract("owner.testnet");
        set_ctx_ts("owner.testnet", 1_000);
        c.enqueue_express(&acct("t0.router.testnet"));
        assert_eq!(c.get_express_queue(None, None).len(), 1);
        assert_eq!(c.get_express_board()[0].token_id, acct("t0.router.testnet"));
    }

    // ---- emergency pause ----

    #[test]
    fn set_paused_owner_only_and_toggles() {
        let mut c = new_contract("owner.testnet");
        assert!(!c.is_paused());
        set_predecessor("owner.testnet");
        c.set_paused(true);
        assert!(c.is_paused());
        c.set_paused(false);
        assert!(!c.is_paused());
    }

    #[test]
    #[should_panic(expected = "only the owner can pause")]
    fn set_paused_rejects_non_owner() {
        let mut c = new_contract("owner.testnet");
        set_predecessor("attacker.testnet");
        c.set_paused(true);
    }

    #[test]
    #[should_panic(expected = "trading is paused")]
    fn paused_blocks_buys() {
        let mut c = live_launch("owner.testnet", "tok.testnet", "creator.testnet");
        set_predecessor("owner.testnet");
        c.set_paused(true);
        set_predecessor("wrap.testnet");
        let _ = c.ft_on_transfer(acct("buyer.testnet"), U128(1_000_000), buy_msg("tok.testnet", 0, false));
    }

    #[test]
    #[should_panic(expected = "trading is paused")]
    fn paused_blocks_sells() {
        let mut c = live_launch("owner.testnet", "tok.testnet", "creator.testnet");
        set_predecessor("owner.testnet");
        c.set_paused(true);
        set_predecessor("tok.testnet");
        let _ = c.ft_on_transfer(acct("seller.testnet"), U128(1_000), sell_msg(0));
    }

    #[test]
    fn paused_still_allows_claims_and_distribute() {
        let mut c = live_launch("owner.testnet", "tok.testnet", "creator.testnet");
        set_predecessor("wrap.testnet");
        assert_filled(c.ft_on_transfer(acct("buyer.testnet"), U128(1_000_000), buy_msg("tok.testnet", 0, false)));
        // Now pause — fund-returning + reward paths must remain open.
        set_predecessor("owner.testnet");
        c.set_paused(true);
        // creator claim
        set_predecessor("creator.testnet");
        let _ = c.claim_creator_fees(acct("tok.testnet"));
        // permissionless holder distribute (forwards the bucket to the reward-bearing FT) still works
        set_predecessor("keeper.testnet");
        let _ = c.distribute_holder_rewards(acct("tok.testnet"));
        assert_eq!(c.get_accrual(acct("tok.testnet")).unwrap().holder, U128(0));
    }

    // ==================== END STEP 2 FEATURE TESTS ====================
}


