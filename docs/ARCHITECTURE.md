# podclub.fun — Architecture

> Status markers: ✅ built & tested · 🔨 built, on-chain proof pending ·
> 🟡 partial · ⬜ not started · 🔒 blocked on a credential.
> The **code is the source of truth**; this doc describes how the pieces fit together.

podclub.fun is a pump.fun-style fair-launch launchpad for NEP-141 meme tokens on NEAR.
The product is currently branded "podclub.fun" in code — rebrand is deferred.

## 1. System shape

```
                    ┌──────────────────────────────────────────────┐
   Creator/Trader   │                  NEAR (testnet)               │
   (browser +       │                                               │
    wallet-selector)│   ┌───────────────────────────────────────┐  │
        │           │   │  fastlaunch-router  (the factory +      │  │
        │  sign     │   │  bonding-curve engine + fee/accrual +   │  │
        ├──────────►│   │  holder-staking + buyback + express     │  │
        │           │   │  queue + King view + pause)             │  │
        │           │   └───┬───────────────────────────────┬─────┘  │
        │           │       │ deploys (global-contract ref) │        │
        │           │       ▼                               ▼        │
        │           │   ┌─────────────┐   ...          ┌─────────────┐│
        │           │   │ t1.<router> │  (NEP-141      │ tN.<router> ││
        │           │   │ launch FT   │   launch FTs)  │ launch FT   ││
        │           │   └─────────────┘                └─────────────┘│
        │           │       ▲ curve trades via wrap.testnet (wNEAR)  │
        │           │       │ graduation → dclv2.ref-dev.testnet     │
        │           └───────┼───────────────────────────────────────┘
        │                   │ view calls + event logs
        │  read (REST/JSON) │
        ▼                   ▼
   ┌─────────────────────────────────────────┐
   │  Cloudflare (static site + read-path)     │  🔨/🔒
   │  ┌───────────┐   ┌──────────────────────┐ │
   │  │ Pages      │   │ Worker: indexer +    │ │
   │  │ (static    │◄──┤ /api/* read endpoints│ │
   │  │  web/)     │   │  backed by KV/D1     │ │
   │  └───────────┘   └──────────────────────┘ │
   │      ▲ public static site (no gate)        │
   └──────┼─────────────────────────────────────┘
          │ browser
        Trader
```

**No application server holds funds or state.** Blockchain state is authoritative for
everything financial. The Cloudflare read-path is a *cache/index* of on-chain data — it
never originates prices, balances, or fees.

## 2. Contracts

### 2.1 `contract-ft` — launch token (NEP-141) ✅
- `near-contract-standards` `FungibleToken`, hand-delegated trait impls.
- Fixed supply (1e9 × 10²⁴) minted **once** to the router in `new()`, guarded by
  `require!(!env::state_exists())`. **No `mint`/`burn`/owner method** → supply immutable,
  no inflation authority. Published once as an immutable **global contract** (NEP-591);
  each launch subaccount references it by code hash and stakes only for its own state
  (~0.003 NEAR), so a launch costs ~0.1 NEAR instead of ~3.4.

### 2.2 `fastlaunch-router` — factory + engine
The router **is** the factory and holds each launch's on-curve supply, trading it against
a constant-product virtual-reserve curve (`curve.rs`). One contract, several concerns:

| Concern | Method(s) | Status |
|---|---|---|
| Create launch (public, fee-paid) | `create_launch` → `on_ft_deployed` | ✅ |
| Seed curve + go live | `ft_on_transfer("seed")`, `go_live` | ✅ |
| Buy / sell on curve | `ft_on_transfer` (buy via wNEAR, sell via token) | ✅ |
| 1% fee, 10/90 split, accrual buckets | `book_fee` / `unbook_fee` | ✅ |
| Creator / protocol fee claims | `claim_creator_fees`, `claim_protocol_fees` | ✅ |
| Graduation (permissionless) | `graduate` | ✅ |
| DCL pool seed (owner, locked LP) | `seed_pool` → `on_pool_seeded` | ✅ (🔒 testnet dry-run) |
| **Holder rewards** (stake → reward-per-share → claim) | `ft_on_transfer("stake")`, `unstake`, `distribute_holder_rewards`, `claim_holder_rewards` | 🔨 |
| **Buyback** (buy-from-curve + lock) | `execute_buyback` | 🔨 |
| **King of the Hill** (bonding-progress ranking) | `get_king_of_the_hill`, `get_king` | 🔨 |
| **Express queue** (FIFO Premium Board) | `promote_express_queue`, `cancel_express`, `get_express_board` | 🔨 |
| **Emergency pause** (blocks trading only) | `set_paused`, `is_paused` | 🔨 |

See §5 for the holder-rewards, buyback, and express-queue designs.

## 3. Trading & money flow

1. **Buy:** buyer `ft_transfer_call`s wNEAR → router `ft_on_transfer` skims 1% fee, prices
   the remainder on the curve, books the fee, advances reserves, delivers tokens from the
   router's own supply. A failed delivery rolls everything back (`on_buy_deliver`).
