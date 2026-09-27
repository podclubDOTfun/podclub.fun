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
// The id below is an EPHEMERAL end-to-end test router (faucet-funded). It was re-deployed for the
// 2026-09-26e seed_pool/DCL graduation proof with the fixed graduation →
// seed_pool path. It will disappear — swap it for whatever testnet router you deploy. Never put a
// mainnet id in this file.
window.FL_CONTRACT_ID = 'pdc-sp-100ca0.testnet';
window.FL_RPC_URL = 'https://test.rpc.fastnear.com';
// Optionally route reads through a deployed indexer Worker instead of direct RPC:
// window.FL_API_BASE = 'https://<your-worker>.workers.dev';
