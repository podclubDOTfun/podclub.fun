// podclub.fun — live NEAR/USD spot price helper (real oracle).
//
// USD-default display needs a live NEAR price to convert
// on-chain NEAR amounts (fees, MCAP set via virtual_near) into USD. This value is REAL market data
// pulled from public price APIs — it is NOT part of the protocol and never originates any on-chain
// value. If every source fails we expose `null`; callers must render a neutral placeholder ("—")
// and NEVER fabricate a number. No API key, no tracking.
(function () {
  var STATE = { usd: null, at: 0 };          // last good NEAR→USD spot + when we got it (ms)
  var TTL = 60 * 1000;                        // refresh at most once a minute
  var waiters = [];                           // resolve() callbacks awaiting the first quote
  var inflight = false;

  // Public, CORS-enabled, key-less spot sources. Tried in order; first that parses wins.
  var SOURCES = [
    {
      url: "https://api.coingecko.com/api/v3/simple/price?ids=near&vs_currencies=usd",
      pick: function (j) { return j && j.near && j.near.usd; },
    },
    {
      url: "https://api.binance.com/api/v3/ticker/price?symbol=NEARUSDT",
      pick: function (j) { return j && j.price ? parseFloat(j.price) : null; },
    },
  ];

  async function fetchOnce() {
    for (var i = 0; i < SOURCES.length; i++) {
      try {
        var res = await fetch(SOURCES[i].url, { headers: { accept: "application/json" } });
        if (!res.ok) continue;
        var j = await res.json();
        var v = SOURCES[i].pick(j);
        if (typeof v === "number" && isFinite(v) && v > 0) return v;
      } catch (_) { /* try the next source */ }
    }
    return null; // all sources failed — caller keeps the last good value or shows "—"
  }

  async function refresh() {
    if (inflight) return STATE.usd;
    inflight = true;
    try {
      var v = await fetchOnce();
      if (v != null) { STATE.usd = v; STATE.at = Date.now(); }
    } finally {
      inflight = false;
      var w = waiters; waiters = [];
      w.forEach(function (fn) { try { fn(STATE.usd); } catch (_) {} });
      // Broadcast so any page can re-render USD figures when a fresh quote lands.
      try { window.dispatchEvent(new CustomEvent("fl:price", { detail: { usd: STATE.usd } })); } catch (_) {}
    }
    return STATE.usd;
  }

  // Latest cached NEAR→USD (may be null before the first successful fetch). Synchronous.
  function usd() { return STATE.usd; }

  // Promise<number|null> — resolves with a spot, refreshing if the cache is cold/stale.
  function ready() {
    if (STATE.usd != null && Date.now() - STATE.at < TTL) return Promise.resolve(STATE.usd);
    var p = new Promise(function (resolve) { waiters.push(resolve); });
    refresh();
    return p;
  }

  // Convert a NEAR amount (Number) to a USD string like "$1,234" / "$0.67"; null → "—".
  function nearToUsd(near) {
    if (STATE.usd == null || near == null || !isFinite(near)) return "—";
    return fmtUsd(near * STATE.usd);
  }

  // Convert a target USD amount to whole NEAR (Number); null if no live price yet.
  function usdToNear(usdAmount) {
    if (STATE.usd == null || !STATE.usd) return null;
    return usdAmount / STATE.usd;
  }

  function fmtUsd(v) {
    if (v == null || !isFinite(v)) return "—";
    if (v >= 1000) return "$" + Math.round(v).toLocaleString("en-US");
    if (v >= 1) return "$" + v.toFixed(2);
    if (v >= 0.01) return "$" + v.toFixed(3);
    return "$" + v.toPrecision(2);
  }

  window.FLPrice = { usd: usd, ready: ready, refresh: refresh, nearToUsd: nearToUsd, usdToNear: usdToNear, fmtUsd: fmtUsd, TTL: TTL };
  // Warm the cache on load and keep it fresh while the tab is open.
  refresh();
  setInterval(refresh, TTL);
})();
