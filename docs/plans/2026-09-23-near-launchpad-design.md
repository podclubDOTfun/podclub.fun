# NEAR Launchpad — Design

Date: 2026-09-23 · Working name: TBD

## 1. Goal

A flaunch/pump.fun-style token launchpad on **NEAR mainnet**. One-transaction fair launch:
the user provides token metadata; the launchpad deploys a canonical **NEP-141**, seeds a
**permanent Rhea DCL** (concentrated-liquidity) pool with the full 1B supply, and routes
trading fees to the creator. Direct competitor to nearpad.family.

## 2. Positioning vs NearPad

| | NearPad | This project |
|---|---|---|
| Trading fee | 1% | **1%** |
| Creator fee share | 70% | **up to 90%** |
| Launch cost to creator | ~0.161 NEAR | match / near chain floor |

Same headline fee as NearPad, but **90% of it goes to the creator** (vs their 70%), and the
creator can route their share three ways: claimable wNEAR, auto-buyback+burn, or holder rewards.
This costs us **nothing** (pure contract params). The moat is open-source dev funded by NEAR
Protocol Rewards plus a fee model that also works for **tokens not launched here** (any token can
route swaps through our router and its deployer earns the creator share).

## 3. Economics (verified 2026-09-23)

- Per-launch chain cost ≈ **0.161 NEAR** = gas ~0.0185 (burned) + storage ~0.1425 (locked).
  The **creator pays this**; the platform fronts nothing.
- **Global contracts** (NEP-591, NEAR ≥ v2.6): publish code once = **10 NEAR/100KB, BURNED**;
  referencing it = **<0.001 NEAR**. This is why per-token storage is only ~0.07 NEAR.
- **Rhea/Ref referral** program exists (~4% of the swap fee) but the widget/SDK exposes **no
  integrator-fee param** → to capture our 1% cut we need **our own router contract**.
- **Funding:** NEAR Protocol Rewards — scoring is **80% GitHub / 20% on-chain**
  (Bronze $1k → Silver $3k → Gold $6k → Diamond $10k/mo). Building in public = earning.

## 4. Cost tiers to reach mainnet

- **Level 1** (frontend-only): <$1, but only referral crumbs, no creator-share. Not competitive.
- **Level 2** (one router contract): **~$5-7** locked/recoverable **IF** we reuse an existing
  public FT factory / global NEP-141 (to avoid the ~$50-75 global-contract publish burn). **TARGET.**
- Build + test on **testnet: $0**.

## 5. Architecture

- **Factory/Router contract** (Rust, near-sdk), deployed once. `launch_token(meta, pair, tax,
  fee_dest)` in **one tx**: create token (via global-contract ref / public factory) → mint 1B →
  create + seed Rhea DCL pool single-sided → register fee split. Handles fee routing (1%
  trading fee: 10% of the fee to protocol (mandatory), 90% to the creator pool). Keeps a registry of launches for Explore.
- **Fee-share split (creator-set, sums to 100% of the 90% pool):** three destinations —
  (a) **Creator fee** claimable as wNEAR; (b) **Auto-buyback+burn** — market-buys the token from
  its own Rhea pool and burns it (deflationary support, on-chain, no keeper, executed lazily on
  the next fee-settling call); (c) **Holder rewards** — the portion is streamed pro-rata to
  current token holders (hold-to-earn, no staking). Stored as basis points per token
  (`creator_bps + buyback_bps + holder_bps = 10000`). 0/100 in any leg is allowed (e.g. 100% cash,
  or 70% cash / 20% burn / 10% holders).
