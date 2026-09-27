// podclub.fun wallet — real NEAR wiring (testnet) via near-wallet-selector, build-less ESM.
// The existing connect modal is kept as-is; wallet options now sign in for real against the
// live router. Social / import options stay in the modal but are marked "soon" until an
// MPC / key-import backend exists — we never fake a connection.
// NOTE: imports go through esm.run (jsDelivr ESM). The esm.sh builds of these packages
// leak Node globals (`require`/`Buffer`) and fail in a build-less browser context; esm.run
// ships clean browser ESM, so full multi-wallet works with no bundler.
import { setupWalletSelector } from 'https://esm.run/@near-wallet-selector/core@10.1.4';
import { setupMeteorWallet } from 'https://esm.run/@near-wallet-selector/meteor-wallet@10.1.4';
import { setupMyNearWallet } from 'https://esm.run/@near-wallet-selector/my-near-wallet@10.1.4';
import { setupNightly } from 'https://esm.run/@near-wallet-selector/nightly@10.1.4';
import { setupHotWallet } from 'https://esm.run/@near-wallet-selector/hot-wallet@10.1.4';

globalThis.global ||= globalThis;

export const NETWORK = 'testnet';
// Current testnet factory router (deployed 2026-09-25, tx DvRV3k9…). Swap when a stable named
// testnet/mainnet account is chosen. Token ids are minted as `<label>.<CONTRACT_ID>` subaccounts.
// A page may override this (and RPC_URL) for a throwaway testnet router by setting
// window.FL_CONTRACT_ID / window.FL_RPC_URL before this module loads — see web/config.testnet.js.
// The committed default is NOT touched, so testnet ids never ship in the mainnet build.
export const CONTRACT_ID = (typeof window !== 'undefined' && window.FL_CONTRACT_ID) || 'flpad-28720.testnet';
// FastNEAR public testnet RPC (rpc.testnet.near.org is deprecated).
export const RPC_URL = (typeof window !== 'undefined' && window.FL_RPC_URL) || 'https://test.rpc.fastnear.com';
// wNEAR (wrapped NEAR) token id — the curve settles trades in wNEAR and all fee/reward payouts are
// wNEAR. testnet = wrap.testnet, mainnet = wrap.near. Buys wrap NEAR→wNEAR here before the swap.
export const WNEAR_ID = NETWORK === 'mainnet' ? 'wrap.near' : 'wrap.testnet';

