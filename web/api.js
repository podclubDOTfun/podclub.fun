// podclub.fun frontend read layer (STEP 2g). ONE place the UI reads on-chain state from.
//
// Two backends, in order:
//   1. the deployed Cloudflare Worker API (window.FL_API_BASE, e.g. "https://api.podclub.fun") —
//      a cached snapshot of the same on-chain views (see indexer/);
//   2. direct NEAR RPC view calls (default until the Worker is deployed).
// Both return REAL on-chain data. Nothing here fabricates a price, balance, volume or holder count:
// a value that isn't on-chain (24h change, holder count, off-chain social metrics) is returned as
// null/undefined and the UI must render it as "—", never invent it.
(function () {
  var CONTRACT_ID = (window.FL_CONTRACT_ID) || "flpad-28720.testnet";
  var RPC_URL = (window.FL_RPC_URL) || "https://test.rpc.fastnear.com";
  var API_BASE = (window.FL_API_BASE || "").replace(/\/+$/, ""); // "" → direct RPC only

  // ---- raw view call over RPC ----
  async function view(method, args) {
    // Reuse the wallet's view() if present (same RPC), else call RPC directly.
    if (window.FLWallet && typeof FLWallet.view === "function") return FLWallet.view(method, args);
    var body = {
      jsonrpc: "2.0", id: "fl", method: "query",
      params: {
        request_type: "call_function", finality: "final",
        account_id: CONTRACT_ID, method_name: method,
        args_base64: btoa(JSON.stringify(args || {})),
      },
    };
    var res = await fetch(RPC_URL, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(body) });
    var j = await res.json();
    if (j.error) throw new Error((j.error.data || j.error.message || "rpc error"));
    if (j.result && j.result.error) throw new Error(j.result.error);
    var str = new TextDecoder().decode(new Uint8Array(j.result.result));
    return str ? JSON.parse(str) : null;
  }

  // A view that may not exist on an older deployed router → null instead of throwing.
  async function tryView(method, args) { try { return await view(method, args); } catch (e) { return null; } }

  // ---- derived, non-authoritative metrics (pure fns, mirror indexer/src/index.js) ----
  var SCALE = 1000000000000000000n; // 1e18
  function spotPrice(l) {
    try {
      var cur = BigInt(l.virtual_near) + BigInt(l.real_near);
      var res = BigInt(l.token_reserve);
      if (res === 0n) return null;
      var scaled = (cur * SCALE) / res;
      var whole = scaled / SCALE;
      var frac = (scaled % SCALE).toString().padStart(18, "0").replace(/0+$/, "");
      return frac ? whole + "." + frac : String(whole);
    } catch (e) { return null; }
  }
  function progressBps(l) {
    try {
      var grad = BigInt(l.graduation_near);
      if (grad === 0n) return 0;
      var bps = (BigInt(l.real_near) * 10000n) / grad;
      return Number(bps > 10000n ? 10000n : bps);
    } catch (e) { return 0; }
  }
  function enrich(l) {
    l.spot_price = spotPrice(l);
    l.progress_bps = progressBps(l);
    if (l.volume_24h === undefined) l.volume_24h = null; // only the indexer can supply a real 24h delta
    return l;
  }

  async function apiGet(path) {
    if (!API_BASE) return null; // no Worker configured → caller uses RPC path
    var res = await fetch(API_BASE + path);
    if (res.status === 503 || res.status === 404) return null; // fall back to RPC
    if (!res.ok) throw new Error("api " + res.status);
    return res.json();
  }

  var PAGE = 50;

  var FLApi = {
    CONTRACT_ID: CONTRACT_ID,
    yoctoToNear: function (y) { try { return Number(BigInt(y) / 1000000000000000000n) / 1e6; } catch (e) { return null; } },

    // All launches, enriched. Prefers the Worker snapshot; falls back to paged direct RPC.
    launches: async function () {
      var api = await apiGet("/api/launches");
      if (api && api.launches) return api.launches;
      var num = Number(await view("get_num_launches", {}));
      var out = [];
      for (var from = 0; from < num; from += PAGE) {
        var page = await view("get_launches", { from_index: from, limit: PAGE });
        if (!Array.isArray(page) || !page.length) break;
        out.push.apply(out, page);
        if (page.length < PAGE) break;
      }
      return out.map(enrich);
    },

    // One launch by token id, enriched. null if unknown.
    launch: async function (id) {
      var api = await apiGet("/api/launch/" + encodeURIComponent(id));
      if (api && api.launch) return api.launch;
      var l = await view("get_launch", { token_id: id });
      return l ? enrich(l) : null;
    },

    // King-of-the-Hill leaderboard. null when the view is absent on the deployed contract.
    king: async function () {
      var api = await apiGet("/api/king");
      if (api && api.king) return api.king;
      return tryView("get_king_of_the_hill", { limit: 50 });
    },

    // Express Premium Board. null when unavailable.
    express: async function () {
      var api = await apiGet("/api/express");
      if (api && api.express) return api.express;
      return tryView("get_express_board", {});
    },

    stats: async function () {
      var api = await apiGet("/api/stats");
      if (api) return api;
      return {
        num_launches: Number(await view("get_num_launches", {})),
        protocol: {
          treasury: await tryView("get_treasury", {}),
          protocol_bps: await tryView("get_protocol_bps", {}),
          protocol_fees: await tryView("get_protocol_fees", {}),
        },
      };
    },

    // Exact swap previews straight from the curve (post-fee). Always live RPC — never cached.
    quoteBuy: function (id, amountYocto) { return view("quote_buy", { token_id: id, amount: amountYocto }); },
    quoteSell: function (id, tokenInYocto) { return view("quote_sell", { token_id: id, token_in: tokenInYocto }); },

    // Per-account holder-reward reads (real; may 404 on an older router → null).
    stake: function (id, account) { return tryView("get_stake", { token_id: id, account: account }); },
    pendingHolderRewards: function (id, account) { return tryView("get_pending_holder_rewards", { token_id: id, account: account }); },
    accrual: function (id) { return tryView("get_accrual", { token_id: id }); },

    // Helpers the UI shares.
    ageFrom: function (createdAtNs) {
      if (!createdAtNs) return "";
      var ms = Number(BigInt(createdAtNs) / 1000000n);
      var s = Math.max(0, (Date.now() - ms) / 1000);
      if (s < 3600) return Math.floor(s / 60) + "m";
      if (s < 86400) return Math.floor(s / 3600) + "h";
      return Math.floor(s / 86400) + "d";
    },
    spotPrice: spotPrice,
    progressBps: progressBps,
  };

  window.FLApi = FLApi;
})();