- **Split is set ONCE and then IMMUTABLE — DECIDED (2026-09-23).** For a launched token the split
  is chosen on the Create form and fixed inside the `launch_token` tx; there is **no** post-launch
  edit (no `set_fee_split`, the dashboard only *displays* it and lets the creator *claim*). This is
  a trust guarantee — holders/buyers can see the split can never be rugged after they buy. Same for
  external tokens: the split is fixed once at `register_external_token` and cannot change after.
  (Trade-off accepted: creators can't retune later; immutability > flexibility.)
- **Token:** canonical NEP-141 referencing a global contract (cheap per-token storage).
- **External ("old") tokens:** tokens **not** launched here can still register with the router and
  route swaps through our Trade page. They incur the same 1% (90/10); the 90% creator share is
  claimable by the **token's deployer / contract owner**. This extends the fee model to the whole
  NEAR long-tail, not just our own launches.
  - **Setup & scope (answers the "kecolongan" worry):** the 1% is **only** charged on swaps that
    go **through our router** — nothing we do touches trades on Ref or any other DEX. So there is
    nothing to be cheated out of: we never *promise* to tax the whole market, only the volume that
    routes through us. If an old token also trades elsewhere, those trades simply earn us (and the
    owner) nothing — no loss, just no gain. The owner registers **once** (owner wallet signs, see
    below), sets the split **once**, and it locks — identical rule to launched tokens.
  - **Old-token caveat on buyback/holder legs:** buyback+burn needs a pool to buy from and holder
    rewards needs the holder set; for an external token whose main pool may be on another DEX, v1
    keeps it simple — an old token's split may be limited to **creator-cash + holder-rewards**, with
    buyback enabled only when the token has a Rhea pool the router can trade against. (Verify.)
  - **Ownership proof — DECIDED: the token OWNER's wallet signs; the router verifies
    `signer == token.owner_id`.** On registration/claim the router reads the token's owner
    (`get_owner`/`owner_id` view) and only accepts the call from that account. (NEAR note: a token
    *account* can technically sign for itself, but requiring the owner's normal wallet is the clean
    UX — "sign in with the owner wallet," not the CA.) For our own launches the owner is set to the
    creator at launch, so it's always known. Requires the token to expose an owner field; the
    renounced case is handled below.
  - **Unclaimed / ownerless fees — DECIDED:**
    - Owner exists but hasn't registered yet → the creator's 90% **accrues in the router keyed by
      `token_id`, waiting for the owner** (never swept to treasury). First registration proves
      ownership and unlocks it.
    - **Owner removed / burned (renounced) → the creator's 90% auto-routes to HOLDER REWARDS**
      (still 90/10: protocol keeps its 10%). For a renounced token `holder_bps` is effectively 100%
      of the creator pool — fees flow back to the community via the holder-reward accumulator, and
      nobody can claim them as cash. Renounce detection: no reachable `owner_id` (null / burn
      address / self-owned account with no full-access key).
- **Rhea integration:** create_pool / add_liquidity (DCL) / claim fees. **Interface TBD — verify.**
- **Frontend:** static multi-page site (self-contained HTML + shared CSS), bundled with **Vite**
  (no server framework → deploys as static assets to S3+CloudFront). Pages: Create, Explore,
  Trade, Creator Dashboard.
- **Auth / wallets (DECIDED 2026-09-23):** two login paths.
  (A) **External NEAR wallets** via `@near-wallet-selector` — Meteor, **HOT Wallet**, MyNEAR
  Wallet, Nightly. Wallets own their keys; import/export/send/receive handled by the wallet.
  (B) **Social login (Google / X)** via **NEAR Auth / FastAuth** (`@fast-auth-near/browser-sdk`
  + `javascript-provider` + `near-api-js`): Auth0 identity → **deterministic** NEAR account
  (`fast-auth.near` + MPC signer `v1.signer`), *same login always controls the same account —
  never a fresh keypair* (this was the hard requirement). MPC signing = non-custodial, we store
  no keys; no raw-seed export for social accounts (restore = log in again — accepted). Relayer
  can sponsor onboarding gas. **Build/test on testnet first; mainnet needs approved Auth0
  credentials.** Send/receive NEAR works for both paths once connected.
- **Read layer:** token list from factory view methods; price/chart from DexScreener +
  GeckoTerminal (free, no server).

## 6. Data flows

- **Launch:** fill form → 1 signed tx → factory deploys token + mints + creates/seeds DCL pool +
  registers → live & tradeable.
- **Trade:** swap on our Trade page → routed via router → fee skim → **10% to protocol treasury (mandatory)**, 90% to the creator pool, split by
  the creator's `creator_bps / buyback_bps / holder_bps` into (a) claimable wNEAR, (b) buyback+burn,
  and (c) holder rewards. The protocol's 10% accrues to a treasury earmarked for redistribution to
  active users.
- **Claim:** creator dashboard → `router.claim_creator_fees()` → transfer accrued.

## 7. Open questions — VERIFY before writing contract code

1. Rhea DCL exact interface: `create_pool`, `add_liquidity`, position storage minimum, fee-claim.
2. Public FT factory / global NEP-141 on mainnet we can reuse (decides ~$5-7 vs +$50-75 burn).
3. Single-sided DCL seeding: confirm **0 NEAR capital** needed (full supply on one side).
4. `contract-pays-gas` (native): can our factory sponsor creator gas? (covers gas, not storage).
5. Build toolchain: `cargo-near` + `wasm32` build feasibility (rustc 1.98 from source, no rustup).
6. Auto-buyback execution: can the router swap wNEAR→token against the token's own Rhea DCL pool
   in-contract (or must it route via the Rhea router)? Slippage/MEV bound on the buyback swap;
   burn = transfer to a null account vs a `burn` method on the NEP-141. Confirm gas cost of the
   lazy buyback so a normal trade doesn't get too expensive.
7. **Holder rewards mechanism:** pro-rata distribution to holders is expensive to push on NEAR
   (per-account storage/gas). Prefer a **pull/accumulator** model (global reward-per-share index,
   holders claim; requires a holder snapshot or balance hook). Decide: index-based claim vs
   periodic wNEAR airdrop; who pays the settlement gas.
8. **Fee scope — DECIDED (2026-09-23): the 1% is charged by the ROUTER, on any swap that passes
   through it — regardless of where the token was launched.**
   - Token launched here → its liquidity is seeded in our pool, so its trades route through the
     router → 1% (mandatory, funds the ecosystem).
   - "Old" token (launched elsewhere) swapped **through our router** → 1% (it's using our protocol);
     the 90% creator share is claimable by that token's deployer/owner.
   - Token launched elsewhere **and** traded elsewhere → never touches our router → we take nothing.
   - Tokens stay **plain NEP-141** (no fee-on-transfer / tax token). We do **not** try to enforce
     the 1% on third-party DEXs. Edge case accepted: a platform token *could* be traded on another
     DEX and escape the fee there — revisit a tax-token variant only if that leakage matters.

## 8. Milestones (double as Protocol Rewards deliverables)

1. Repo + design doc (this).
2. Toolchain up + NEP-141 compiles + testnet deploy.
3. Router contract MVP + Rhea testnet integration.
4. Frontend: Create + Explore on testnet.
5. Trade + Creator Dashboard.
6. Light audit + mainnet deploy (~$5-7) + first real launch.

Commit consistently in public throughout (80% of the reward score).
