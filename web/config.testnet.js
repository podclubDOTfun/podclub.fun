// podclub.fun — TESTNET-ONLY frontend config (opt-in; NOT loaded by any page by default).
//
// Purpose: point the static app at a throwaway testnet router without editing the committed
// prod defaults in wallet.js / api.js. To use, add this ONE line ABOVE the module scripts on
// any page you want to test (index/explore/trade/dashboard/create), e.g.:
//
//     <script src="config.testnet.js"></script>
//     <script type="module" src="wallet.js"></script>
//
// wallet.js and api.js both read window.FL_CONTRACT_ID / window.FL_RPC_URL when present and
// fall back to their committed defaults otherwise — so nothing here ships in a mainnet build.
//
// The id below is the STABLE (kept, non-throwaway) testnet router — the B1 build (router wasm
// sha256 8fe5dab6…, global FT by-hash 569a25e3…) deployed 2026-09-27 for the web write-path
// wiring proof (buy / sell / creator-claim / holder-claim). Seed launches: pod1.podclubfun.testnet,
// pod2.podclubfun.testnet. Never put a mainnet id in this file.
window.FL_CONTRACT_ID = 'podclubfun.testnet';
window.FL_RPC_URL = 'https://test.rpc.fastnear.com';
// Optionally route reads through a deployed indexer Worker instead of direct RPC:
// window.FL_API_BASE = 'https://<your-worker>.workers.dev';
