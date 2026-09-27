# podclub.fun — Security Model

> The **code is the source of truth.** This doc states the security properties the
> contracts are designed to hold and how they are enforced/tested. Status markers as in
> `ARCHITECTURE.md` (✅ tested · 🔨 built (on-chain proof pending) · 🔒 needs testnet/audit).

## 1. Threat model & core invariants

podclub handles user funds through a bonding-curve router. The invariants below MUST hold
exactly; each is enforced on-chain and covered by unit tests.

### 1.1 Constrained admin (no privileged fund access) ✅
The owner/treasury can **never** touch user balances, move the price, drain funds, or
redirect protocol fees:
- **Protocol split is immutable.** `protocol_bps = 1000` (10%) is set in `new()` and has
  **no setter** anywhere. There is no code path to change where fees go.
- **Treasury is immutable.** Set once in `new()`, no setter. Launch fees can only ever go
  to `treasury_id`.
- **Fee split per launch is immutable.** `FeeSplit{creator,buyback,holder}` is fixed at
  registration/creation, must total exactly 10000 bps, and has no setter.
- **Owner-gated methods move no user funds arbitrarily:** `register_launch` (records a
  launch; cannot alter an existing one), `seed_pool` (moves a *graduated* launch's leftover
  tokens into a **locked** LP the router itself can't withdraw), `claim_protocol_fees`
  (only the already-accrued protocol bucket), and `set_paused` (blocks trading only — §1.5).
- The owner has **no** method to withdraw `real_near`, a launch's accrual buckets, staked
  tokens, or the on-curve reserve.

### 1.2 Value conservation (no mint / no leak) ✅
- Fee math: `protocol_cut + pool_cut == fee`, and `creator + buyback + holder == pool_cut`
  exactly, with the holder bucket absorbing every rounding remainder. Stress-tested across
  awkward splits and amounts (`stress_fee_split_never_creates_or_destroys_value`).
- Launch-token supply is minted once and is immutable (no mint/burn method on the FT).
- Buyback (🔨) **locks** bought tokens in-router (no move path) rather than minting a burn
  authority — supply integrity preserved.

### 1.3 The shadow reserve is never withdrawable ✅
Sells are priced against `virtual_near + real_near` but **payout is capped at `real_near`**
(`curve::near_out`). A seller — or the whole market dumping — can never extract more wNEAR
than buyers actually put in. Covered by `hack_adversarial_sequence_never_pays_out_more_than_paid_in`
and `stress_mass_dump_zeroes_real_near_without_underflow`.

### 1.4 No profitable round-trip / no free money ✅
Buy-then-immediately-sell always loses to the 1% × 2 fee + the real-wNEAR cap
(`hack_round_trip_cannot_extract_profit`). Selling into an empty curve pays exactly zero.

### 1.5 Emergency pause blocks trading only 🔨
`set_paused(true)` (owner-only) blocks `buy`/`sell`/`dev buy`. It does **not** block claims,
`unstake`, or any fund-returning path, and gives the owner **no** new access to funds. Pause
is a circuit breaker, not a key to the vault.

## 2. Async safety (reentrancy / rollback) ✅
Every cross-contract payout uses **optimistic state + zero-then-restore-on-failure**:
- Buys/sells advance the curve and book the fee *before* the delivery leg, then
  `on_buy_deliver` / `on_sell_payout` fully unwind (unbook fee, restore reserves, refund)
  if delivery fails. A failed trade is never charged and never strands funds.
- Claims (creator/protocol, and 🔨 holder) zero the bucket optimistically; the callback
  restores it only if the wNEAR transfer failed → no double-claim window
  (`stress_no_double_claim_after_successful_payout`, `hack_reentrant_double_claim_before_callback`).
- The 5 (soon more) `#[private]` callbacks are guarded by near-sdk's caller check emitted
  into the wasm extern wrapper (verified statically — a unit test calling the inner fn
  bypasses that guard by design).
- Rollback is **additive and per-trade**: an interleaved failed buy strips exactly its own
  footprint and leaves concurrent trades intact (`stress_interleaved_buy_rollback_...`).

## 3. Arithmetic safety ✅
- Release profile sets `overflow-checks = true` + `panic = "abort"`. Hot-path `+`/`-` on
  reserves/fees rely on these checks (a wrap aborts the tx rather than corrupting state).
- Curve products (~1e60 at launchpad scale) use U256 `mul_div` to avoid u128 wrap, then fit
  back in u128 because results are bounded by a reserve. Overflow-tested at magnitude.
- Holder-reward accumulator (🔨) scales by `ACC_PRECISION` and carries the division
  remainder forward so integer division never leaks or mints dust.

## 4. Access control summary
| Method | Guard |
|---|---|
| `register_launch`, `seed_pool`, `claim_protocol_fees`, `set_paused` | owner only |
| `create_launch` | public, payable (fee + deposit), factory-minted id (no squatting) |
| `go_live` | owner or creator |
| `claim_creator_fees` | that launch's creator only |
| `graduate`, `promote_express_queue`, `distribute_holder_rewards`, `execute_buyback` | permissionless (deterministic threshold/queue/curve math — no discretion) |
| `claim_holder_rewards`, `unstake` | the staking account only |
| `#[private]` callbacks | near-sdk caller check (self only) |

Permissionless methods are safe because they encode **no discretion**: `graduate` only fires
past a fixed threshold; `execute_buyback` spends a pre-accrued bucket at the current curve
price and locks the result; `distribute_holder_rewards` only moves an accrued bucket into
the per-share accumulator; queue promotion is pure FIFO by timestamp.

## 5. Frontend / operational
- **No secrets in frontend or repo.** `web/wallet.js` holds only public config (network,
  contract id, public RPC URL). Beta access codes live in a Cloudflare env secret consumed
  by `_worker.js` (not a financial control).
- **No mocks for financial data.** Any UI figure that cannot yet be read from chain is
  marked INCOMPLETE rather than faked.
- **Testnet only** for now. Mainnet deploy, DNS, and fund movement require credentials not yet provisioned.

## 6. Known gaps / must-do before mainnet 🔒
- `seed_pool` DCL saga is ABI-verified but **runtime-unproven** — needs a testnet dry-run
  (deposit sizes, single-sided `add_liquidity`, `min_amount = 0` has no slippage floor).
- Owner account holds a full-access key (upgrade authority) = an off-chain trust assumption;
  must be locked/renounced at mainnet cutover.
- Full external security review before handling real funds.
- Post-graduation `real_near` handling (currently stays with the router) to be finalized.
