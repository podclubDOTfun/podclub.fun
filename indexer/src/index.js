// podclub.fun read-path Worker — indexer (scheduled) + read-only /api/* (fetch).
//
// Source of truth is the NEAR router contract. This Worker is a CACHE/INDEX of on-chain
// state read over public RPC — it never originates any financial value. See ARCHITECTURE.md §4.

const SNAPSHOT_KEY = "snapshot:v1";
const HISTORY_PREFIX = "vol:"; // per-launch cumulative-volume history for 24h windows
const PAGE = 50;

// ---- NEAR RPC view helper (read-only call_function) ----
async function view(env, method, args) {
  const body = {
    jsonrpc: "2.0",
    id: "idx",
    method: "query",
    params: {
      request_type: "call_function",
      finality: "final",
      account_id: env.CONTRACT_ID,
      method_name: method,
      args_base64: btoa(JSON.stringify(args || {})),
    },
  };
  const res = await fetch(env.RPC_URL, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(body),
  });
  const j = await res.json();
  if (j.error) throw new Error(j.error.data || j.error.message || "rpc error");
  if (j.result && j.result.error) throw new Error(j.result.error);
  const bytes = new Uint8Array(j.result.result);
  const str = new TextDecoder().decode(bytes);
  return str ? JSON.parse(str) : null;
}

// ---- derived (non-authoritative) metrics computed purely from on-chain fields ----
// Spot price of one whole token in wNEAR on the constant-product curve:
//   price = (virtual_near + real_near) / token_reserve
// Both reserves are yocto-scaled (24 decimals) and the token has 24 decimals, so the ratio is
// already "wNEAR per whole token". Returned as a decimal string; null if the curve is empty.
function spotPrice(l) {
  const cur = BigInt(l.virtual_near) + BigInt(l.real_near);
  const res = BigInt(l.token_reserve);
  if (res === 0n) return null;
  // price = cur/res, kept to 18 significant fractional digits without floating error.
  const SCALE = 1_000_000_000_000_000_000n; // 1e18
  const scaled = (cur * SCALE) / res;
  const whole = scaled / SCALE;
  const frac = (scaled % SCALE).toString().padStart(18, "0").replace(/0+$/, "");
  return frac ? `${whole}.${frac}` : `${whole}`;
}

// Bonding progress in basis points (real_near / graduation_near), capped at 10000. Integer-only.
function progressBps(l) {
  const grad = BigInt(l.graduation_near);
  if (grad === 0n) return 0;
  const bps = (BigInt(l.real_near) * 10000n) / grad;
  return Number(bps > 10000n ? 10000n : bps);
}

// A single launch, enriched with derived fields and (if history exists) a real 24h volume delta.
function enrich(l, vol24h) {
  return {
    ...l,
    spot_price: spotPrice(l),
    progress_bps: progressBps(l),
    // Cumulative real wNEAR volume is on-chain (l.volume). 24h delta is computed from stored
    // history; null until we have a snapshot ~24h old — never faked.
    volume_24h: vol24h,
  };
}

// A view call that MAY be absent on an older deployed contract (e.g. get_king_of_the_hill before
// the STEP-2 redeploy). Returns null on MethodNotFound / any RPC error instead of failing the whole
// poll — the API then reports that section as unavailable rather than fabricating it.
async function tryView(env, method, args) {
  try {
    return await view(env, method, args);
  } catch (e) {
    return null;
  }
}

const DAY_MS = 24 * 60 * 60 * 1000;

// Update the rolling per-launch volume history and return the real 24h volume delta (in yocto,
// as a decimal string) — or null if we don't yet have a sample ≥24h old (never guessed).
function updateVolHistory(hist, tokenId, volumeStr, now) {
  const series = hist[tokenId] || [];
  series.push({ t: now, v: volumeStr });
  // Drop samples older than 24h, but keep the most recent one before the window so we always have
  // a baseline to diff against once the series spans a full day.
  let cutoffIdx = 0;
  for (let i = 0; i < series.length; i++) {
    if (series[i].t >= now - DAY_MS) break;
    cutoffIdx = i;
  }
  const trimmed = series.slice(cutoffIdx);
  hist[tokenId] = trimmed;
  const oldest = trimmed[0];
  // Only report a 24h figure once the baseline is genuinely ≥24h old.
  if (!oldest || oldest.t > now - DAY_MS) return null;
  const delta = BigInt(volumeStr) - BigInt(oldest.v);
  return (delta < 0n ? 0n : delta).toString();
}

