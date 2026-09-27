# BUILD LOG

Running log of executed build briefs. Newest first.

---

## 2026-09-27 — Graduation threshold 5000 → 2400 wNEAR

Brief: `BRIEF-graduation-threshold-2400.md`. Owner-chosen FIXED wNEAR constant (declined oracle / USD-peg). Contract logic otherwise unchanged.

- `contract/src/lib.rs:89` — `LAUNCH_GRADUATION_NEAR` 5_000 → **2_400** ×1e24; comment `// 2400 wNEAR (~$12k @ NEAR $5)`. Also fixed the `set_graduation_near` doc comment (L820) "5000 wNEAR" → "2400 wNEAR". Public create path (`record_launch`, L776) already passes the constant, so new public launches auto-inherit 2400. Other `5_000`/`5000` in lib.rs are independent test literals (virtual_near, buy amounts) — left as-is per brief.
- Docs (same commit): `docs/ROADMAP.md:12`, `docs/ARCHITECTURE.md:92`, `docs/ECONOMICS.md:31`, `README.md:37` → 2,400 (README/ECONOMICS note ≈ $12k). No `$25k` mention existed.

### Acceptance (live testnet, stable router `podclubfun.testnet`)
1. `cargo test` — green (106 + 10 passed, 0 failed).
2. Rebuilt wasm (`cargo build --target wasm32-unknown-unknown --release` + `wasm-opt -Oz`), redeployed to stable router. Deploy tx `5C3SaVkDxHAZGKA5HQRPgPiPntZKAzvbLyzB9eeUf3p8`.
3. **Constant default = 2400** — public create path: `grad.podclubfun.testnet` (creator `pdc-b1-bob-e5c249.testnet`, create tx `5pjbNKuiRJGqG9rTYZ4HmRV87JXGtPygcwKaJfDdu9mD`); `get_launch.graduation_near` = `2400000000000000000000000000` = 2400×1e24. ✓
4. **graduate gate** (QA-shortcut threshold 3 wNEAR on `pod2.podclubfun.testnet` via `set_graduation_near`, tx `JE5CSpairkpzfG8sAzeYFjs2CzbqurGggWVtyEEmztEv`, per brief):
   - BELOW (real_near 0 < 3): `graduate` **rejected** ("graduation threshold not reached"); pod2 stayed `Live`. (near-cli-rs surfaced the require! failure as a wasm-trap and did not print the failed-tx hash; reject verified by unchanged on-chain state.)
   - AT/ABOVE: bob bought 4 wNEAR (tx `8hFXeA89UXaJZbJH5XCFBSTd2HL6ZMKmuCK33GJzCa6Z`) → real_near 3.96 ≥ 3; `graduate` **succeeded** (tx `Dz7PYsYRfJ6wiRUJveNX3azEcb2KEQVsgfG1J5UE1KEo`), pod2 → `Graduated`. ✓

### Notes for owner
- The QA proof graduated `pod2.podclubfun.testnet` (one of the two seed launches) — it is now `Graduated`, not Live. `pod1.podclubfun.testnet` remains Live; `grad.podclubfun.testnet` is a fresh Seeding launch. Re-seed a Live launch if Explore/Trade needs two.
- Committed on `master`, NOT pushed. No mainnet. No keys committed. `res/fastlaunch_router.wasm` refreshed to the deployed build.

---

## 2026-09-27 — Web write-path wiring (buy / sell / creator-claim / holder-claim) on stable testnet

Brief: `/tmp/BRIEF-web-writepath-wiring.md`. Wiring only — no contract logic changed.

### STEP 0 — stable testnet router (done in prior session, re-verified this session)
- Router / owner / treasury: **`podclubfun.testnet`** (funded, non-throwaway; keys in `/root/.near-credentials`, NOT in repo).
- Deployed router wasm = committed B1 build (sha256 `8fe5dab6…`), global-by-hash FT `569a25e3…`.
- wNEAR (`wrap.testnet`) registered on the router; `graduation_near` = 5 NEAR (low, exercisable).
- Seed launches (both `Live`): `pod1.podclubfun.testnet` (8000/1000/1000), `pod2.podclubfun.testnet` (7000/2000/1000).
- `web/config.testnet.js` → `podclubfun.testnet` / `https://test.rpc.fastnear.com`.

### STEP 1 — wallet.js
- Added `WNEAR_ID`, `viewOn(accountId,…)` (view() delegates), `signAndCallOn(receiverId,…)`, `signAndBatch(receiverId,steps)` (atomic multi-action tx). Exported on `window.FLWallet`.