var SVG={
  meteor:'<svg viewBox="0 0 24 24" fill="#DE4F4F"><path d="M0 .234l21.912 20.537s.412.575-.124 1.151c-.535.576-1.236.083-1.236.083L0 .234zm6.508 2.058l17.01 15.638s.413.576-.123 1.152c-.534.576-1.235.083-1.235.083L6.508 2.292zM1.936 6.696l17.01 15.638s.412.576-.123 1.152-1.235.082-1.235.082L1.936 6.696zm10.073-2.635l11.886 10.927s.287.401-.087.805-.863.058-.863.058L12.009 4.061zm-8.567 7.737l11.886 10.926s.285.4-.088.803c-.375.403-.863.059-.863.059L3.442 11.798zm14.187-5.185l5.426 4.955s.142.188-.044.377c-.185.188-.428.027-.428.027l-4.954-5.358v-.001zM6.178 17.231l5.425 4.956s.144.188-.042.377-.427.026-.427.026l-4.956-5.359z"/></svg>',
  hot:'<svg viewBox="0 0 24 24" fill="#ff7a00"><path d="M13.5 1.5c.6 2.6-.4 4.2-1.8 5.7-1.6 1.7-3.7 3.5-3.7 6.6a6 6 0 1 0 12 0c0-1.6-.7-3.2-1.6-4.3.1 1.4-.7 2.3-1.7 2.5.8-2.6-.4-5.3-2-7.1-.5-.5-.9-1-1.2-1.4z"/></svg>',
  mnw:'<svg viewBox="0 0 24 24" fill="#e9edf2"><path d="M21.443 0c-.89 0-1.714.46-2.18 1.218l-5.017 7.448a.533.533 0 0 0 .792.7l4.938-4.282a.2.2 0 0 1 .334.151v13.41a.2.2 0 0 1-.354.128L5.03.905A2.555 2.555 0 0 0 3.078 0h-.521A2.557 2.557 0 0 0 0 2.557v18.886a2.557 2.557 0 0 0 4.736 1.338l5.017-7.448a.533.533 0 0 0-.792-.7l-4.938 4.283a.2.2 0 0 1-.333-.152V5.352a.2.2 0 0 1 .354-.128l14.924 17.87c.486.574 1.2.905 1.952.906h.521A2.558 2.558 0 0 0 24 21.445V2.557A2.558 2.558 0 0 0 21.443 0Z"/></svg>',
  nightly:'<svg viewBox="0 0 24 24" fill="#8a8df0"><path d="M12 3a9 9 0 1 0 9 9 7 7 0 0 1-9-9z"/></svg>',
  google:'<svg viewBox="0 0 48 48"><path fill="#EA4335" d="M24 9.5c3.54 0 6.71 1.22 9.21 3.6l6.85-6.85C35.9 2.38 30.47 0 24 0 14.62 0 6.51 5.38 2.56 13.22l7.98 6.19C12.43 13.72 17.74 9.5 24 9.5z"/><path fill="#4285F4" d="M46.98 24.55c0-1.57-.15-3.09-.38-4.55H24v9.02h12.94c-.58 2.96-2.26 5.48-4.78 7.18l7.73 6c4.51-4.18 7.09-10.36 7.09-17.65z"/><path fill="#FBBC05" d="M10.53 28.59c-.48-1.45-.76-2.99-.76-4.59s.27-3.14.76-4.59l-7.98-6.19C.92 16.46 0 20.12 0 24c0 3.88.92 7.54 2.56 10.78l7.97-6.19z"/><path fill="#34A853" d="M24 48c6.48 0 11.93-2.13 15.89-5.81l-7.73-6c-2.15 1.45-4.92 2.3-8.16 2.3-6.26 0-11.57-4.22-13.47-9.91l-7.98 6.19C6.51 42.62 14.62 48 24 48z"/></svg>',
  x:'<svg viewBox="0 0 24 24" fill="#e9edf2"><path d="M14.234 10.162 22.977 0h-2.072l-7.591 8.824L7.251 0H.258l9.168 13.343L.258 24H2.33l8.016-9.318L16.749 24h6.993zm-2.837 3.299-.929-1.329L3.076 1.56h3.182l5.965 8.532.929 1.329 7.754 11.09h-3.182z"/></svg>',
  import:'<svg viewBox="0 0 24 24" fill="#8b93a1"><path d="M11 3h2v9.6l3.3-3.3 1.4 1.4L12 17.4l-5.7-5.7 1.4-1.4L11 12.6V3zM5 19h14v2H5z"/></svg>'
};
var METHODS={
  meteor:{ic:SVG.meteor,nm:'Meteor Wallet',dsc:'browser extension'},
  hot:{ic:SVG.hot,nm:'HOT Wallet',dsc:'telegram · mobile'},
  mnw:{ic:SVG.mnw,nm:'MyNEAR Wallet',dsc:'web wallet'},
  nightly:{ic:SVG.nightly,nm:'Nightly',dsc:'extension · mobile'},
  google:{ic:SVG.google,nm:'Continue with Google',dsc:'auto-restores your NEAR account'},
  x:{ic:SVG.x,nm:'Continue with X',dsc:'auto-restores your NEAR account'},
  import:{ic:SVG.import,nm:'Import wallet',dsc:'paste an existing key / seed'}
};
// modal keys that map to a real wallet-selector module (the rest show "soon")
var WALLET_ID={ meteor:'meteor-wallet', hot:'hot-wallet', mnw:'my-near-wallet', nightly:'nightly' };
// ---- selector + chain helpers ----
var _selector=null, _initErr=null;
async function selector(){
  if(_selector) return _selector;
  _selector = await setupWalletSelector({
    network: NETWORK,
    modules: [ setupMeteorWallet(), setupMyNearWallet(), setupNightly(), setupHotWallet() ]
  });
  _selector.store.observable.subscribe(function(){ apply(); });
  return _selector;
}
function account(){
  if(!_selector) return null;
  var st=_selector.store.getState();
  var a=(st.accounts||[]).filter(function(x){return x.active})[0]||(st.accounts||[])[0];
  return a?a.accountId:null;
}
// Read-only view call straight to RPC — no wallet, no dependency. Defaults to the router; pass an
// explicit accountId to read another contract (launch token FT, wrap.testnet, …).
async function viewOn(accountId,method,args){
  var res=await fetch(RPC_URL,{method:'POST',headers:{'content-type':'application/json'},
    body:JSON.stringify({jsonrpc:'2.0',id:'v',method:'query',params:{request_type:'call_function',
      finality:'final',account_id:accountId,method_name:method,args_base64:btoa(JSON.stringify(args||{}))}})});
  var j=await res.json();
  if(j.error) throw new Error(j.error.data||j.error.message||'rpc error');
  if(j.result&&j.result.error) throw new Error(j.result.error);
  var str=new TextDecoder().decode(new Uint8Array(j.result.result));
  return str?JSON.parse(str):null;
}
async function view(method,args){ return viewOn(CONTRACT_ID,method,args); }
// Signed change call to the router (needs a connected wallet).
async function signAndCall(method,args,deposit,gas){
  return signAndCallOn(CONTRACT_ID,method,args,deposit,gas);
}
// Signed change call to an ARBITRARY receiver (needs a connected wallet). BUY targets wrap.testnet
// (wNEAR ft_transfer_call → router) and SELL targets the launch-token contract (ft_transfer_call →
// router); create/claim keep the router-targeted signAndCall path. Redirect wallets (MyNEAR/HOT)
// navigate away here and return to the same page — callers refresh balances/quote on return.
async function signAndCallOn(receiverId,method,args,deposit,gas){
  var s=await selector(); var w=await s.wallet();
  return w.signAndSendTransaction({ receiverId:receiverId,
    actions:[{type:'FunctionCall',params:{methodName:method,args:args||{},
      gas:gas||'30000000000000', deposit:deposit||'0'}}] });
}
// Sign a SEQUENCE of function-call actions against ONE receiver, atomically in a single tx (used to
// batch wNEAR storage_deposit + near_deposit + ft_transfer_call for BUY). Each step is
// {methodName,args,deposit,gas}.
async function signAndBatch(receiverId,steps){
  var s=await selector(); var w=await s.wallet();
  return w.signAndSendTransaction({ receiverId:receiverId,
    actions:steps.map(function(st){ return {type:'FunctionCall',params:{
      methodName:st.methodName, args:st.args||{},
      gas:st.gas||'30000000000000', deposit:st.deposit||'0' }}; }) });
}
// ---- modal (same markup as the shipped design) ----
var modal;
function opt(k){var m=METHODS[k];
  return '<button class="wopt" data-m="'+k+'"><span class="ic">'+m.ic+'</span>'
    +'<span><span class="nm">'+m.nm+'</span><br><span class="dsc">'+m.dsc+'</span></span>'
    +'<span class="go">→</span></button>';
}
function build(){
  modal=document.createElement('div'); modal.className='modal-bg';
  modal.innerHTML=''
  +'<div class="modal" role="dialog" aria-modal="true">'
  +'<div class="modal-head"><h3>Connect to podclub.fun</h3><button class="x" data-close>×</button></div>'
  +'<p class="cap">One account for launching, trading and claiming fees. <b>testnet</b></p>'
  +'<div class="wsec">NEAR wallets</div>'
  +opt('meteor')+opt('hot')+opt('mnw')+opt('nightly')
  +'<div class="wdiv">or sign in — we create &amp; restore your NEAR account</div>'
  +opt('google')+opt('x')
  +'<div class="wdiv">advanced</div>'+opt('import')
  +'<div class="wmsg" id="wmsg"></div>'
  +'<div class="wsafe">Social login derives the same NEAR account every time via a non-custodial MPC network — your keys are never stored by us and never regenerated. Export anytime.</div>'
  +'</div>';
  document.body.appendChild(modal);
  modal.addEventListener('click',function(e){
    if(e.target===modal||e.target.hasAttribute('data-close'))return close();
    var o=e.target.closest('[data-m]'); if(o)pick(o.getAttribute('data-m'));
  });
  document.addEventListener('keydown',function(e){if(e.key==='Escape')close()});
}
function wmsg(t,bad){var el=modal&&modal.querySelector('#wmsg'); if(!el)return;
  el.textContent=t||''; el.className='wmsg'+(t?' on':'')+(bad?' bad':''); }
