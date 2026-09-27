// Creator dashboard — real on-chain reads (STEP 2g). Shows the connected account's own launches
// pulled from the router's view methods; never fabricates balances, fees or holder counts. Values
// the deployed router does not expose (per-creator claimable fees) render as "—", never guessed.
(function(){
  var g=function(id){return document.getElementById(id);};
  var gate=g('gate'),dash=g('dash'),holdings=g('holdings'),note=g('dashNote');
  var NR='<i class="nr"></i>';
  var loadedFor=null;

  function toNear(y){try{return Number(BigInt(y)/1000000000000000000n)/1e6;}catch(e){return null;}}
  function fmtN(n,d){if(n==null||!isFinite(n))return '—';return n.toLocaleString('en-US',{maximumFractionDigits:d==null?4:d});}
  function fmtPx(v){if(v==null||!isFinite(v))return '—';return v<0.01?v.toPrecision(3):v<1?v.toFixed(4):fmtN(v,3);}
  function compact(n){if(n==null||!isFinite(n))return '—';if(n>=1e9)return fmtN(n/1e9,2)+'B';if(n>=1e6)return fmtN(n/1e6,2)+'M';if(n>=1e3)return fmtN(n/1e3,2)+'K';return fmtN(n,2);}
  function esc(s){return String(s==null?'':s).replace(/[&<>"]/g,function(c){return {'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;'}[c];});}

  function apply(){
    var w=window.FLWallet&&FLWallet.get();
    var acct=w?w.acct:null;
    gate.style.display=acct?'none':'';
    dash.style.display=acct?'':'none';
    if(acct&&acct!==loadedFor){ loadedFor=acct; load(acct); }
    if(!acct)loadedFor=null;
  }
  window.addEventListener('fl:wallet',apply);

  function splitTag(sp){
    var cr=Math.round((sp.creator_bps||0)/100),bb=Math.round((sp.buyback_bps||0)/100),hd=Math.round((sp.holder_bps||0)/100);
    return {html:'<span class="alloc-bar" title="'+cr+'% creator · '+bb+'% buyback · '+hd+'% holders">'
      +'<span class="seg seg-c" style="width:'+cr+'%"></span>'
      +'<span class="seg seg-b" style="width:'+bb+'%"></span>'
      +'<span class="seg seg-h" style="width:'+hd+'%"></span></span>', cr:cr,bb:bb,hd:hd};
  }

  function row(l){
    var sym=l.symbol||'', spot=l.spot_price==null?null:parseFloat(l.spot_price);
    var sp=splitTag(l.split||{});
    var prog=((l.progress_bps||0)/100).toFixed(1);
    return '<div class="hrow">'
      +'<div class="tk"><div class="tlogo">'+esc((sym||'?').slice(0,1))+'</div>'
        +'<div class="tkmeta">'+esc(l.name||'')
          +'<div class="tkn-sym">$'+esc(sym)+' · <a class="scan" href="trade.html?t='+encodeURIComponent(l.token_id)+'">trade ↗</a></div>'
          +'<div class="hsplit">'+sp.html+'<span class="split-txt mono">'+sp.cr+'/'+sp.bb+'/'+sp.hd
            +' <span class="lock" title="Fixed at launch — cannot change">🔒</span></span></div>'
        +'</div></div>'
      +'<div>'+fmtPx(spot)+' '+NR+'</div>'
      +'<div class="accent">'+prog+'%</div>'
      +'<div class="hide-sm">'+esc(l.phase||'—')+'</div>'
      +'<div class="hide-sm">'+compact(toNear(l.volume))+' '+NR+'</div>'
      +'<div class="claimable">—</div>'
      +'<div class="hact"><button class="btn btn-ghost btn-sm" disabled title="Creator-fee claim view not live on the router yet">Claim</button></div>'
      +'</div>';
  }

  async function load(acct){
    holdings.innerHTML=holdings.querySelector('.head').outerHTML+'<div class="hrow"><div class="tk">Loading your launches…</div></div>';
    note.textContent='';
    var all;
    try{ all=await FLApi.launches(); }
    catch(e){ holdings.innerHTML=holdings.querySelector('.head').outerHTML+'<div class="hrow"><div class="tk">Couldn’t reach the network. Refresh to retry.</div></div>'; return; }
    var mine=all.filter(function(l){return l.creator===acct;});
    // aggregate cards (real, derivable)
    g('dCount').textContent=mine.length;
    var raised=mine.reduce(function(s,l){var n=toNear(l.real_near);return s+(n||0);},0);
    var vol=mine.reduce(function(s,l){var n=toNear(l.volume);return s+(n||0);},0);
    g('dRaised').innerHTML=fmtN(raised,2)+'<span class="u">'+NR+'</span>';
    g('dVol').innerHTML=compact(vol)+'<span class="u">'+NR+'</span>';
    // Per-creator claimable fees aren't exposed by the deployed router → honest "—".
    g('dClaim').textContent='—';

    holdings.innerHTML=holdings.querySelector('.head').outerHTML+(mine.length
      ? mine.map(row).join('')
      : '<div class="hrow"><div class="tk">No launches under '+esc(acct)+' yet — <a href="create.html">launch one</a>.</div></div>');
    note.textContent='Live from '+FLApi.CONTRACT_ID+'. Prices are on-curve spot; per-creator claimable fees appear once that view ships on the router.';
  }

  apply();
})();
