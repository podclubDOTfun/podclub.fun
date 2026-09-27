// podclub.fun private-beta gate — advanced-mode Pages Function (runs on Cloudflare edge).
// Valid codes come from the BETA_CODES env var (comma-separated), set as a Pages secret.
const COOKIE = "fl_beta";

function parseCookies(h) {
  const out = {};
  (h || "").split(/;\s*/).forEach((p) => {
    const i = p.indexOf("=");
    if (i > 0) out[p.slice(0, i)] = decodeURIComponent(p.slice(i + 1));
  });
  return out;
}

function gatePage(msg) {
  return `<!doctype html><html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>podclub.fun — Private Beta</title>
<style>body{margin:0;min-height:100vh;display:grid;place-items:center;background:#0a0a0a;color:#e5e5e5;font-family:ui-monospace,Menlo,monospace}
.box{width:min(92vw,360px);padding:28px;border:1px solid #262626;border-radius:14px;background:#111}
h1{font-size:16px;margin:0 0 4px}p{color:#9a9a9a;font-size:12px;margin:0 0 18px}
input,button{width:100%;box-sizing:border-box;padding:11px 12px;border-radius:9px;font-size:14px;font-family:inherit}
input{background:#0a0a0a;border:1px solid #333;color:#fff;margin-bottom:10px}
button{background:#22c55e;border:0;color:#04120a;font-weight:700;cursor:pointer}
.err{color:#f87171;font-size:12px;margin:0 0 10px}</style></head>
<body><form class="box" method="GET" action="/">
<h1>podclub.fun — Private Beta</h1><p>Enter your access code to continue.</p>
${msg ? `<div class="err">${msg}</div>` : ""}
<input name="code" placeholder="access code" autocomplete="off" autofocus required>
<button type="submit">Enter</button></form></body></html>`;
}

export default {
  async fetch(request, env) {
    const url = new URL(request.url);

    // Let ACME / domain-validation probes bypass the beta gate so Cloudflare can
    // issue the custom-domain SSL cert (validation is served under /.well-known/).
    if (url.pathname.startsWith("/.well-known/")) {
      return env.ASSETS.fetch(request);
    }

    const codes = new Set(
      (env.BETA_CODES || "").split(",").map((s) => s.trim()).filter(Boolean),
    );
    const submitted = url.searchParams.get("code");
    const cookies = parseCookies(request.headers.get("Cookie"));

    if (submitted !== null) {
      if (codes.has(submitted)) {
        return new Response(null, {
          status: 302,
          headers: {
            "Location": url.pathname,
            "Set-Cookie": `${COOKIE}=${encodeURIComponent(submitted)}; Path=/; Secure; HttpOnly; SameSite=Lax; Max-Age=2592000`,
          },
        });
      }
      return new Response(gatePage("Invalid code."), {
        status: 401,
        headers: { "content-type": "text/html; charset=utf-8" },
      });
    }

    if (cookies[COOKIE] && codes.has(cookies[COOKIE])) {
      return env.ASSETS.fetch(request);
    }

    return new Response(gatePage(""), {
      status: 401,
      headers: { "content-type": "text/html; charset=utf-8" },
    });
  },
};