function open(){ if(!modal)build(); selector().catch(function(e){wmsg('Wallet init failed: '+e.message,1);}); modal.classList.add('on'); wmsg(''); }
function close(){ if(modal)modal.classList.remove('on'); }

async function pick(k){
  if(!WALLET_ID[k]){ wmsg('“'+METHODS[k].nm+'” isn’t wired yet on testnet — use a NEAR wallet above.',1); return; }
  wmsg('Opening '+METHODS[k].nm+'…');
  try{
    var s=await selector();
    var w=await s.wallet(WALLET_ID[k]);
    await w.signIn({ contractId: CONTRACT_ID, methodNames: [] });
    close(); apply();               // injected wallets land here; redirect wallets navigate away
  }catch(e){ wmsg((e&&e.message)||'Could not connect. Is the wallet installed?',1); }
}
async function disconnect(){
  try{ var s=await selector(); var w=await s.wallet(); await w.signOut(); }catch(e){}
  apply();
}
// ---- reflect connection state into every [data-connect] slot ----
function apply(){
  var acct=account();
  if(acct){
    document.querySelectorAll('[data-connect]').forEach(function(el){
      el.outerHTML='<span class="acct" data-connect><span class="dot"></span>'+acct
        +'<button class="out" data-disconnect title="Disconnect">⏻</button></span>';
    });
  } else {
    document.querySelectorAll('.acct[data-connect]').forEach(function(el){
      el.outerHTML='<button class="btn btn-ghost" data-connect>Connect wallet</button>';
    });
  }
  window.dispatchEvent(new CustomEvent('fl:wallet',{detail:acct?{acct:acct}:null}));
}

document.addEventListener('click',function(e){
  if(e.target.closest('[data-disconnect]')){e.preventDefault();return disconnect();}
  if(e.target.closest('[data-connect]')){e.preventDefault();return open();}
});

window.FLWallet={open:open,disconnect:disconnect,
  get:function(){var a=account();return a?{acct:a}:null;},
  signAndCall:signAndCall,signAndCallOn:signAndCallOn,signAndBatch:signAndBatch,
  view:view,viewOn:viewOn,
  CONTRACT_ID:CONTRACT_ID,WNEAR_ID:WNEAR_ID,NETWORK:NETWORK,RPC_URL:RPC_URL};

// Init on load so a redirect-return (MyNEAR / HOT) is captured and the nav reflects it.
selector().then(apply).catch(function(e){ _initErr=e; console.error('[FLWallet] init failed',e); });
if(document.readyState!=='loading')apply(); else document.addEventListener('DOMContentLoaded',apply);
