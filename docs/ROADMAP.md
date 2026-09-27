# podclub.fun — Roadmap & TODO

Living status doc. Updated as-we-go. Product: **podclub.fun** (branded "podclub.fun" in code;
rebrand deferred) · Chain: **NEAR** · Hosting: **Cloudflare** · Model: pump.fun-style bonding
curve → graduation to a locked Rhea DCL pool.

Legend: ✅ done & tested · 🟡 partial · ⬜ not started · 🔒 needs testnet/audit before mainnet

## Accepted design decisions (2026-09-25, fee + naming updated 2026-09-26)
1. **Express differs by opening market cap + visibility + fee** (SUPERSEDES the earlier "Express is
   decoupled from price / visibility-only" decision — revised 2026-09-25). Both tiers share the SAME curve shape, supply, and
   2,400-wNEAR graduation threshold. The launch fee is **tiered**: **Basic 0.18 / Express 0.35 NEAR** — the earlier flat
   0.18 is superseded. Express differs in: a higher opening MCAP (its per-launch `virtual_near`
   targets ~$10k FDV vs Basic's ~$4k), Premium Board visibility, and the molten EXPRESS badge.
   `virtual_near` is a per-launch argument (opening MCAP == FDV, since 100% of supply is on the
   curve); the frontend converts the tier's USD target to wNEAR at the live NEAR price. King ranking
   uses `real_near/graduation_near`, so a higher opening MCAP does not by itself inflate ranking.
2. **README reconciled to the bonding-curve model** (the code is the source of truth, not the older
   "Rhea DCL fair-launch, no curve" copy). ⬜ README rewrite still pending.
3. **Single brand root + public factory**. ONE
   factory instance lives on the single root `podclubdotfun.near`; every token is a
   `<label>.podclubdotfun.near` subaccount (`label` = lowercased ticker) minted by the factory, so no
   one can squat an id to siphon fees. Collisions get an FCFS `-N` suffix on the account id only (the
   symbol may duplicate freely). Tier is a fee+perks FLAG, not a namespace. The old owner-only
   `register_launch` stays as a low-level/testing primitive.
4. **Repo layout kept flat** under `fast-launch/` (contract, contract-ft, web, indexer, api) — no
   `/apps` restructure.

## Phase 0 — Inspection ✅
Repo inspected, report delivered, baseline established (see below).

## Phase 1 — Foundation 🟡
- ✅ `contract-ft` (NEP-141 launch token): fixed supply, mint-once, no post-init mint authority.
  Was non-compiling against near-contract-standards 5.29 (deprecated `impl_fungible_token_*!` macros
  emit `storage_withdraw(Option<U128>)` vs the trait's `Option<NearToken>`); rewritten with
  hand-delegated trait impls. 3 unit tests green; builds to wasm (`res/fastlaunch_ft.wasm`).
- ✅ Factory in the router: `create_launch` (payable) → charges tier launch fee (real transfer to
  treasury) → deploys FT subaccount minting full supply to router → seeds curve from owned supply.
  `on_ft_deployed` callback records the launch only on deploy success; refunds fee+deposit on failure.
- ✅ Launch fees on-chain: Economy **0.167 NEAR**, Express **0.5 NEAR** → `treasury_id`.
- ✅ Contract tests: **router 77**, **ft 3** — all green (`cargo test`).
- ✅ **Testnet saga verified (2026-09-25).** Router deployed+init to `flpad-28720.testnet`
  (treasury `fastlaunch-81480.testnet`). `create_launch` from `flcreator-8766.testnet` deployed
  `t1.flpad-28720.testnet` end-to-end: FT minted 1e33 to router, curve seeded, fee **+0.167 NEAR**
  landed in treasury, excess refunded to creator. Tx `EqMAiQf4L5PCJqgMY7twn4Fd5vuiPATQzi9M6YtgwEtP`.
- ✅ **`FT_DEPLOY_DEPOSIT` tuned** 3.5 → **3.4 NEAR** — measured floor is 3.201 NEAR (320.1 KB code +
  router balance entry) with ~0.2 margin.
- ✅ **Global-contract migration (NEP-591) — the launch-cost fix.** The 320 KB FT wasm is now published
  ONCE as an immutable global-by-hash contract (testnet tx `4WZRHAV4…`, hash
  `9B5cmmBFpdXH3TTNBRzofcp9Swkb4fS5GW9KbUqc8WdD`, one-time ~32 NEAR). `create_launch` now points each
  subaccount at that code via `use_global_contract` instead of shipping the wasm per launch, so a token
  account stakes only its own state (**measured 318 bytes ⇒ 0.00318 NEAR**). `FT_DEPLOY_DEPOSIT` cut
  3.4 → **0.1 NEAR**; router wasm shrank 852.8 → 532.7 KB. Verified live: `create_launch` from
  `flcreator-8766.testnet` deployed `t2.flpad-28720.testnet` referencing the global hash, curve seeded,
  fee +0.167 to treasury (tx `2KJiXiSqUwYM3uz1UTHMkgrbCafaiK91BZ8ZKizXCN6Q`). **Real launch cost fell
  from ~3.58 NEAR to ~0.28 NEAR (Economy) / ~0.61 (Express).** Router redeployed tx `96iWFbekX2…`.
- ✅ Frontend cost + token-id copy corrected (`web/index.html`, `web/create.html`): total ~3.58 → ~0.28,
  token storage 3.4 → 0.1, JS `FT_DEPLOY_DEPOSIT` 3.4 → 0.1; token-account preview now honestly shows
  the protocol-assigned sequential id `t#.podclubdotfun.near` (was falsely `ticker.podclubdotfun.near`).
- ⬜ Frontend wired to `create_launch` (5-step wizard), token page reading real state.
- ⬜ near-workspaces integration test (optional; live testnet saga now covers the happy path).

## Phase 2 — Trading 🟡→🔨
- ✅ Bonding curve, buy/sell, slippage (`min_out`), 1% fee, 10% protocol cut (immutable), 90% split
  validated == 100%, creator + protocol claims, adversarial tests.
- 🔨 **Buyback execution** (`execute_buyback`) — spends the buyback bucket against the
  launch's own curve fee-free and locks the bought tokens (`burned` tally, no move path). Synchronous,
  permissionless. + tests.
- 🔨 **Holder rewards** — stake-based reward-per-share accumulator: stake via
  `ft_on_transfer("stake")`, `distribute_holder_rewards`, `claim_holder_rewards`, `unstake`.
  Anti-double-claim + remainder-carry (no dust leak). + tests.
- 🔨 **Emergency pause** (`set_paused`) — owner-only, blocks trading only.
- ⬜ Post-graduation `real_near` handling (currently stays with the router).
- 🔒 FEE TEST (100 NEAR → 0.10 / creator / buyback / holder) on testnet, both buy & sell.

## Phase 3 — Indexer / read-path 🔨/🔒
Chain events + view methods → Cloudflare Worker indexer → KV/D1 → read-only `/api/*` → frontend.
Built against the Cloudflare-compatible stack; deploy needs the Cloudflare token. Until deployed, the UI reads router view methods directly over RPC.

## Phase 4 — Express 🔨
On-chain FIFO Premium Board: `Queued→Active→Expired`(+`Cancelled`), `EXPRESS_BOARD_SLOTS` active,
30-min `EXPRESS_ACTIVE_MS`, deterministic promotion (`promote_express_queue`), restart-survival
(state-backed), 0.35 NEAR Express fee wired to enqueue on deploy. + tests.

## Phase 5 — King + Graduation 🔨
King = deterministic on-chain bonding-progress ranking (`get_king_of_the_hill`/`get_king`) by
`real_near / graduation_near` among Live launches. Graduation exists on-chain; frontend consumes
the King view.

## Phase 6 — Security + Optimization 🔒
Full test matrix, security review, storage/perf review, near-workspaces integration tests, DCL
seed_pool testnet dry-run. No mainnet until critical issues resolved.

## Baseline (2026-09-25)
`cargo test` — router 77 pass, ft 3 pass. Both crates build to wasm. Build toolchain:
cargo 1.98.1, node v22, near-cli.
