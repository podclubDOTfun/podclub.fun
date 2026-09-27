# podclub.fun

**Fair-launch token launchpad on NEAR.** Create a token in a single transaction: provide the
metadata, and podclub.fun deploys a canonical **NEP-141** that starts trading immediately on a
**bonding curve**. As people buy, real liquidity accrues; once a launch reaches its graduation
threshold, the curve closes and the token graduates to a **permanent Rhea DCL**
(concentrated-liquidity) pool. No presale, no team allocation, no hidden unlocks — trading is open
from the first block and every trading fee routes back to the creator.

## Why podclub.fun

The fee structure is the product. podclub.fun charges a flat **1%** trading fee and hands **up to
90%** of it straight back to the creator, keeping only a flat **10%** for the protocol. Launching a
token costs a small flat fee — **0.18 NEAR** (Basic) or **0.35 NEAR** (Express) — plus chain gas;
no presale, no team allocation, no hidden unlocks.

| podclub.fun | |
|---|---|
| Trading fee | **1%** |
| Creator fee share | **up to 90%** |
| Protocol fee | **10%** — ecosystem redistribution |
| Launch fee | **0.18 NEAR** (Basic) / **0.35 NEAR** (Express) |

A flat **10%** of every trading fee is retained by the protocol (earmarked for ecosystem
redistribution to active users); the remaining **90%** is the creator's. Creators split their 90%
across three destinations with simple controls — **claimable earnings** (paid in wNEAR),
**auto-buyback & burn**, and **holder rewards** (streamed pro-rata to token holders) — any mix,
0–100%, chosen at launch and then **locked forever**. Buyers can trust the split can never be
changed after they buy.

## How a launch works

1. **Launch** — one signed call deploys the NEP-141 and opens trading on a bonding curve. The tier
   sets the opening valuation (Basic ≈ $4k, Express ≈ $10k FDV). No presale, no pre-mint to a team.
2. **Trade on the curve** — anyone can buy and sell against the curve immediately. Every trade pays
   the flat 1% fee, routed to the creator's chosen split.
3. **Graduate** — once a launch accumulates **5,000 wNEAR** of real liquidity, `graduate`
   (permissionless — anyone can call it) closes the curve.
4. **Rhea DCL pool** — the graduated launch is seeded into a **permanent Rhea DCL**
   concentrated-liquidity pool, where it trades from then on.

## The fee split

Every creator decides, **at launch**, how their 90% share of trading fees is divided across three
destinations — and the choice is **locked forever** once the token goes live:

- **Creator earnings** — accrue as wNEAR, claimable anytime from the dashboard.
- **Auto-buyback & burn** — market-buys the token and burns it, on-chain and lazily (no keeper).
  Continuous, programmable buy pressure funded from fees already earned.
- **Holder rewards** — streamed pro-rata to current holders, earned just by holding, no staking.

Any mix that sums to 100% is allowed (e.g. 100% cash, or 70% cash / 20% burn / 10% holders).
Because it can never change after launch, buyers can trust the split they see at launch is the
split forever. Tokens whose owner has been renounced route their whole 90% to holder rewards.

## Tiers

Two launch tiers, chosen at create time — a fee + perks flag, not a separate namespace:

- **Basic** (0.18 NEAR) — standard launch, ~$4k opening valuation.
- **Express** (0.35 NEAR) — larger opening valuation (~$10k), a priority slot on the **Premium
  Board** (a rotating FIFO of featured launches), and an Express badge.

## Features

- **One-transaction launch** — token + bonding-curve market + fee routing, all in a single signed
  call.
- **Creator dashboard** — claim accrued fees and track earnings, burns, and holder rewards (the
  fee split itself is fixed at launch and shown read-only).
- **Explore & Trade** — discover launches and swap directly, with fees skimmed to the creator.
- **Flexible sign-in** — connect an existing NEAR wallet (Meteor, HOT, MyNEAR, Nightly, …). Social
  sign-in (Google / X) is planned.

## Architecture

- **Contracts** (Rust, near-sdk): a router/factory that launches tokens (`create_launch`), runs the
  bonding-curve market (buy/sell with fee accounting), graduates a launch (`graduate`) and seeds
  its Rhea DCL pool (`seed_pool`), and handles creator fee accounting (`claim_creator_fees`) — plus
  a canonical NEP-141 token. External tokens can register (`register_launch`) to route swaps
  through the router, with the creator share claimable by the token's deployer.
- **Frontend**: a static multi-page app (Create / Explore / Trade / Dashboard) bundled with Vite
  and deployed as static assets behind a CDN. Read data comes from contract view methods and
  public price feeds — no application server, no user tracking.
- **Wallets**: `@near-wallet-selector` for external wallets.

## Status

In active development. Contracts are live on **NEAR testnet**; create, buy, sell, and fee claims
are wired to a testnet router. Mainnet follows once the on-chain economics are verified
end-to-end.

See [`docs/plans/2026-09-23-near-launchpad-design.md`](docs/plans/2026-09-23-near-launchpad-design.md)
for the full design.

```
contract/      # Rust (near-sdk) — router/factory + bonding curve + graduation + fee logic
contract-ft/   # canonical NEP-141 launch token
web/           # static multi-page frontend (Vite) — Create / Explore / Trade / Dashboard
indexer/       # off-chain read/index layer for Explore & dashboards
docs/          # design & plans
```

## Development

podclub.fun is built and released fully in the open. Contributions, issues, and review are
welcome.

## License

Released under the [MIT License](LICENSE).
