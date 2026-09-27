# podclub.fun — Token Economics (on-chain source of truth)

Every number here is enforced in `contract/src/lib.rs` (router) and `contract/src/curve.rs`. The
frontend/indexer must READ these, never redefine them. Values are the testnet defaults as of
2026-09-25. (Product is branded "podclub.fun" in code; rebrand deferred.)

**Invariants (must hold exactly):** tiered launch fee **Basic 0.18 / Express 0.35 NEAR**;
trading fee **1%** on buy AND sell; protocol fee = **10% of the collected trading fee**;
distribution pool = remaining **90%**, split creator + buyback + holder = **exactly 100%** of
that pool; fixed supply **1,000,000,000**, no hidden minting.
Worked example — 100 NEAR trade → 1 NEAR fee → protocol 0.10; split 50/25/25 → creator 0.45,
buyback 0.225, holders 0.225.

## Launch token (NEP-141, `contract-ft`)
| Param | Value | Notes |
|---|---|---|
| Total supply | 1,000,000,000 | `LAUNCH_SUPPLY = 1e9 × 10^24` |
| Decimals | 24 | `TOKEN_DECIMALS` |
| Mint authority | none | minted once to router at init; no `mint`/`burn` method exists |
| Token id | `<label>.<root>` | factory-minted subaccount of the single brand root (`podclubdotfun.near`); `label` = lowercased ticker; FCFS `-N` suffix on collision (account id only — the symbol may duplicate freely); no id squatting |

## Bonding curve (constant-product, virtual reserves)
Both tiers share the SAME curve shape and graduation threshold. They differ ONLY in the opening
market cap (`virtual_near`), which is now set **per launch** by the caller.

| Param | Value | Notes |
|---|---|---|
| Virtual NEAR reserve | per launch (`virtual_near` arg) | opening MCAP/FDV == `virtual_near` (100% supply on curve); Basic ≈ $4k, Express ≈ $10k, converted to wNEAR at the live NEAR price by the frontend. Contract is USD-agnostic. |
| Token reserve (initial) | full supply | seeded from router-owned supply in `on_ft_deployed` |
| Curve | `x·y = k` | `mul_div` / `tokens_out` / `near_out` with u256 intermediates |
| Graduation threshold | 5,000 wNEAR | `LAUNCH_GRADUATION_NEAR` — TIER-INDEPENDENT, permissionless `graduate()` |
| Dev-buy cap | 4% | `DEV_BUY_CAP_BPS = 400` |

## Launch fee (tiered, real on-chain transfer)
| Tier | Fee | Constant |
|---|---|---|
| Basic (Economy) | 0.18 NEAR | `BASIC_LAUNCH_FEE = from_millinear(180)` |
| Express | 0.35 NEAR | `EXPRESS_LAUNCH_FEE = from_millinear(350)` |

`create_launch` requires `attached ≥ tier_fee(tier)`; excess is refunded. The creator attaches ONLY the
tier fee — the router funds the token deploy (`FT_DEPLOY_DEPOSIT = 0.03 NEAR`, millinear(30)) out of
it. On deploy **success**, the protocol margin (`tier_fee − FT_DEPLOY_DEPOSIT`) is
transferred to `treasury_id` in `on_ft_deployed`; on **failure** the full fee is refunded to the
creator (the router's deploy deposit auto-returns on batch revert). The deploy deposit backs the
keyless token account's own state and is **non-reclaimable by design** (no key/withdraw path — this
is what keeps launches rug-proof). The FT wasm is a NEP-591 global contract, so a launch subaccount
stakes only ~0.003 NEAR of its own state; 0.03 is a conservative testnet pad and the unused portion
stays as the token account's balance. **Mainnet target is 0.006 NEAR**, gated on a real testnet
floor measurement first (do not lower it blind).

## Trading fee (1% total, enforced on-chain)
| Slice | Value | Mutability |
|---|---|---|
| Total fee | 1.00% | `FEE_BPS = 100` |
| Protocol cut | 10% of fee | `protocol_bps = 1000` — **IMMUTABLE, no setter** |
| Distribution pool | 90% of fee | split creator / buyback / holder |

`FeeSplit { creator_bps, buyback_bps, holder_bps }` must sum to exactly 10,000 (validated on
`create_launch` / `record_launch`; rejected otherwise). Worked example, 100 NEAR trade:

```
fee            = 1.00 NEAR
protocol       = 0.10 NEAR              → treasury (immutable)
pool           = 0.90 NEAR              → split by FeeSplit
  e.g. 50/25/25 → creator 0.45 / buyback 0.225 / holder 0.225
```

## Accrual buckets — status
- **creator**: ✅ accrues + `claim_creator_fees` (callback restore on payout failure, anti-double-claim).
- **protocol**: ✅ accrues + `claim_protocol_fees`.
- **buyback**: 🔨 accrues + `execute_buyback` (permissionless): spends the bucket against the
  launch's own curve fee-free, moves `real_near += amount` / `token_reserve -= out`, and **locks**
  the bought tokens (added to a `burned` tally; the router has no path to move them). Requires `Live`.
- **holder**: 🔨 accrues + a stake-based reward-per-share accumulator. Holders stake the launch
  token into the router (`ft_transfer_call` msg `{"action":"stake"}`); `distribute_holder_rewards`
  pushes the accrued `holder` bucket into `acc_reward_per_share` (÷ `total_staked`, remainder
  carried); stakers `claim_holder_rewards` / `unstake`. Anti-double-claim via optimistic-zero +
  callback restore. See `ARCHITECTURE.md` §5.1.

## Express Premium Board — status 🔨
Express launches are enqueued on deploy into an on-chain FIFO board: states
`Queued → Active → Expired` (+ `Cancelled`), up to `EXPRESS_BOARD_SLOTS` active for
`EXPRESS_ACTIVE_MS` (30 min), promoted deterministically by enqueue order via
`promote_express_queue()`. Express pays the higher **0.35 NEAR** fee (vs Basic's 0.18) and differs
in **three** ways: a higher opening market cap (its `virtual_near` targets ~$10k FDV vs Basic's
~$4k), this Premium Board visibility, **and** the molten EXPRESS badge. The curve shape, graduation
threshold, and supply are identical to Basic — only the opening MCAP, fee, and perks differ.

## Emergency pause — status 🔨
`set_paused(bool)` (owner-only) blocks buys/sells only; claims, unstake, and all
fund-returning paths remain open. No fund access is granted by pause.

## Graduation → DCL pool seed
`seed_pool` (owner-operated) migrates the graduated curve into a Rhea/Ref DCL v2 pool
(`dclv2.ref-dev.testnet` testnet / `dclv2.ref-labs.near` mainnet, ABI verified). Currently
token-only single-sided (`min_amount = 0`); the accumulated `real_near` stays with the router —
🔒 final handling to be decided before mainnet.
