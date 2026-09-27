// Node test for the podclub read-path Worker. No Cloudflare runtime / wrangler needed:
// - global fetch is stubbed to return canned NEAR RPC `call_function` responses,
// - the KV binding is a tiny in-memory Map stub.
// Run: node --test indexer/test/indexer.test.mjs
import { test } from "node:test";
import assert from "node:assert/strict";
import { _test } from "../src/index.js";

const { spotPrice, progressBps, updateVolHistory, poll, handle } = _test;

const YOCTO = 10n ** 24n;
const y = (n) => (BigInt(n) * YOCTO).toString();

// A Seeding→Live-ish launch fixture matching the contract's LaunchInfo shape.
function launch(over = {}) {
  return {
    token_id: "pepe.podclubdotfun.near",
    creator: "alice.testnet",
    name: "Pepe",
    symbol: "PEPE",
    tier: "Economy",
    phase: "Live",
    split: { creator_bps: 7000, buyback_bps: 2000, holder_bps: 1000 },
    socials: { x: null, telegram: null, discord: null, website: null },
    virtual_near: y(890),
    real_near: y(1000),
    token_reserve: y(500_000_000),
    on_curve_supply: y(1_000_000_000),
    graduation_near: y(5000),
    dev_bought: "0",
    fees_collected: y(10),
    volume: y(2000),
    created_at: 1790000000000000000,
    ...over,
  };
}

// ---- RPC stub: encodes each method's canned return as the byte-array RPC gives back ----
function encodeResult(obj) {
  const str = JSON.stringify(obj);
  return Array.from(new TextEncoder().encode(str));
}
function makeRpc(responses) {
  // responses: { method_name: value | (args)=>value | "__ERR__" }
  return async (_url, opts) => {
    const body = JSON.parse(opts.body);
    const m = body.params.method_name;
    const args = JSON.parse(atob(body.params.args_base64));
    const r = responses[m];
    if (r === undefined) {
      return { json: async () => ({ result: { error: "MethodResolveError(MethodNotFound)" } }) };
    }
    const val = typeof r === "function" ? r(args) : r;
    return { json: async () => ({ result: { block_height: 1, result: encodeResult(val) } }) };
  };
}

// In-memory KV stub with the get/put surface the Worker uses.
function makeKv() {
  const m = new Map();
  return { get: async (k) => (m.has(k) ? m.get(k) : null), put: async (k, v) => void m.set(k, v), _m: m };
}

test("spotPrice = (virtual+real)/reserve as wNEAR per whole token", () => {
  // (890+1000)/500,000,000 = 0.00000378 wNEAR per token
  const p = spotPrice(launch());
  assert.equal(p, "0.00000378");
  assert.equal(spotPrice(launch({ token_reserve: "0" })), null);
});

test("progressBps = real/graduation in bps, capped at 10000", () => {
  assert.equal(progressBps(launch({ real_near: y(1000), graduation_near: y(5000) })), 2000);
  assert.equal(progressBps(launch({ real_near: y(9999), graduation_near: y(5000) })), 10000);
  assert.equal(progressBps(launch({ graduation_near: "0" })), 0);
});

test("updateVolHistory returns null until a ≥24h baseline exists, then the real delta", () => {
  const hist = {};
  const now = 1_000_000_000_000;
  // First sample: no baseline yet.
  assert.equal(updateVolHistory(hist, "t", y(2000), now), null);
  // 12h later: still no full-day baseline.
  assert.equal(updateVolHistory(hist, "t", y(2500), now + 12 * 3600e3), null);
  // 25h after the first sample: baseline is now ≥24h old → real delta 3000-2000 = 1000.
  const d = updateVolHistory(hist, "t", y(3000), now + 25 * 3600e3);
  assert.equal(d, y(1000));
});

test("updateVolHistory never returns a negative delta", () => {
  const hist = {};
  const now = 2_000_000_000_000;
  updateVolHistory(hist, "t", y(5000), now);
  const d = updateVolHistory(hist, "t", y(4000), now + 25 * 3600e3); // volume can't really drop, but guard anyway
  assert.equal(d, "0");
});

