// Creator dashboard — real on-chain reads + write-path claims (web write-path wiring).
// Shows the connected account's own launches (creator fees, claimable via router
// claim_creator_fees) AND any launch where it holds a claimable B1 holder reward (claimable via
// the launch-token FT's claim_rewards). Never fabricates a number: a value not on-chain is "—".
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
  function pos(y){try{return BigInt(y||'0')>0n;}catch(e){return false;}}

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

  // A creator-launch row with real per-launch creator-claimable (get_accrual.creator) + live Claim.
  function row(l){
    var sym=l.symbol||'', spot=l.spot_price==null?null:parseFloat(l.spot_price);
    var sp=splitTag(l.split||{});
    var prog=((l.progress_bps||0)/100).toFixed(1);
    var claim=l._creatorClaim||'0', claimN=toNear(claim), canClaim=pos(claim);
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
      +'<div class="claimable">'+(claimN!=null?fmtN(claimN,4)+' '+NR:'—')+'</div>'
      +'<div class="hact"><button class="btn '+(canClaim?'btn-primary':'btn-ghost')+' btn-sm" data-claim-creator="'+esc(l.token_id)+'"'
        +(canClaim?'':' disabled title="No creator fees accrued yet"')+'>Claim</button></div>'
      +'</div>';
  }

  // Injected "Holder rewards" section: any launch (created by anyone) where the connected account
  // holds a claimable B1 reward. Claimed from the launch-token FT (claim_rewards, 0 deposit).
  function holderSection(items){
    var box=g('holderRewards');
    if(!box){ box=document.createElement('div'); box.id='holderRewards'; box.style.marginTop='28px'; dash.appendChild(box); }
    if(!items.length){ box.innerHTML=''; return; }
    box.innerHTML='<h3 style="margin:8px 0 4px">Holder rewards</h3>'
      +'<p class="lead" style="margin-top:0">wNEAR you’ve earned just by holding these tokens — no stake, claim anytime.</p>'
      +'<div class="holdings">'
      +'<div class="hrow head"><div>Token</div><div>Balance</div><div>Claimable reward</div><div></div></div>'
      +items.map(function(it){
        return '<div class="hrow">'
          +'<div class="tk"><div class="tlogo">'+esc((it.sym||'?').slice(0,1))+'</div>'
            +'<div class="tkmeta">'+esc(it.name||'')+'<div class="tkn-sym">$'+esc(it.sym)
              +' · <a class="scan" href="trade.html?t='+encodeURIComponent(it.token_id)+'">trade ↗</a></div></div></div>'
          +'<div>'+compact(toNear(it.bal))+'</div>'
          +'<div class="claimable accent">'+fmtN(toNear(it.reward),6)+' '+NR+'</div>'
          +'<div class="hact"><button class="btn btn-primary btn-sm" data-claim-holder="'+esc(it.token_id)+'">Claim rewards</button></div>'
          +'</div>';
      }).join('')+'</div>';
  }

  async function load(acct){
    holdings.innerHTML=holdings.querySelector('.head').outerHTML+'<div class="hrow"><div class="tk">Loading your launches…</div></div>';
    note.textContent='';
    var all;
    try{ all=await FLApi.launches(); }
    catch(e){ holdings.innerHTML=holdings.querySelector('.head').outerHTML+'<div class="hrow"><div class="tk">Couldn’t reach the network. Refresh to retry.</div></div>'; return; }
    var mine=all.filter(function(l){return l.creator===acct;});

    // Per-launch creator-claimable (real): router get_accrual(token_id).creator, in yocto wNEAR.
    var accr=await Promise.all(mine.map(function(l){
      return FLWallet.viewOn(FLApi.CONTRACT_ID,'get_accrual',{token_id:l.token_id}).catch(function(){return null;});
    }));
    mine.forEach(function(l,i){ l._creatorClaim=(accr[i]&&accr[i].creator)||'0'; });

    g('dCount').textContent=mine.length;
    var raised=mine.reduce(function(s,l){var n=toNear(l.real_near);return s+(n||0);},0);
    var vol=mine.reduce(function(s,l){var n=toNear(l.volume);return s+(n||0);},0);
    g('dRaised').innerHTML=fmtN(raised,2)+'<span class="u">'+NR+'</span>';
    g('dVol').innerHTML=compact(vol)+'<span class="u">'+NR+'</span>';

    var totalClaim=mine.reduce(function(s,l){var n=toNear(l._creatorClaim);return s+(n||0);},0);
    g('dClaim').innerHTML=fmtN(totalClaim,4)+'<span class="u">'+NR+'</span>';
    var sub=g('dClaimSub'); if(sub)sub.textContent='creator fees, claimable in wNEAR';
    var all_btn=g('claimAll'); if(all_btn){ all_btn.disabled=!(totalClaim>0); }

    holdings.innerHTML=holdings.querySelector('.head').outerHTML+(mine.length
      ? mine.map(row).join('')
      : '<div class="hrow"><div class="tk">No launches under '+esc(acct)+' yet — <a href="create.html">launch one</a>.</div></div>');
    note.textContent='Live from '+FLApi.CONTRACT_ID+'. Prices are on-curve spot; creator fees are claimable in wNEAR.';

    // Holder rewards across ALL launches for the connected account (B1 hold-to-earn).
    try{
      var rew=await Promise.all(all.map(function(l){
        return Promise.all([
          FLWallet.viewOn(l.token_id,'get_rewards',{account:acct}).catch(function(){return null;}),
          FLWallet.viewOn(l.token_id,'ft_balance_of',{account_id:acct}).catch(function(){return null;})
        ]);
      }));
      var items=[];
      all.forEach(function(l,i){ var r=rew[i][0], b=rew[i][1];
        if(pos(r)) items.push({token_id:l.token_id,sym:l.symbol,name:l.name,reward:r,bal:b||'0'}); });
      holderSection(items);
    }catch(e){ /* leave holder section absent on read failure */ }
  }

  // ---- write path: claims ----
  function txLink(tx){ var h=(window.FLWallet&&FLWallet.NETWORK==='mainnet')?'https://nearblocks.io':'https://testnet.nearblocks.io'; return tx?(' — <a href="'+h+'/txns/'+tx+'" target="_blank" rel="noopener">'+tx.slice(0,10)+'… ↗</a>'):''; }
  async function withBtn(btn,label,fn){
    var old=btn.textContent, wasDisabled=btn.disabled; btn.disabled=true; btn.textContent='Confirm in wallet…';
    try{
      var r=await fn();
      var tx=r&&r.transaction&&r.transaction.hash;
      note.innerHTML=label+' claimed'+txLink(tx); note.classList.remove('bad');
      if(loadedFor)load(loadedFor);                       // refresh amounts after an injected-wallet claim
    }catch(e){
      note.innerHTML=label+' claim failed: '+esc((e&&e.message)||'cancelled'); note.classList.add('bad');
      btn.disabled=wasDisabled; btn.textContent=old;
    }
  }

  document.addEventListener('click',function(e){
    var cc=e.target.closest&&e.target.closest('[data-claim-creator]');
    var ch=e.target.closest&&e.target.closest('[data-claim-holder]');
    var ca=e.target.closest&&e.target.closest('#claimAll');
    if(cc){ e.preventDefault(); var t=cc.getAttribute('data-claim-creator');
      return withBtn(cc,'Creator fees',function(){ return FLWallet.signAndCall('claim_creator_fees',{token_id:t},'0','60000000000000'); }); }
    if(ch){ e.preventDefault(); var tk=ch.getAttribute('data-claim-holder');
      return withBtn(ch,'Holder reward',function(){ return FLWallet.signAndCallOn(tk,'claim_rewards',{},'0','60000000000000'); }); }
    if(ca){ e.preventDefault(); if(ca.disabled)return;
      // Claim each launch that has a creator balance; sequential so redirect wallets don't race.
      return (async function(){
        var btns=[].slice.call(document.querySelectorAll('[data-claim-creator]')).filter(function(b){return !b.disabled;});
        for(var i=0;i<btns.length;i++){ btns[i].click(); }   // per-row handler does the signing + refresh
      })();
    }
  });

  apply();
})();
