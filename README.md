# podclub.fun

**Fair-launch token launchpad on NEAR.** Create a token in a single transaction: provide the
metadata, and podclub.fun deploys a canonical **NEP-141**, seeds a **permanent Rhea DCL**
(concentrated-liquidity) pool with the full supply, and routes trading fees back to the creator.
No presale, no team allocation, no hidden unlocks — the whole supply goes straight into the pool
and the token is tradeable the moment it launches.

## Why podclub.fun

The fee structure is the product. podclub.fun charges a flat **1%** trading fee and hands **up to
90%** of it straight back to the creator, keeping only a flat **10%** for the protocol. Launching
costs the creator near the chain floor — no presale, no team allocation, no hidden unlocks.

| podclub.fun | |
|---|---|
| Trading fee | **1%** |
| Creator fee share | **up to 90%** |
| Protocol fee | **10%** — ecosystem redistribution |
| Launch cost to creator | near the chain floor |

A flat **10%** of every trading fee is retained by the protocol (earmarked for ecosystem
redistribution to active users); the remaining **90%** is the creator's. Creators split their 90%
across three destinations with simple controls — **claimable earnings** (paid in wNEAR),
**auto-buyback & burn**, and **holder rewards** (streamed pro-rata to token holders) — any mix,
0–100%, chosen at launch and then **locked forever**. Buyers can trust the split can never be
changed after they buy.

## The fee split

Every creator decides, **at launch**, how their 90% share of trading fees is divided across three
destinations — and the choice is **locked forever** once the token goes live:

- **Creator earnings** — accrue as wNEAR, claimable anytime from the dashboard.
- **Auto-buyback & burn** — market-buys the token from its own Rhea pool and burns it, on-chain and
  lazily (no keeper). Continuous, programmable buy pressure funded from fees already earned.
- **Holder rewards** — streamed pro-rata to current holders, earned just by holding, no staking.

Any mix that sums to 100% is allowed (e.g. 100% cash, or 70% cash / 20% burn / 10% holders).
Because it can never change after launch, buyers can trust the split they see at launch is the
split forever. Tokens whose owner has been renounced route their whole 90% to holder rewards.

## Features

- **One-transaction launch** — token + liquidity pool + fee routing, all in a single signed call.
- **Creator dashboard** — claim accrued fees and track earnings, burns, and holder rewards (the
  fee split itself is fixed at launch and shown read-only).
- **Explore & Trade** — discover launches and swap directly, with fees skimmed to the creator.
- **Flexible sign-in** — connect an existing NEAR wallet (Meteor, HOT, MyNEAR, Nightly, …) or
  sign in with Google or X. Social sign-in resolves — deterministically and non-custodially — to
  the *same* NEAR account every time; keys are never held by podclub.fun.

## Architecture

- **Contracts** (Rust, near-sdk): a router/factory that launches tokens, creates and seeds the
  Rhea DCL pool, and handles fee accounting (`launch_token` with the fee split fixed at launch,
  `register_external_token`, `claim_creator_fees`) plus a canonical NEP-141 token. External tokens
  can also register to route swaps through the router, with the creator share claimable by the
  token's deployer.
- **Frontend**: a static multi-page app (Create / Explore / Trade / Dashboard) bundled with Vite
  and deployed as static assets behind a CDN. Read data comes from contract view methods and
  public price feeds — no application server, no user tracking.
- **Wallets**: `@near-wallet-selector` for external wallets; NEAR Auth (MPC) for social sign-in.

## Status

In active development. Contracts are being brought up on **NEAR testnet** first; mainnet follows
once the on-chain economics are verified end-to-end. The frontend and design system are built;
wallet and contract wiring is in progress.

See [`docs/plans/2026-09-23-near-launchpad-design.md`](docs/plans/2026-09-23-near-launchpad-design.md)
for the full design.

```
contracts/   # Rust (near-sdk) — router/factory + NEP-141 + fee logic
web/         # static multi-page frontend (Vite) — Create / Explore / Trade / Dashboard
docs/        # design & plans
```

## Development

podclub.fun is built and released fully in the open. Contributions, issues, and review are
welcome.

## License

Released under the [MIT License](LICENSE).