2. **Sell:** seller `ft_transfer_call`s the launch token → router prices gross wNEAR out
   (capped at `real_near` — the virtual reserve is never withdrawable), skims 1%, pays the
   remainder. Failed payout returns the tokens (`on_sell_payout`).
3. **Fee split:** every fee → 10% protocol (immutable) + 90% pool, pool split by the
   launch's immutable `FeeSplit` into `creator` / `buyback` / `holder` accrual buckets.
4. **Graduation:** at 2400 real wNEAR anyone may `graduate` (freezes the curve, moves no
   funds). Owner then `seed_pool`s the leftover tokens into a **locked** single-sided DCL
   position (LP NFT held by the router, no remove path).

## 4. Read path (indexer → storage → API → frontend) 🔨/🔒

The chain emits NEP-297-style `fastlaunch` events (`create_launch`, `seed`, `go_live`,
`buy`, `sell`, `graduate`, `claim`, and the new `buyback`/`stake`/`unstake`/
`holder_distribute`/`express_*` events) plus cumulative per-launch counters readable via
view methods (`get_launch`, `get_launches`, `quote_buy`, `quote_sell`, `get_accrual`,
`get_king_of_the_hill`, `get_express_board`, `get_stake`).

- **Indexer (Cloudflare Worker, scheduled):** polls the router's view methods + recent
  events over public RPC, writes a denormalized snapshot to a datastore.
- **Storage:** Cloudflare KV (snapshot blobs) or D1 (SQLite; relational, for 24h windows).
- **API (Worker fetch handler):** read-only `/api/*` JSON endpoints (launches list, single
  launch, King board, express board) consumed by the static frontend.
- **No faked data:** every figure the UI shows is derived from on-chain reads. Until the
  read-path is deployed (needs the Cloudflare token), the UI reads the router's view
  methods **directly** over RPC via `web/wallet.js`'s `view()` helper.

## 5. Feature designs

### 5.1 Holder rewards — stake-based reward-per-share 🔨
The router cannot trustlessly enumerate external NEP-141 holders, so holder rewards use the
industry-standard **staking accumulator** (MasterChef-style):
- Holders **stake** their launch tokens into the router (`ft_transfer_call` msg
  `{"action":"stake"}`). Staked tokens are tracked separately from `token_reserve` (they do
  not affect the curve).
- Per launch: `total_staked`, `acc_reward_per_share` (scaled by `ACC_PRECISION`), and a
  carried `holder_remainder` so integer division never leaks dust.
- `distribute_holder_rewards(token_id)` (permissionless) moves the accrued `accrual.holder`
  wNEAR into `acc_reward_per_share += amount·PREC / total_staked` (requires stakers).
- Each staker tracks `staked` + `reward_debt`. Pending = `staked·acc/PREC − reward_debt`.
  `claim_holder_rewards` pays pending wNEAR (zero-then-restore callback, anti-double-claim);
  `unstake` settles pending then returns tokens (rollback on failure).

### 5.2 Buyback — buy-from-curve + lock 🔨
`execute_buyback(token_id)` (permissionless, deterministic — no admin discretion): spends
the entire `accrual.buyback` wNEAR against the launch's **own** curve fee-free (internal
protocol op), moving `real_near += amount`, `token_reserve −= out`, and adding `out` to a
`burned` tally. The bought tokens never leave the router and there is **no method to move
them** → permanently locked (equivalent to a burn; the audited FT has no burn method, so
we lock rather than mint a burn path). Fully synchronous — no external transfer, no
rollback needed. Requires `Live` phase (the curve must be open to buy from).

### 5.3 King of the Hill 🔨
Deterministic ranking by bonding progress `real_near / graduation_near` among `Live`
launches, exposed as a view (`get_king_of_the_hill(limit)` + `get_king`). Progress is
computed as `real_near · PREC / graduation_near`; ties broken by `created_at` then id.
Read-only, so no gas cost to traders; the frontend/indexer consumes it.

### 5.4 Express queue — FIFO Premium Board 🔨
On-chain FIFO with states `Queued → Active → Expired` (+ `Cancelled`). An Express launch
(0.5 NEAR fee) is enqueued on successful deploy. Up to `EXPRESS_BOARD_SLOTS` entries are
`Active` at once for `EXPRESS_ACTIVE_MS` (30 min); `promote_express_queue()` (permissionless,
also called internally) expires timed-out actives and promotes the oldest `Queued` entries
deterministically by enqueue order. State lives in contract storage → survives restart.
`cancel_express` lets the creator/owner drop a still-`Queued` entry.

### 5.5 Emergency pause 🔨
Global `paused` flag, `set_paused(bool)` owner-only. When paused, **buys and sells are
blocked** (dev buy too). Claims, `unstake`, and every fund-returning path stay open — pause
can never touch funds or lock users out of their own balances.

## 6. DevOps
- **Hosting: Cloudflare** (Pages static site + Worker read-path) + current registrar. The
  `infra/` AWS Terraform is **legacy/parked** — not applied. The site is served publicly as
  a static app (no access gate). DNS/domain must not change.
- **Build:** `scripts/ec2-build-bootstrap.sh` installs rustup + wasm32 + cargo-near +
  near-cli. `cargo test` for unit tests; `cargo near build` for reproducible wasm.
