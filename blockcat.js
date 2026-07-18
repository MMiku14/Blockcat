// ==UserScript==
// @name         ◆ Blockcat /拦猫
// @namespace    http://tampermonkey.net/
// @version      2.0.0
// @description  Rule Pipeline · URL缓存 · 统一拦截入口 · 响应过滤插件 · DPlayer构造前M3U8桥接净化(强制hls.js) · 统一DOM清道夫 · 微任务批处理
// @author       cat & Blockcat-Optimizer
// @match        *://*/*
// @run-at       document-start
// @grant        none
// ==/UserScript==

(function () {
  'use strict';

  const VERSION = '2.0.0';

  /* ═══ SECTION 1 · CONFIG ═══ */
  /* ─── 用户配置（按需调整）─── */
  const CFG = {
    log: true, logLevel: 'INFO',                // 日志总开关与最低级别
    m3u8Cleanse: true,                          // 净化 m3u8 播放列表，剥离广告分片
    m3u8Debug: false,                           // 输出 m3u8 播放列表分析日志
    mediaM3u8Bridge: true,                      // DPlayer 构造前桥接 m3u8 -> blob -> hls.js
    blockPopups: true,                          // 拦截 window.open 弹窗
    blockAutoplay: true,                        // 拦截黑名单资源的媒体自动播放
    sanitizeCookies: true,                      // 巡查清理广告相关 Cookie 与全局锁变量
    blockPunycode: true,                        // 拦截 Punycode 混淆域名
    hookBlobMedia: false,                       // Hook URL.createObjectURL/MediaSource（调试用，默认关闭）
    exposeGlobal: false,                        // 是否将诊断 API 挂载到 window
  };

  /* ─── 内部常量（一般不需要改）─── */
  const INT = {
    mockResponses: true,                        // 拦截命中后返回伪造响应
    antiDetect: true,                           // 伪装 Function.prototype.toString
    dplayerPreBridge: true,                     // DPlayer 构造前桥接
    hookDPlayer: true,                          // Hook DPlayer 构造函数
    hookHls: true,                              // Hook Hls.js
    domInsertBlock: true,                       // 拦截恶意 DOM 插入
    domWriteBlock: true,                        // 拦截 document.write 恶意片段
    malClassScan: true,                         // 扫描混淆广告 class
    playerDiag: true,                           // 播放器诊断
    strictNonStdPort: false,                    // 拦截所有第三方非标端口
    blockCloud: false,                          // 拦截云服务商临时域名
    lruSize: 800,                               // LRU 缓存容量
    cookiePollMs: 4000,                         // Cookie 巡查间隔
    lockPollMul: 4,                             // 全局锁巡查倍数
    pendingMax: 5000,                           // MutationObserver 队列上限
    mockDelayMs: 1,                             // XHR mock 延迟
    m3u8SafetyRatio: 0.5,                       // m3u8 广告比率安全阀
    mediaM3u8MaxDepth: 4,                       // Master Playlist 递归最大深度
    mediaM3u8TimeoutMs: 8000,                   // bridge 超时
    bridgeRetry: 2,                             // bridge fetch 重试次数
    bridgeRetryMs: 500,                         // bridge fetch 重试间隔
    bridgeCacheMax: 50,                         // bridge blob 缓存上限
    playerDataPollMs: 300,                      // player_data 轮询间隔
    playerDataPollMax: 30,                      // player_data 轮询上限
  };

  const FAKE_TS = (() => { const b = new Uint8Array(188); b[0]=0x47; b[1]=0x1F; b[2]=0xFF; b[3]=0x10; return b.buffer; })();
  const BODY_ADCONFIG = '[]';
  const BODY_ADSCRIPT = '/* bc */';

  /* ═══ SECTION 2 · LOG ═══ */
  const LV = { DEBUG: 1, INFO: 2, WARN: 3, ERROR: 4, SUCCESS: 5 };
  const TH = {
    DEBUG:{bg:'#0f172a',fg:'#64748b',i:'◆'}, INFO:{bg:'#1e3a8a',fg:'#93c5fd',i:'●'},
    WARN:{bg:'#78350f',fg:'#fde68a',i:'▲'}, ERROR:{bg:'#7f1d1d',fg:'#fca5a5',i:'✕'},
    SUCCESS:{bg:'#064e3b',fg:'#6ee7b7',i:'✓'},
  };
  const MIN = CFG.log ? (LV[CFG.logLevel] || 1) : 99;
  function emit(l, t, m, d) {
    if (LV[l] < MIN) return;
    const th = TH[l], ts = new Date().toISOString().slice(11, 23);
    const a = '%c' + th.i + ' BC[' + ts + ']%c ' + t;
    const s1 = 'background:' + th.bg + ';color:' + th.fg + ';padding:1px 7px;border-radius:3px;font:700 9px/14px monospace;';
    const s2 = 'color:' + th.fg + ';font:600 11px/14px monospace;margin-left:4px;';
    d ? (console.groupCollapsed(a, s1, s2, '|', m), console.dir(d), console.groupEnd())
      : console.log(a, s1, s2, '|', m);
  }
  const Log = { debug:(t,m,d)=>emit('DEBUG',t,m,d), info:(t,m,d)=>emit('INFO',t,m,d),
    warn:(t,m,d)=>emit('WARN',t,m,d), error:(t,m,d)=>emit('ERROR',t,m,d), ok:(t,m,d)=>emit('SUCCESS',t,m,d) };

  /* ═══ SECTION 3 · STATS（分组）═══ */
  const Stats = {
    block:  { fetch:0, xhr:0, beacon:0, popup:0, script:0, img:0, domInsert:0, domRemoved:0, cookie:0, autoplay:0 },
    bridge: { mediaBridge:0, dplayerBridge:0, hlsFiltered:0, bridgeRetried:0 },
    cache:  { cacheHit:0 },
    diag:   { playerData:0, blobCreated:0, sbChunk:0, m3u8Cleaned:0 },
  };
  const report = () => Log.info('统计报告', '拦截汇总', JSON.parse(JSON.stringify(Stats)));

  /* ═══ SECTION 4 · ANTI-DETECT ═══ */
  const SYM = Symbol('__bc__');
  let mimic = fn => fn;
  if (INT.antiDetect) try {
    const nts = Function.prototype.toString;
    Object.defineProperty(Function.prototype, 'toString', {
      value: function toString() { return (typeof this === 'function' && this[SYM]) ? this[SYM] : nts.call(this); },
      writable: true, configurable: true, enumerable: false,
    });
    Function.prototype.toString[SYM] = 'function toString() { [native code] }';
    mimic = (fn, name) => { try { Object.defineProperty(fn, 'name', { value: name, configurable: true }); } catch (_) {}
      try { Object.defineProperty(fn, SYM, { value: 'function ' + name + '() { [native code] }', enumerable: false, configurable: true }); } catch (_) {}
      return fn; };
  } catch (e) { Log.warn('AntiDetect', e.message); }

  /* ═══ SECTION 5 · LRU + URL 缓存 ═══ */
  class LRU {
    constructor(cap) { this.cap = cap; this.m = new Map(); }
    get(k){ const v=this.m.get(k); if(v===undefined) return undefined; this.m.delete(k); this.m.set(k,v); Stats.cache.cacheHit++; return v; }
    set(k,v){ if(this.m.has(k)) this.m.delete(k); else if(this.m.size>=this.cap) this.m.delete(this.m.keys().next().value); this.m.set(k,v); }
    get size(){ return this.m.size; }
  }
  const cache = new LRU(INT.lruSize);
  const urlCache = new LRU(INT.lruSize);
  function parseURL(u, base){ if(!base){ const c=urlCache.get(u); if(c!==undefined) return c; }
    let r=null; try{ r=new URL(u, base||location.origin); }catch(_){}
    if(!base) urlCache.set(u,r); return r; }

  /* ═══ SECTION 6 · FROZEN VERDICTS ═══ */
  const ALLOW = Object.freeze({ blocked:false, type:'', reason:'' });
  const V = (type, reason) => Object.freeze({ blocked:true, type, reason });
  const V_ADSCRIPT = V('AdScript','黑产广告脚本');
  const V_ADCONFIG = V('AdConfig','广告中心化配置');
  const V_PUNYCODE = V('Punycode','Punycode 混淆域名');

  /* ═══ SECTION 7 · UNIFIED TRIE ═══ */
  class Trie {
    constructor(tok){ this.root={c:{},end:false,meta:null,wild:false}; this.tok=tok; }
    add(key, meta){ const s=this.tok(key); let n=this.root;
      for(const g of s){ if(g==='*'){ n.wild=true; n.meta=meta; n.end=true; return; }
        (n.c[g]=n.c[g]||{c:{},end:false,meta:null,wild:false}); n=n.c[g]; }
      n.end=true; n.meta=meta; }
    match(key){ const s=this.tok(key); let n=this.root;
      for(const g of s){ if(n.wild) return n.meta; const x=n.c[g]; if(!x) return null; n=x; if(n.end) return n.meta; }
      return n.end?n.meta:null; }
  }
  const PT = new Trie(p => p.replace(/\/+$/,'').split('/').filter(Boolean));
  PT.add('/000/flink/click.php', V('Tracker','流量追踪-点击'));
  PT.add('/000/flink/analytics.php', V('Tracker','流量追踪-统计'));
  PT.add('/000/flink/url.php', V('Tracker','流量追踪-跳转'));
  PT.add('/000/flink/check_domain.php', V('Tracker','流量追踪-域名探测'));
  PT.add('/000/report_error_video/*', V('Tracker','视频错误上报'));
  PT.add('/ajax/hits', V('Tracker','点击率更新'));
  const HT = new Trie(h => h.split('.').filter(Boolean).reverse());
  HT.add('amazonaws.com', V('CloudInfra','AWS 临时域名'));
  HT.add('cloudapp.azure.com', V('CloudInfra','Azure 临时域名'));

  /* ═══ SECTION 8 · 工具函数 ═══ */
  const AD_KW = ['/fixed_ui_','/fixed_jump_'];
  function hasAny(s, arr){ for(let i=0;i<arr.length;i++) if(s.indexOf(arr[i])!==-1) return true; return false; }
  function isNonStdPort(port){ return !!port && port!=='80' && port!=='443'; }
  function isPrivateIP(h){ return h==='localhost'||h==='127.0.0.1'||h.indexOf('192.168.')===0||h.indexOf('10.')===0||/^172\.(1[6-9]|2\d|3[01])\./.test(h); }
  function isIP(h){
    if(!h) return false;
    if(h[0]==='[') return h.indexOf(':')!==-1;
    if(h.indexOf(':')!==-1) return true;
    let d=0,s=0,on=false;
    for(let i=0;i<h.length;i++){ const c=h.charCodeAt(i);
      if(c===46){ if(!on||s>255) return false; d++; s=0; on=false; } else if(c>=48&&c<=57){ s=s*10+(c-48); on=true; } else return false; }
    return d===3&&on&&s<=255;
  }

  /* ═══ SECTION 9 · RULE PIPELINE + DECISION ═══ */
  const SELF = location.hostname.toLowerCase();
  const RULES = [
    ({path}) => PT.match(path),
    ({host,path}) => { if(host!==SELF||path.indexOf('/abc/')===-1) return null;
      if(path.endsWith('.js')&&hasAny(path,AD_KW)) return V_ADSCRIPT;
      if(path.endsWith('.json')&&path.indexOf('/data_')!==-1) return V_ADCONFIG; return null; },
    ({host}) => INT.blockCloud ? HT.match(host) : null,
    ({host,port}) => { if(!isNonStdPort(port)||isPrivateIP(host)||host===SELF) return null;
      if(INT.strictNonStdPort) return V('BadPort','第三方非标端口 :'+port);
      if(isIP(host)) return V('BadPortIP','IP+非标端口 '+host+':'+port); return null; },
    ({host}) => (CFG.blockPunycode && host.indexOf('xn--')!==-1) ? V_PUNYCODE : null,
  ];

  function decide(raw){
    if(!raw) return ALLOW;
    if(typeof raw!=='string'){ try{ raw=String(raw); }catch(_){ return ALLOW; } if(!raw) return ALLOW; }
    const c = cache.get(raw); if(c!==undefined) return c;
    const u = parseURL(raw);
    if(!u){ const r = hasAny(raw, AD_KW) ? V_ADSCRIPT : ALLOW; cache.set(raw, r); return r; }
    const ctx = { raw, url:u, path:u.pathname, host:u.hostname.toLowerCase(), port:u.port };
    let verdict = ALLOW;
    for(const rule of RULES){ const r=rule(ctx); if(r&&r.blocked){ verdict=r; break; } }
    if(verdict.blocked) Log.warn('Policy','['+verdict.type+'] '+verdict.reason+' → '+raw.slice(0,120));
    cache.set(raw, verdict);
    return verdict;
  }
  const interceptURL = raw => decide(raw);

  /* ═══ SECTION 10 · MOCK ═══ */
  function mock(type){
    if(type==='AdConfig') return new Response(BODY_ADCONFIG,{status:200,headers:{'Content-Type':'application/json'}});
    if(type==='AdScript') return new Response(BODY_ADSCRIPT,{status:200,headers:{'Content-Type':'application/javascript'}});
    return new Response(FAKE_TS,{status:200,headers:{'Content-Type':'video/MP2T'}});
  }

  /* ═══ SECTION 11 · RESPONSE FILTER PIPELINE（M3U8 净化 + Debug）═══ */
  const M3U8_RE = /\.m3u8(?:$|[?#])/i;
  const M3U8_AD_KW = ['ad_','ad.','creative','fixed','jump','flink','/ads/'];

  function debugM3U8(text, src){
    if(!CFG.m3u8Debug || !text || text.indexOf('#EXTM3U')===-1) return;
    const lines=text.split(/\r?\n/), segs=[], hosts=new Map(), suspicious=[];
    for(const raw of lines){ const ln=raw.trim(); if(!ln||ln[0]==='#') continue; segs.push(ln);
      const uu=parseURL(ln,src), h=uu?uu.hostname.toLowerCase():'(未知)'; hosts.set(h,(hosts.get(h)||0)+1);
      if(hasAny(ln.toLowerCase(), M3U8_AD_KW)) suspicious.push(ln); }
    Log.info('M3U8-Debug','播放列表解析 '+src.slice(0,80),{ url:src, totalSegments:segs.length, hostDistribution:[...hosts.entries()], preview:segs.slice(0,10) });
    if(suspicious.length) Log.warn('M3U8-Debug','疑似广告/追踪分片 '+suspicious.length+'/'+segs.length, suspicious.slice(0,20));
  }

  const M3U8Filter = {
    test(url){ return CFG.m3u8Cleanse && M3U8_RE.test(url); },
    apply(text, src){
      debugM3U8(text, src);
      if(!text || text.indexOf('#EXTM3U')===-1) return text;
      const lines=text.split(/\r?\n/), segs=[];
      let inf=0, disc=false;
      for(let i=0;i<lines.length;i++){ const ln=lines[i].trim();
        if(ln.indexOf('#EXTINF:')===0) inf=parseFloat(ln.slice(ln.indexOf(':')+1));
        else if(ln==='#EXT-X-DISCONTINUITY') disc=true;
        else if(ln && ln[0]!=='#'){ const sl=ln.lastIndexOf('/'); segs.push({line:i,url:ln,dir:sl>0?ln.slice(0,sl):'',dur:inf,disc}); inf=0; disc=false; } }
      if(segs.length<4) return text;
      const freq=Object.create(null); for(const s of segs){ const k=s.dir||'(root)'; freq[k]=(freq[k]||0)+1; }
      let main='', max=0; for(const k in freq) if(freq[k]>max){ max=freq[k]; main=k; }
      const ad=new Set();
      for(let k=0;k<segs.length;k++){ const s=segs[k], sd=s.dir||'(root)', ul=s.url.toLowerCase();
        let bad=sd!==main||ul.indexOf('ad_')!==-1||ul.indexOf('creative')!==-1||ul.indexOf('fixed_')!==-1||ul.indexOf('flink')!==-1;
        if(!bad){ const uu=parseURL(s.url,src); if(uu&&isNonStdPort(uu.port)&&isIP(uu.hostname.toLowerCase())) bad=true; }
        if(bad) ad.add(k); }
      if(ad.size>segs.length*INT.m3u8SafetyRatio){
        const dirs=Object.create(null); for(const idx of ad){ const sd=segs[idx].dir||'(root)'; dirs[sd]=(dirs[sd]||0)+1; }
        Log.warn('M3U8','广告比率过高，触发防误杀',{ mainDir:main, adDirs:dirs, adCount:ad.size, total:segs.length, ratio:(ad.size/segs.length).toFixed(2) });
        return text; }
      const adLines=new Set();
      for(const idx of ad){ const sl=segs[idx].line; adLines.add(sl);
        for(let r=sl-1;r>=0;r--){ const t=lines[r].trim();
          if(t.indexOf('#EXTINF')===0||t==='#EXT-X-DISCONTINUITY'||t.indexOf('#EXT-X-BYTERANGE')===0) adLines.add(r); else break; } }
      const out=[]; let pd=false;
      for(let i=0;i<lines.length;i++){ if(adLines.has(i)) continue; const t=lines[i].trim();
        if(t==='#EXT-X-DISCONTINUITY'){ if(pd) continue; pd=true; } else if(t) pd=false; out.push(lines[i]); }
      if(ad.size>0){ Stats.diag.m3u8Cleaned+=ad.size; Log.ok('M3U8','净化 剥除'+ad.size+'段/共'+segs.length); return out.join('\n'); }
      return text;
    },
  };

  const ResponseFilters = [M3U8Filter];
  function applyFilters(url, text){ for(const f of ResponseFilters) if(f.test(url)) return f.apply(text, url); return text; }
  function anyFilter(url){ for(const f of ResponseFilters) if(f.test(url)) return true; return false; }

  /* ═══ SECTION 12 · M3U8 BRIDGE（fetch 重试 + 并行 rewrite + blob 缓存清理）═══ */
  const RAW_FETCH = window.fetch ? window.fetch.bind(window) : null;
  const bridgeCache = new Map();
  const blobRegistry = [];

  function withTimeout(p, ms, tag){ if(!ms) return p; return new Promise((res, rej) => {
      const t=setTimeout(()=>rej(new Error(tag||'timeout')),ms);
      p.then(v=>{clearTimeout(t);res(v);},e=>{clearTimeout(t);rej(e);}); }); }

  async function fetchWithRetry(url, opts, retries, delay){
    for(let i=0;i<=retries;i++){
      try{ const r=await RAW_FETCH(url, opts); if(r.ok) return r;
        if(r.status>=400&&r.status<500) throw new Error('HTTP '+r.status);
      }catch(e){ if(i>=retries) throw e;
        Stats.bridge.bridgeRetried++;
        Log.debug('Bridge','fetch 重试 #'+(i+1)+' '+url.slice(0,80));
        await new Promise(r=>setTimeout(r, delay)); } }
  }

  function cleanBlobRegistry(){
    while(blobRegistry.length>INT.bridgeCacheMax){
      const old=blobRegistry.shift();
      try{ URL.revokeObjectURL(old); }catch(_){}
      for(const [k,v] of bridgeCache){ if(v===old){ bridgeCache.delete(k); break; } }
    }
  }

  async function rewriteTagURI(line, base, depth){
    const re=/(URI=)(["'])([^"']+)\2/g; let m,last=0,out='',hit=false;
    while((m=re.exec(line))){ hit=true; out+=line.slice(last,m.index);
      const abs=parseURL(m[3],base); let rep=abs?abs.href:m[3];
      if(abs&&M3U8_RE.test(rep)&&depth<INT.mediaM3u8MaxDepth){ try{ rep=await bridgeM3U8(rep,depth+1); }catch(_){ rep=abs.href; } }
      out+=m[1]+m[2]+rep+m[2]; last=re.lastIndex; }
    return hit?out+line.slice(last):line; }

  async function rewriteM3U8Line(line, base, depth){
    if(!line) return line; const t=line.trim(); if(!t) return line;
    if(t[0]==='#') return rewriteTagURI(line,base,depth);
    const abs=parseURL(t,base); if(!abs) return line;
    let rep=abs.href;
    if(M3U8_RE.test(rep)&&depth<INT.mediaM3u8MaxDepth){ try{ rep=await bridgeM3U8(rep,depth+1); }catch(_){ rep=abs.href; } }
    return rep; }

  async function rewritePlaylist(text, base, depth){
    const lines=text.split(/\r?\n/);
    const out=await Promise.all(lines.map(ln=>rewriteM3U8Line(ln,base,depth)));
    return out.join('\n'); }

  function bridgeM3U8(url, depth){
    if(!CFG.mediaM3u8Bridge||!RAW_FETCH) return Promise.resolve(url);
    const c=bridgeCache.get(url); if(c) return c;
    const p=withTimeout((async()=>{
      const t0=(typeof performance!=='undefined'&&performance.now)?performance.now():Date.now();
      const resp=await fetchWithRetry(url,{credentials:'omit'},INT.bridgeRetry,INT.bridgeRetryMs);
      let text=await resp.text();
      text=applyFilters(url,text);
      if(!text||text.indexOf('#EXTM3U')===-1) throw new Error('非M3U8响应');
      const out=await rewritePlaylist(text,url,depth);
      const blob=new Blob([out],{type:'application/vnd.apple.mpegurl'});
      const b=URL.createObjectURL(blob);
      blobRegistry.push(b);
      cleanBlobRegistry();
      Stats.bridge.mediaBridge++;
      Log.ok('Bridge','M3U8 桥接 depth='+depth+' '+url.slice(0,100),{blob:b,cost:Math.round(((typeof performance!=='undefined'&&performance.now)?performance.now():Date.now())-t0)});
      return b;
    })(),INT.mediaM3u8TimeoutMs,'M3U8 bridge timeout').catch(e=>{bridgeCache.delete(url);throw e;});
    bridgeCache.set(url,p);
    return p; }

  /* ═══ SECTION 13 · NETWORK HOOKS（fetch / XHR / beacon）═══ */
  if(window.fetch){ const o=window.fetch; window.fetch=mimic(function fetch(res,init){
    const url=typeof res==='string'?res:(res instanceof Request?res.url:(res&&typeof res.href==='string'?res.href:''));
    const d=interceptURL(url);
    if(d.blocked){ Stats.block.fetch++; return Promise.resolve(INT.mockResponses?mock(d.type):new Response(null,{status:200})); }
    return o.apply(this,arguments).then(resp=>{
      if(resp.ok&&anyFilter(url))
        return resp.clone().text().then(t=>{const c=applyFilters(url,t);
          if(c===t) return resp;
          const h=new Headers(resp.headers); h.delete('content-length'); h.delete('content-encoding');
          return new Response(c,{status:resp.status,statusText:resp.statusText,headers:h});
        }).catch(()=>resp);
      return resp;
    });
  },'fetch'); }

  if(typeof XMLHttpRequest!=='undefined'){
    const oO=XMLHttpRequest.prototype.open, oS=XMLHttpRequest.prototype.send, meta=new WeakMap();
    XMLHttpRequest.prototype.open=mimic(function open(m,url){
      let u; try{ u=(typeof url==='string')?url:String(url); }catch(_){ u=''; }
      meta.set(this,{url:u,d:interceptURL(u)}); return oO.apply(this,arguments); },'open');
    XMLHttpRequest.prototype.send=mimic(function send(body){
      const m=meta.get(this);
      if(m&&m.d.blocked){ Stats.block.xhr++;
        if(INT.mockResponses){ const mt=m.d.type, isTxt=(mt==='AdConfig'||mt==='AdScript');
          const rb=(mt==='AdConfig')?BODY_ADCONFIG:BODY_ADSCRIPT, jsonFallback=(mt==='AdConfig')?[]:{};
          try{ Object.defineProperties(this,{ readyState:{get(){return 4;},configurable:true}, status:{get(){return 200;},configurable:true},
            statusText:{get(){return 'OK';},configurable:true},
            response:{get(){ if(this.responseType==='json'){ try{return JSON.parse(rb);}catch(_){return jsonFallback;} } return isTxt?rb:FAKE_TS; },configurable:true},
            responseText:{get(){return rb;},configurable:true}, responseURL:{get(){return m.url;},configurable:true} });
            setTimeout(()=>{ try{this.dispatchEvent(new Event('readystatechange'));}catch(_){}
              try{this.dispatchEvent(new Event('load'));}catch(_){} try{this.dispatchEvent(new Event('loadend'));}catch(_){} },INT.mockDelayMs);
            return; }catch(_){}}
        this.abort(); return; }
      if(m&&m.url&&anyFilter(m.url)){
        const rt=Object.getOwnPropertyDescriptor(XMLHttpRequest.prototype,'responseText');
        if(rt){ let pur=null;
          const cleanse=(raw)=>{ if(pur!==null) return pur;
            if(typeof raw==='string'){ pur=applyFilters(m.url,raw); return pur; } return raw; };
          Object.defineProperty(this,'responseText',{get(){ return cleanse(rt.get.call(this)); },configurable:true});
          const rp=Object.getOwnPropertyDescriptor(XMLHttpRequest.prototype,'response');
          if(rp) Object.defineProperty(this,'response',{get(){ return cleanse(rp.get.call(this)); },configurable:true}); } }
      return oS.apply(this,arguments);
    },'send'); }

  if(navigator.sendBeacon){ const o=navigator.sendBeacon;
    navigator.sendBeacon=mimic(function sendBeacon(url,data){ if(interceptURL(url).blocked){ Stats.block.beacon++; return true; } return o.apply(this,arguments); },'sendBeacon'); }

  /* ═══ SECTION 14 · POPUP / AUTOPLAY ═══ */
  if(CFG.blockPopups){ const o=window.open;
    const fake=Object.freeze({ closed:true,focus(){},blur(){},close(){},postMessage(){},print(){},stop(){},
      moveTo(){},moveBy(){},resizeTo(){},resizeBy(){},scroll(){},scrollTo(){},scrollBy(){},
      opener:null,name:'',innerWidth:0,innerHeight:0,outerWidth:0,outerHeight:0,
      location:{href:'',assign(){},replace(){},reload(){},toString(){return '';}},document:null });
    window.open=mimic(function open(url,t,f){ if(url&&interceptURL(url).blocked){ Stats.block.popup++; return fake; } return o.apply(this,arguments); },'open'); }

  if(CFG.blockAutoplay){ const o=HTMLMediaElement.prototype.play;
    HTMLMediaElement.prototype.play=mimic(function play(){ try{ const s=this.currentSrc||this.src;
      if(s&&interceptURL(s).blocked){ Stats.block.autoplay++; return Promise.resolve(); } }catch(_){} return o.apply(this,arguments); },'play'); }

  /* ═══ SECTION 15 · DOM 防护（mal-class 扫描 + insert/write hook）═══ */
  function ws(c){ return c===32||c===9||c===10||c===12||c===13; }
  function isHexLike(cls,start,end){ for(let i=start;i<end;i++){ const c=cls.charCodeAt(i);
    if(!((c>=48&&c<=57)||(c>=97&&c<=102)||(c>=65&&c<=70))) return false; } return true; }
  function scanMalToken(cls,pre,min,max,hexBody){ const pl=pre.length; let idx=cls.indexOf(pre);
    while(idx!==-1){ if(idx===0||ws(cls.charCodeAt(idx-1))){
        let e=idx+pl; while(e<cls.length&&!ws(cls.charCodeAt(e))) e++; const l=e-idx;
        if(l>=min&&l<=max){ if(!hexBody||isHexLike(cls,idx+pl,e)) return true; } }
      idx=cls.indexOf(pre,idx+1); } return false; }
  function isMalClass(cls){ return INT.malClassScan&&!!cls&&typeof cls==='string'&&
    (scanMalToken(cls,'b_',8,8,true)||scanMalToken(cls,'Type',11,13,true)); }

  const sD=Object.getOwnPropertyDescriptor(HTMLScriptElement.prototype,'src');
  if(sD) Object.defineProperty(HTMLScriptElement.prototype,'src',{ get(){return sD.get.call(this);},
    set:mimic(function src(v){ if(interceptURL(v).blocked){ Stats.block.script++; return; } sD.set.call(this,v); },'set src'), configurable:true,enumerable:true });
  const iD=Object.getOwnPropertyDescriptor(HTMLImageElement.prototype,'src');
  if(iD) Object.defineProperty(HTMLImageElement.prototype,'src',{ get(){return iD.get.call(this);},
    set:mimic(function src(v){ if(interceptURL(v).blocked){ Stats.block.img++; return; } iD.set.call(this,v); },'set src'), configurable:true,enumerable:true });

  function isEl(n){ return !!n&&n.nodeType===1; }
  function nodeBlocked(node){ try{
    if(!isEl(node)) return false;
    if(isMalClass(node.className)) return true;
    const s=node.src||node.href; if(s&&interceptURL(s).blocked) return true;
  }catch(_){} return false; }

  function hookInsert(proto,method){ const o=proto[method]; if(!o) return;
    proto[method]=mimic(function(...args){
      if(method==='appendChild'||method==='insertBefore'||method==='replaceChild'){
        const child=args[0],ref=args[1];
        if(isEl(child)&&nodeBlocked(child)){ Stats.block.domInsert++; return method==='replaceChild'?ref:child; }
        return o.apply(this,args); }
      const out=[]; let hit=false;
      for(const a of args){ if(isEl(a)&&nodeBlocked(a)){ hit=true; continue; } out.push(a); }
      if(hit) Stats.block.domInsert++;
      return o.apply(this,out);
    },method); }

  if(INT.domInsertBlock){
    for(const m of ['appendChild','insertBefore','replaceChild']) hookInsert(Node.prototype,m);
    for(const m of ['append','prepend','after','before','replaceWith']){
      hookInsert(Element.prototype,m);
      if(typeof DocumentFragment!=='undefined') hookInsert(DocumentFragment.prototype,m); }
    if(Element.prototype.insertAdjacentElement){ const o=Element.prototype.insertAdjacentElement;
      Element.prototype.insertAdjacentElement=mimic(function insertAdjacentElement(pos,el){
        if(isEl(el)&&nodeBlocked(el)){ Stats.block.domInsert++; return el; } return o.apply(this,arguments); },'insertAdjacentElement'); }
  }

  if(INT.domWriteBlock){
    const writeBlocked=(str)=>{ if(typeof str!=='string'||!str) return false;
      if(str.indexOf('<script')===-1&&str.indexOf('<iframe')===-1) return false;
      return hasAny(str,AD_KW)||str.indexOf('/000/flink')!==-1; };
    for(const method of ['write','writeln']){ const o=document[method]; if(!o) continue;
      document[method]=mimic(function(...args){ for(const s of args) if(writeBlocked(s)){ Stats.block.domInsert++; return; }
        return o.apply(this,args); },method); }
  }

  /* ═══ SECTION 16 · COOKIE + LOCK PATROL ═══ */
  const CK=new Set(['jump_visit_count','__ad_visited']);
  const LK=['LOCK_FIXED_','SYS_REQ_','CSS_uc','LOCK_JUMP_'];
  function isLockKey(k){ if(typeof k!=='string') return false; for(const pf of LK) if(k.indexOf(pf)===0) return true; return false; }
  function patrolCookie(){ try{ for(const p of document.cookie.split(';')){ const n=p.trim().split('=')[0];
      if(CK.has(n)){ const exp='=; expires=Thu, 01 Jan 1970 00:00:00 UTC; path=/;';
        document.cookie=n+exp; try{ document.cookie=n+exp+' domain=.'+location.hostname+';'; }catch(_){}
        Stats.block.cookie++; } } }catch(_){} }
  function patrolLocks(){ try{ for(const k in window){ if(isLockKey(k)){ try{ delete window[k]; }catch(_){} } } }catch(_){} }
  if(CFG.sanitizeCookies){
    patrolCookie(); patrolLocks();
    setInterval(patrolCookie,INT.cookiePollMs);
    setInterval(patrolLocks,INT.cookiePollMs*INT.lockPollMul); }

  /* ═══ SECTION 17 · MUTATION OBSERVER ═══ */
  const pending=[]; let scheduled=false;
  const schedule=(typeof queueMicrotask==='function')?(fn)=>queueMicrotask(fn):(fn)=>Promise.resolve().then(fn);
  function flush(){ scheduled=false; if(!pending.length) return; const batch=pending.splice(0); let removed=0;
    for(const n of batch){ if(!n.isConnected) continue; try{
      if(n.tagName==='SCRIPT'&&n.src&&interceptURL(n.src).blocked){ n.remove(); removed++; continue; }
      if(isMalClass(n.className)){ n.remove(); removed++; continue; }
      if(n.tagName==='IMG'&&n.src&&interceptURL(n.src).blocked){ n.remove(); removed++; } }catch(_){} }
    if(removed) Stats.block.domRemoved+=removed; }
  const obs=new MutationObserver(muts=>{ for(const mu of muts) for(const n of mu.addedNodes) if(isEl(n)){
      if(pending.length>=INT.pendingMax){ Log.warn('DOM','突变队列超限，切换同步刷新'); flush(); }
      pending.push(n); }
    if(!scheduled&&pending.length){ scheduled=true; schedule(flush); } });
  const startObs=()=>{ obs.observe(document.documentElement,{childList:true,subtree:true}); Log.debug('Init','DOM 清道夫启动'); };
  document.documentElement?startObs():addEventListener('DOMContentLoaded',startObs,{once:true});

  /* ═══ SECTION 18 · 播放器模块（诊断 + DPlayer 桥接 + Hls 兼容 + Blob 诊断）═══ */
  const IS_PLAYER=/\/static\/player\//i.test(location.pathname);
  if(INT.playerDiag&&IS_PLAYER) Log.info('Player','当前处于播放器上下文',{url:location.href});

  if(INT.playerDiag){
    const traceIframes=()=>{
      const list=document.querySelectorAll?document.querySelectorAll('iframe'):[];
      if(!list.length) return;
      const tree=[...list].map((f,i)=>{ let inner='(未加载)';
        try{ inner=f.contentWindow&&f.contentWindow.location.href; }catch(_){ inner='(跨域)'; }
        return {i,src:f.src,inner}; });
      Log.info('Player','iframe 树',tree); };
    document.readyState==='loading'
      ?addEventListener('DOMContentLoaded',traceIframes,{once:true}):traceIframes();
    let pdTries=0;
    const pdTimer=setInterval(()=>{
      pdTries++;
      if(window.player_data){ clearInterval(pdTimer); Stats.diag.playerData++;
        Log.ok('Player','捕获 player_data',{...window.player_data}); }
      else if(pdTries>=INT.playerDataPollMax){ clearInterval(pdTimer); }
    },INT.playerDataPollMs); }

  /* ─── DPlayer 构造前 M3U8 桥接（强制 hls.js customType）─── */
  function wrapDPlayerClass(Orig){
    if(typeof Orig!=='function'||Orig.__bc) return Orig;
    Orig.__bc=true;

    const Wrapped=mimic(function DPlayer(opt){
      const o=opt||{};
      try{ const vi=o&&o.video;
        Log.info('DPlayer','创建播放器实例',vi?{url:vi.url,type:vi.type,pic:vi.pic,live:vi.live}:o); }catch(_){}

      const canBridge=CFG.mediaM3u8Bridge&&INT.dplayerPreBridge&&
        o&&o.video&&typeof o.video.url==='string'&&M3U8_RE.test(o.video.url)&&
        (o.video.type==='hls'||!o.video.type);

      if(canBridge){
        const original=o.video.url;
        Log.info('Bridge','DPlayer 构造前桥接开始',original);
        try{ const c=(typeof o.container==='string')?document.querySelector(o.container):o.container;
          if(c&&c.innerHTML!==undefined) c.innerHTML=''; }catch(_){}

        const holder={};
        bridgeM3U8(original,0).then(blob=>{
          const hlsCtor=window.Hls;
          const next=Object.assign({},o,{
            video:Object.assign({},o.video,{
              url:blob, type:'customHls',
              customType:Object.assign({},o.video&&o.video.customType,{
                customHls:function(video){
                  if(!hlsCtor){ Log.warn('Bridge','Hls 不可用，回退直接赋值'); video.src=blob; return; }
                  try{ const h=new hlsCtor(); h.loadSource(blob); h.attachMedia(video);
                    Log.ok('Bridge','hls.js 已接管 blob playlist');
                  }catch(e){ Log.error('Bridge','hls.js 挂载失败: '+e.message); video.src=blob; }
                },
              }),
            }),
          });
          try{ const ins=new Orig(next); Stats.bridge.dplayerBridge++;
            Log.ok('Bridge','DPlayer 构造前桥接成功',{from:original,to:blob});
            Object.assign(holder,ins);
            try{ Object.setPrototypeOf(holder,Object.getPrototypeOf(ins)); }catch(_){}
          }catch(e){ Log.error('Bridge','DPlayer 构造(净化后)失败: '+e.message);
            try{ const ins2=new Orig(o); Object.assign(holder,ins2);
              try{ Object.setPrototypeOf(holder,Object.getPrototypeOf(ins2)); }catch(_){} }catch(_){} }
        }).catch(e=>{
          Log.warn('Bridge','DPlayer 构造前桥接失败，回退原始地址: '+e.message,original);
          try{ const ins3=new Orig(o); Object.assign(holder,ins3);
            try{ Object.setPrototypeOf(holder,Object.getPrototypeOf(ins3)); }catch(_){} }catch(_){}
        });
        return holder;
      }

      return new Orig(o);
    },'DPlayer');

    Wrapped.prototype=Orig.prototype;
    for(const k in Orig) try{ Wrapped[k]=Orig[k]; }catch(_){}
    try{ Object.defineProperty(Wrapped,'__bc',{value:true,configurable:true}); }catch(_){}
    Log.ok('Player','捕获 DPlayer 构造函数');
    return Wrapped;
  }

  if(INT.hookDPlayer){
    let DP=(typeof window.DPlayer==='function'&&!window.DPlayer.__bc)?wrapDPlayerClass(window.DPlayer):window.DPlayer;
    Object.defineProperty(window,'DPlayer',{
      get(){ return DP; },
      set(v){ DP=(typeof v==='function'&&!v.__bc)?wrapDPlayerClass(v):v; },
      configurable:true }); }

  /* ─── Hls Loader 包装（兼容路径：非 DPlayer 场景下 hls.js 直接加载远程 m3u8 时净化）─── */
  function wrapHlsLoader(LoaderClass){
    if(!LoaderClass||LoaderClass.__bcLoader) return LoaderClass;
    const Wrapped=function(config){
      const inst=new LoaderClass(config);
      if(!inst||typeof inst.load!=='function') return inst;
      const oLoad=inst.load.bind(inst);
      inst.load=function(context,loaderConfig,callbacks){
        const oSuccess=callbacks&&callbacks.onSuccess;
        if(!oSuccess) return oLoad(context,loaderConfig,callbacks);
        const wrapped=Object.assign({},callbacks,{
          onSuccess(response,stats,ctx,networkDetails){
            try{ if(response&&typeof response.data==='string'&&ctx&&ctx.url&&anyFilter(ctx.url)){
                const cleaned=applyFilters(ctx.url,response.data);
                if(cleaned!==response.data){ Stats.bridge.hlsFiltered++; response.data=cleaned; } }
            }catch(_){}
            return oSuccess(response,stats,ctx,networkDetails); },
        });
        return oLoad(context,loaderConfig,wrapped); };
      return inst; };
    Wrapped.__bcLoader=true;
    return Wrapped; }

  function wrapHlsClass(OriginalHls){
    if(!OriginalHls||OriginalHls.__bc) return OriginalHls;
    OriginalHls.__bc=true;
    try{ const ls=OriginalHls.prototype&&OriginalHls.prototype.loadSource;
      if(ls) OriginalHls.prototype.loadSource=mimic(function loadSource(src){
        Log.info('Hls','loadSource',src); return ls.apply(this,arguments); },'loadSource'); }catch(e){ Log.error('Hls',e.message); }
    return new Proxy(OriginalHls,{
      construct(target,args){
        const userCfg=args[0]||{}, merged=Object.assign({},userCfg);
        const baseLoader=merged.loader||(target.DefaultConfig&&target.DefaultConfig.loader);
        if(baseLoader) merged.loader=wrapHlsLoader(baseLoader);
        Log.ok('Player','捕获 Hls.js 实例并注入过滤 Loader');
        return Reflect.construct(target,[merged]); },
    }); }

  if(INT.hookHls){
    let H=(typeof window.Hls==='function'&&!window.Hls.__bc)?wrapHlsClass(window.Hls):window.Hls;
    Object.defineProperty(window,'Hls',{
      get(){ return H; },
      set(v){ H=(typeof v==='function'&&!v.__bc)?wrapHlsClass(v):v; },
      configurable:true }); }

  /* ─── Blob/MediaSource 诊断（默认关闭，调试时开启 CFG.hookBlobMedia）─── */
  if(CFG.hookBlobMedia){
    const oCOU=URL.createObjectURL;
    URL.createObjectURL=mimic(function createObjectURL(obj){
      Stats.diag.blobCreated++;
      try{ const isMS=(typeof MediaSource!=='undefined')&&obj instanceof MediaSource;
        Log.debug('Blob','createObjectURL',isMS?'MediaSource':(obj&&obj.constructor&&obj.constructor.name)); }catch(_){}
      return oCOU.apply(this,arguments); },'createObjectURL');
    if(typeof MediaSource!=='undefined'){
      const oASB=MediaSource.prototype.addSourceBuffer;
      MediaSource.prototype.addSourceBuffer=mimic(function addSourceBuffer(mime){
        Log.info('MediaSource','addSourceBuffer',mime);
        const sb=oASB.apply(this,arguments), oAB=sb.appendBuffer;
        sb.appendBuffer=mimic(function appendBuffer(chunk){ Stats.diag.sbChunk++; return oAB.apply(this,arguments); },'appendBuffer');
        return sb; },'addSourceBuffer'); }
  }

  /* ═══ SECTION 19 · DIAGNOSTIC API ═══ */
  const API={ stats:()=>JSON.parse(JSON.stringify(Stats)), cache:()=>({size:cache.size,cap:INT.lruSize}), decide, report, version:VERSION, rules:()=>RULES.length, isPlayer:()=>IS_PLAYER, bridgeM3U8:(url)=>bridgeM3U8(url,0) };
  if(CFG.exposeGlobal){ try{ Object.defineProperty(window,SYM,{value:API,enumerable:false,configurable:true,writable:false}); }catch(_){} }
  Log.ok('BOOT','◆ Blockcat v'+VERSION);
})();