test("poll() writes an enriched snapshot from real view data", async () => {
  const env = {
    CONTRACT_ID: "flpad.testnet",
    RPC_URL: "https://rpc.example",
    SNAPSHOT: makeKv(),
  };
  const rpc = makeRpc({
    get_num_launches: 2,
    get_launches: ({ from_index }) => (from_index === 0 ? [launch(), launch({ token_id: "doge.podclubdotfun.near", symbol: "DOGE" })] : []),
    get_king_of_the_hill: [{ token_id: "pepe.podclubdotfun.near", progress_bps: 2000 }],
    get_express_board: [],
    get_treasury: "treasury.testnet",
    get_protocol_bps: 1000,
    get_protocol_fees: y(3),
  });
  globalThis.fetch = rpc;

  const snap = await poll(env);
  assert.equal(snap.num_launches, 2);
  assert.equal(snap.launches.length, 2);
  assert.equal(snap.launches[0].spot_price, "0.00000378");
  assert.equal(snap.launches[0].progress_bps, 2000);
  assert.equal(snap.launches[0].volume_24h, null); // no 24h baseline on first poll
  assert.deepEqual(snap.king, [{ token_id: "pepe.podclubdotfun.near", progress_bps: 2000 }]);
  assert.equal(snap.protocol.treasury, "treasury.testnet");
  // Snapshot persisted to KV.
  assert.ok(env.SNAPSHOT._m.get("snapshot:v1"));
});

test("poll() degrades gracefully when a new view is absent (MethodNotFound → null, not faked)", async () => {
  const env = { CONTRACT_ID: "old.testnet", RPC_URL: "x", SNAPSHOT: makeKv() };
  globalThis.fetch = makeRpc({
    get_num_launches: 1,
    get_launches: ({ from_index }) => (from_index === 0 ? [launch()] : []),
    // get_king_of_the_hill / get_express_board intentionally omitted → stub returns MethodNotFound
    get_treasury: "t.testnet",
    get_protocol_bps: 1000,
    get_protocol_fees: "0",
  });
  const snap = await poll(env);
  assert.equal(snap.king, null);
  assert.equal(snap.express, null);
  assert.equal(snap.launches.length, 1);
});

test("handle() serves /api/* from the snapshot with CORS, honest 503/404", async () => {
  const kv = makeKv();
  const env = { CONTRACT_ID: "c.testnet", RPC_URL: "x", ALLOW_ORIGIN: "*", SNAPSHOT: kv };

  // Before any snapshot: 503 so the frontend can fall back to direct RPC.
  let res = await handle(new Request("https://w/api/launches"), env);
  assert.equal(res.status, 503);

  globalThis.fetch = makeRpc({
    get_num_launches: 1,
    get_launches: ({ from_index }) => (from_index === 0 ? [launch()] : []),
    get_king_of_the_hill: null, // present but empty-ish
    get_express_board: [],
    get_treasury: "t.testnet",
    get_protocol_bps: 1000,
    get_protocol_fees: "0",
  });
  await poll(env);

  res = await handle(new Request("https://w/api/launches"), env);
  assert.equal(res.status, 200);
  assert.equal(res.headers.get("access-control-allow-origin"), "*");
  const body = await res.json();
  assert.equal(body.launches[0].token_id, "pepe.podclubdotfun.near");

  res = await handle(new Request("https://w/api/launch/pepe.podclubdotfun.near"), env);
  assert.equal(res.status, 200);
  res = await handle(new Request("https://w/api/launch/nope.testnet"), env);
  assert.equal(res.status, 404);

  res = await handle(new Request("https://w/api/health"), env);
  assert.equal(res.status, 200);

  res = await handle(new Request("https://w/api/nonsense"), env);
  assert.equal(res.status, 404);

  res = await handle(new Request("https://w/api/launches", { method: "OPTIONS" }), env);
  assert.equal(res.status, 204);
});