### STEP 2 — trade.html BUY
- `doBuy`: (1) if buyer not registered on the **launch token FT**, `storage_deposit` there first (own tx — required or the router's delivery `ft_transfer` panics and the whole buy rolls back); (2) atomic wNEAR batch: `storage_deposit(self)` + `near_deposit(amt)` + `ft_transfer_call(receiver=router, msg=WnearMsg{token_id,min_out,dev_buy:false})` 1yocto / 120 Tgas.

### STEP 3 — trade.html SELL
- `doSell`: register seller on wNEAR if needed, then `ft_transfer_call` on the launch token (receiver=router, msg=TokenMsg{action:"sell",min_out}) 1yocto / 120 Tgas.

### STEP 4 — dashboard.js claims
- Creator: per-launch `get_accrual(token_id).creator` → `claim_creator_fees(token_id)` (0 deposit, creator-only).
- Holder: `get_rewards(account)` + `ft_balance_of` per launch → FT `claim_rewards()` (**0 deposit — NOT payable**).

### LIVE PROOF (stable router `podclubfun.testnet`, token `pod1.podclubfun.testnet`, buyer `pdc-b1-alice-7ad38e.testnet`)
Each action run on-chain as the exact tx the wired UI issues:

| Action | tx hash | result |
|---|---|---|
| register buyer on POD1 | `7UV4ui3yaftkp27SUBEKCgw7pnVY6Zc2TDz5gXpnQ2f1` | registered |
| wrap 2 NEAR → wNEAR | `5dKupXiEJMyMcdTowM5TBMhjXDbopUoodmiTrZmmmFVC` | 2 wNEAR |
| **BUY** (ft_transfer_call → router) | `5ahR6fJozpBvA4EuhcUSysMtaeWBHDxJT4LACgf2kQSv` | alice +165275459098497495826377295492487 POD1; real_near 1.98 |
| **SELL** (ft_transfer_call → router) | `J1Y8fH32PGHEh89Xi52ji6FSRMzHHBnbjityYQpAqyNt` | −half POD1, +1.0684 wNEAR |
| **creator CLAIM** | `2PdUKPBC2kvUW12uF9WsaBvJhRjhDFHCZHah2YWmj2YZ` | 22170103730664240218380 yocto wNEAR |
| holder distribute (keeper, permissionless) | `8Gg8kV1KabcUqcs2GwUPHxT1ZxGJkJ6d1g8bUoXi2zV1` | 2771262966333030027298 pushed to FT |
| **holder CLAIM** | `GNify9XsQb5Pz6Bv6xicymHNQy1jzAHpoTetdW9439rn` | 2771262966333012520868 yocto wNEAR to alice |

### Findings for owner review (STOP + report)
1. **Buyer FT-registration gap (fixed in wiring).** The router's delivery uses a plain `ft_transfer`; an unregistered buyer makes delivery panic and `on_buy_deliver` rolls the whole buy back (confirmed live earlier: tx `87ZFsGSBHMP4yT6Jd3xyGU1JCgV7oaqh8jxHx2UVz5LN` fired a "buy" event but reverted). `doBuy` now `storage_deposit`s the buyer on the launch FT first. No contract change.
2. **Holder rewards need `distribute_holder_rewards` first.** `get_rewards` on the FT is 0 until the router's permissionless `distribute_holder_rewards(token_id)` flushes the accrued holder bucket into the FT. The dashboard (per brief) only reads/claims the FT — it does NOT trigger distribute. So the holder-rewards section shows nothing until a keeper (or anyone) calls distribute. **Decision needed:** keeper cron vs. a UI "sync" affordance vs. auto-distribute. Left as-is (faithful to brief); not wired to avoid oversteping scope.
3. **"Through the actual browser UI with a wallet extension" could not be automated** by a CLI agent (no browser + injected wallet). The transactions above are byte-for-byte what the wired `doBuy`/`doSell`/claim handlers issue (same receivers, methods, args, deposits, gas). Manual browser repro checklist: connect Meteor on the wired pages against `podclubfun.testnet`, buy on `trade.html?t=pod1.podclubfun.testnet`, sell, then claim on `dashboard.html`.

### Regression check
- `create.html` unchanged except the `config.testnet.js` include; `create_launch` proven working on the stable router (pod1/pod2 exist).

Commit: on `master`, NOT pushed (owner review pending, README fixes still outstanding). No mainnet. No keys committed.
