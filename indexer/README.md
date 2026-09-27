# podclub.fun read-path (Cloudflare Worker)

Read-only indexer + API. **Originates no financial value** — every number it serves is a copy of
on-chain state read over public NEAR RPC (`ARCHITECTURE.md §4`).

## What it does
- `scheduled()` (cron `* * * * *`): calls the router's **view** methods over public RPC
  (`get_num_launches`, `get_launches`, `get_king_of_the_hill`, `get_express_board`,
  `get_treasury`/`get_protocol_bps`/`get_protocol_fees`), enriches each launch with derived fields
  (`spot_price`, `progress_bps`, real `volume_24h` from stored history), and writes one denormalized
  snapshot to KV. New views absent on an older deployed router return `null`, never faked data.
- `fetch()`: serves read-only JSON from that snapshot:
  - `GET /api/launches` — all launches (enriched)
  - `GET /api/launch/:token_id` — one launch (404 if unknown)
  - `GET /api/king` — King-of-the-Hill leaderboard (503 if the view is unavailable on-chain)
  - `GET /api/express` — Express Premium Board (503 if unavailable)
  - `GET /api/stats` — `num_launches` + protocol treasury/bps/fees
  - `GET /api/health` — liveness
  Before the first snapshot exists, data routes return **503** so the frontend falls back to reading
  RPC directly rather than showing anything fabricated.

## Test (no wrangler / Cloudflare account needed)
```
node --test indexer/test/
```
Stubs global `fetch` with canned RPC responses + an in-memory KV; covers the derived math, 24h
volume windowing, graceful degradation, and every `/api/*` route. A live smoke run against
`flpad-28720.testnet` confirms it reads the real 2 testnet launches.

## Deploy — BLOCKED on a credential
Deploy needs a Cloudflare API token + a KV namespace id.
Bindings and deploy are configured once the token exists:
```
npx wrangler kv namespace create SNAPSHOT      # paste id + preview_id into wrangler.toml
npx wrangler deploy                            # publishes the Worker + cron trigger
```
Set `CONTRACT_ID` in `wrangler.toml` to the router account and (optionally) tighten `ALLOW_ORIGIN`
to the Pages origin. `RPC_URL` is a public key-free endpoint.

Until deployed, the frontend uses its direct-RPC fallback (`web/api.js`) and shows the same real
on-chain data, just without the cache/cron layer.