// ---- scheduled(): poll on-chain view methods → write a denormalized KV snapshot ----
async function poll(env) {
  const now = Date.now();
  const num = Number(await view(env, "get_num_launches", {}));

  const launchesRaw = [];
  for (let from = 0; from < num; from += PAGE) {
    const page = await view(env, "get_launches", { from_index: from, limit: PAGE });
    if (!Array.isArray(page) || page.length === 0) break;
    launchesRaw.push(...page);
    if (page.length < PAGE) break;
  }

  const histStr = await env.SNAPSHOT.get(HISTORY_PREFIX + "index");
  const hist = histStr ? JSON.parse(histStr) : {};

  const launches = launchesRaw.map((l) => {
    const vol24h = updateVolHistory(hist, l.token_id, String(l.volume ?? "0"), now);
    return enrich(l, vol24h);
  });

  // New views that an older deployed router may not expose yet → null, not faked.
  const king = await tryView(env, "get_king_of_the_hill", { limit: 50 });
  const express = await tryView(env, "get_express_board", {});
  const treasury = await tryView(env, "get_treasury", {});
  const protocolBps = await tryView(env, "get_protocol_bps", {});
  const protocolFees = await tryView(env, "get_protocol_fees", {});

  const snapshot = {
    updated_at: now,
    contract_id: env.CONTRACT_ID,
    rpc_url: env.RPC_URL,
    num_launches: num,
    launches,
    king,
    express,
    protocol: {
      treasury: treasury ?? null,
      protocol_bps: protocolBps ?? null,
      protocol_fees: protocolFees ?? null,
    },
  };

  await env.SNAPSHOT.put(SNAPSHOT_KEY, JSON.stringify(snapshot));
  await env.SNAPSHOT.put(HISTORY_PREFIX + "index", JSON.stringify(hist));
  return snapshot;
}

// ---- fetch(): serve read-only /api/* JSON from the KV snapshot ----
function cors(env) {
  return {
    "access-control-allow-origin": env.ALLOW_ORIGIN || "*",
    "access-control-allow-methods": "GET, OPTIONS",
    "access-control-allow-headers": "content-type",
  };
}

function json(body, env, status = 200) {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json; charset=utf-8", ...cors(env) },
  });
}

async function loadSnapshot(env) {
  const s = await env.SNAPSHOT.get(SNAPSHOT_KEY);
  return s ? JSON.parse(s) : null;
}

async function handle(request, env) {
  const url = new URL(request.url);
  const path = url.pathname.replace(/\/+$/, "") || "/";

  if (request.method === "OPTIONS") return new Response(null, { status: 204, headers: cors(env) });
  if (request.method !== "GET") return json({ error: "method not allowed" }, env, 405);

  if (path === "/api/health") {
    return json({ ok: true, contract_id: env.CONTRACT_ID }, env);
  }

  const snap = await loadSnapshot(env);
  // No snapshot yet → 503, so the frontend falls back to direct RPC rather than showing nothing
  // (and never shows fabricated data).
  if (!snap) return json({ error: "snapshot not ready" }, env, 503);

  if (path === "/api/launches") {
    return json({ updated_at: snap.updated_at, launches: snap.launches }, env);
  }
  if (path.startsWith("/api/launch/")) {
    const id = decodeURIComponent(path.slice("/api/launch/".length));
    const l = snap.launches.find((x) => x.token_id === id);
    return l ? json({ updated_at: snap.updated_at, launch: l }, env) : json({ error: "not found" }, env, 404);
  }
  if (path === "/api/king") {
    return snap.king == null
      ? json({ error: "king view unavailable on the deployed contract" }, env, 503)
      : json({ updated_at: snap.updated_at, king: snap.king }, env);
  }
  if (path === "/api/express") {
    return snap.express == null
      ? json({ error: "express view unavailable on the deployed contract" }, env, 503)
      : json({ updated_at: snap.updated_at, express: snap.express }, env);
  }
  if (path === "/api/stats") {
    return json(
      { updated_at: snap.updated_at, num_launches: snap.num_launches, protocol: snap.protocol },
      env
    );
  }
  return json({ error: "not found" }, env, 404);
}

export default {
  async scheduled(_event, env, _ctx) {
    await poll(env);
  },
  async fetch(request, env, _ctx) {
    return handle(request, env);
  },
};

// Exported for unit tests (Node) — pure/testable pieces, no Cloudflare runtime needed.
export const _test = { spotPrice, progressBps, enrich, updateVolHistory, poll, handle };

