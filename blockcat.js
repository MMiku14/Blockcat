// ==UserScript==
// @name         ◆ Blockcat /拦猫
// @namespace    http://tampermonkey.net/
// @version      2.0.0
// @description  反向域名 HostTrie · 冻结判决驻留 · 位掩码策略 · 单遍多关键词扫描 · 统一 DOM 清道夫 · 微任务批处理
// @author       cat & Blockcat-Optimizer
// @match        *://*/*
// @run-at       document-start
// @grant        none
// ==/UserScript==


(function () {
  'use strict';

  const VERSION = '2.0.0';

  /* ═══ SECTION 1 · CONFIG ═══ */
  const CFG = {
    log: true, logLevel: 'INFO',               // 日志总开关与最低输出级别：DEBUG/INFO/WARN/ERROR/SUCCESS
    mockResponses: true,                        // 拦截命中后返回伪造响应，而非直接空响应/中断请求
    m3u8Cleanse: true,                          // 净化 m3u8 播放列表，剥离广告分片
    antiDetect: true,                           // 伪装 Function.prototype.toString，隐藏 hook 痕迹
    blockPopups: true,                          // 拦截 window.open 弹窗
    blockAutoplay: true,                        // 拦截命中黑名单资源的媒体自动播放
    sanitizeCookies: true,                      // 巡查清理广告相关 Cookie 与全局锁变量
    hlsHijacker: true,                          // 监听 Hls.js 实例挂载（仅记录日志，不改变行为）
    strictNonStdPort: false,                    // 严格模式：拦截所有第三方非标端口（可能误杀正常业务）
    cheapTldBlock: false,                       // 拦截廉价高风险 TLD 域名
    blockPunycode: true,                        // 拦截 Punycode 混淆域名（xn--）
    blockCloud: false,                          // 拦截云服务商临时域名（AWS/Azure 等）
    domInsertBlock: true,                       // 拦截恶意节点的 DOM 插入（appendChild/append/before 等）
    domWriteBlock: true,                        // 拦截 document.write/writeln 注入的恶意片段
    malClassScan: true,                         // 扫描混淆广告 class 名（b_xxxxxx / TypeXXX 样式）
    lruSize: 800,                               // decide() 判决结果 LRU 缓存容量
    cookiePollMs: 4000,                         // Cookie 巡查间隔（毫秒）
    lockPollMul: 4,                             // 全局锁变量巡查间隔倍数（实际间隔 = cookiePollMs * lockPollMul）
    pendingMax: 5000,                           // MutationObserver 突变队列上限，超限立即同步刷新防 OOM
    mockDelayMs: 1,                             // XHR 伪造响应触发 load 事件的延迟（毫秒）
    m3u8SafetyRatio: 0.5,                       // m3u8 广告分片占比超过该阈值时放弃净化，防止误杀
    autoReport: false,                          // 页面卸载/隐藏时是否自动输出统计报告；默认关闭，改用 API.report()/API.stats() 手动查看
    exposeGlobal: false,                        // 是否将诊断 API 挂载到 window（以隐藏 Symbol 键存放）
  };
  // 合法 TS 包 = 188 字节，避免播放器错误重试暴露拦截
  const FAKE_TS = (() => { const b = new Uint8Array(188); b[0]=0x47; b[1]=0x1F; b[2]=0xFF; b[3]=0x10; return b.buffer; })();
  const BODY_ADCONFIG = '[]';
  const BODY_ADSCRIPT = '/* bc */';

  /* ═══ SECTION 2 · LOG (lazy, leveled) ═══ */
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

  /* ═══ SECTION 3 · STATS ═══ */
  const Stats = { fetch:0, xhr:0, beacon:0, popup:0, script:0, img:0, domInsert:0, domRemoved:0, m3u8:0, cookie:0, autoplay:0, cacheHit:0 };
  // 手动接口：随时调用查看当前累计拦截统计，不再依赖页面关闭时的一次性弹出
  const report = () => Log.info('统计报告', '拦截汇总', { ...Stats });
  if (CFG.autoReport) {
    let done = false;
    const auto = () => { if (done) return; done = true; report(); };
    addEventListener('beforeunload', auto);
    addEventListener('pagehide', auto);
    document.addEventListener('visibilitychange', () => { if (document.visibilityState === 'hidden') auto(); });
  }

  /* ═══ SECTION 4 · ANTI-DETECT (native-code mimic) ═══ */
  const SYM = Symbol('__bc__');
  let mimic = fn => fn;
  if (CFG.antiDetect) try {
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

  /* ═══ SECTION 5 · LRU O(1) ═══ */
  class LRU {
    constructor(cap) { this.cap = cap; this.m = new Map(); }
    get(k){ const v=this.m.get(k); if(v===undefined) return undefined; this.m.delete(k); this.m.set(k,v); Stats.cacheHit++; return v; }
    set(k,v){ if(this.m.has(k)) this.m.delete(k); else if(this.m.size>=this.cap) this.m.delete(this.m.keys().next().value); this.m.set(k,v); }
    get size(){ return this.m.size; }
  }
  const cache = new LRU(CFG.lruSize);

  /* ═══ SECTION 6 · FROZEN VERDICTS (interned, zero-alloc on hit) ═══ */
  const ALLOW = Object.freeze({ blocked:false, type:'', reason:'' });
  const V = (type, reason) => Object.freeze({ blocked:true, type, reason });
  const V_ADSCRIPT = V('AdScript','黑产广告脚本');
  const V_ADCONFIG = V('AdConfig','广告中心化配置');
  const V_PUNYCODE = V('Punycode','Punycode 混淆域名');
  const V_CHEAPTLD = V('CheapTLD','廉价TLD');

  /* ═══ SECTION 7 · PathTrie (single + terminal wildcard) ═══ */
  class PathTrie {
    constructor(){ this.root={c:{},end:false,meta:null,wild:false}; }
    add(p, meta){ const s=p.replace(/\/+$/,'').split('/').filter(Boolean); let n=this.root;
      for(const g of s){ if(g==='*'){ n.wild=true; n.meta=meta; n.end=true; return; } (n.c[g]=n.c[g]||{c:{},end:false,meta:null,wild:false}); n=n.c[g]; }
      n.end=true; n.meta=meta; }
    match(p){ const s=p.replace(/\/+$/,'').split('/').filter(Boolean); let n=this.root;
      for(const g of s){ if(n.wild) return n.meta; const x=n.c[g]; if(!x) return null; n=x; } return n.end?n.meta:null; }
  }
  const PT = new PathTrie();
  PT.add('/000/flink/click.php', V('Tracker','流量追踪-点击'));
  PT.add('/000/flink/analytics.php', V('Tracker','流量追踪-统计'));
  PT.add('/000/flink/url.php', V('Tracker','流量追踪-跳转'));
  PT.add('/000/flink/check_domain.php', V('Tracker','流量追踪-域名探测'));
  PT.add('/000/report_error_video/*', V('Tracker','视频错误上报'));
  PT.add('/ajax/hits', V('Tracker','点击率更新'));

  /* ═══ SECTION 8 · HostTrie (reversed labels, O(labels) suffix match) ═══ */
  class HostTrie {
    constructor(){ this.root={c:{},end:false,meta:null}; }
    add(h, meta){ const l=h.split('.').filter(Boolean).reverse(); let n=this.root;
      for(const x of l){ (n.c[x]=n.c[x]||{c:{},end:false,meta:null}); n=n.c[x]; } n.end=true; n.meta=meta; }
    match(h){ const l=h.split('.').filter(Boolean).reverse(); let n=this.root;
      for(const x of l){ const nx=n.c[x]; if(!nx) return null; n=nx; if(n.end) return n.meta; } return n.end?n.meta:null; }
  }
  const HT = new HostTrie();
  // 云域名规则默认不生效（CFG.blockCloud=false），保留规则供需要时开启
  HT.add('amazonaws.com', V('CloudInfra','AWS 临时域名'));
  HT.add('cloudapp.azure.com', V('CloudInfra','Azure 临时域名'));

  /* ═══ SECTION 9 · single-pass multi-keyword scan ═══ */
  const AD_KW = ['/fixed_ui_','/fixed_jump_'];
  function hasAny(s, arr){ for(let i=0;i<arr.length;i++) if(s.indexOf(arr[i])!==-1) return true; return false; }
  const CHEAP = new Set(['skin','casa','cfd','buzz','fit','boats','pics','homes','beer','autos','sbs','xyz']);
  function isCheapTLD(host){ const d=host.lastIndexOf('.'); return d!==-1 && CHEAP.has(host.slice(d+1)); }
  function isNonStdPort(port){ return !!port && port!=='80' && port!=='443'; }

  // 支持 IPv4 与 IPv6（带方括号或裸写）
  function isIP(h){
    if(!h) return false;
    if(h[0]==='['){ return h.indexOf(':')!==-1; }
    if(h.indexOf(':')!==-1) return true;
    let d=0,s=0,on=false;
    for(let i=0;i<h.length;i++){ const c=h.charCodeAt(i);
      if(c===46){ if(!on||s>255) return false; d++; s=0; on=false; } else if(c>=48&&c<=57){ s=s*10+(c-48); on=true; } else return false; }
    return d===3&&on&&s<=255;
  }
  function parseURL(u, base){ try{ return new URL(u, base||location.origin); }catch(_){ return null; } }

  /* ═══ SECTION 10 · DECISION PIPELINE (short-circuit) ═══ */
  const SELF = location.hostname.toLowerCase();
  function decide(raw){
    if(!raw) return ALLOW;
    if(typeof raw!=='string'){ try{ raw=String(raw); }catch(_){ return ALLOW; } if(!raw) return ALLOW; }
    const c = cache.get(raw); if(c!==undefined) return c;

    const u = parseURL(raw);
    if(!u){ const r = hasAny(raw, AD_KW) ? V_ADSCRIPT : ALLOW; cache.set(raw, r); return r; }

    const path=u.pathname, host=u.hostname.toLowerCase(), port=u.port;
    let r = ALLOW;

    // PathTrie
    const pm = PT.match(path);
    if(pm) r = pm;
    // 站点特定 /abc/ 逻辑仅在同源时生效，避免污染无关站点
    if(!r.blocked && host===SELF && path.indexOf('/abc/')!==-1){
      if(path.endsWith('.js') && hasAny(path, AD_KW)) r = V_ADSCRIPT;
      else if(path.endsWith('.json') && path.indexOf('/data_')!==-1) r = V_ADCONFIG;
    }
    // HostTrie
    if(!r.blocked && CFG.blockCloud){ const hm = HT.match(host); if(hm) r = hm; }
    // 非标端口
    if(!r.blocked && isNonStdPort(port)){
      if(host!=='localhost' && host!=='127.0.0.1' && host!==SELF){
        if(CFG.strictNonStdPort) r = V('BadPort','第三方非标端口 :'+port);
        else if(isIP(host)) r = V('BadPortIP','IP+非标端口 '+host+':'+port);
      }
    }
    // 廉价 TLD
    if(!r.blocked && CFG.cheapTldBlock && isCheapTLD(host)) r = V_CHEAPTLD;
    // Punycode
    if(!r.blocked && CFG.blockPunycode && host.indexOf('xn--')!==-1) r = V_PUNYCODE;

    if(r.blocked) Log.warn('Policy','['+r.type+'] '+r.reason+' → '+raw.slice(0,120));
    cache.set(raw, r);
    return r;
  }

  /* ═══ SECTION 11 · MOCK factory ═══ */
  function mock(type){
    if(type==='AdConfig') return new Response(BODY_ADCONFIG,{status:200,headers:{'Content-Type':'application/json'}});
    if(type==='AdScript') return new Response(BODY_ADSCRIPT,{status:200,headers:{'Content-Type':'application/javascript'}});
    return new Response(FAKE_TS,{status:200,headers:{'Content-Type':'video/MP2T'}});
  }

  /* ═══ SECTION 12 · M3U8 cleanser (O(N) index + cluster + safety valve) ═══ */
  const M3U8_RE = /\.m3u8(?:$|[?#])/i;
  const M3U8 = { clean(text, src){
    if(!text || text.indexOf('#EXTM3U')===-1) return text;
    const lines = text.split(/\r?\n/), segs = [];
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
      let bad = sd!==main || ul.indexOf('ad_')!==-1 || ul.indexOf('creative')!==-1 || ul.indexOf('fixed_')!==-1 || ul.indexOf('flink')!==-1;
      // 相对路径以 src 为 base 解析，避免静默漏判
      if(!bad){ const uu=parseURL(s.url, src); if(uu){ const h=uu.hostname.toLowerCase();
        if(CFG.cheapTldBlock && isCheapTLD(h)) bad=true;
        if(isNonStdPort(uu.port) && isIP(h)) bad=true; } }
      if(bad) ad.add(k); }
    if(ad.size > segs.length*CFG.m3u8SafetyRatio){ Log.warn('M3U8','广告比率过高，触发防误杀'); return text; }
    const adLines=new Set();
    for(const idx of ad){ const sl=segs[idx].line; adLines.add(sl);
      for(let r=sl-1;r>=0;r--){ const t=lines[r].trim();
        if(t.indexOf('#EXTINF')===0||t==='#EXT-X-DISCONTINUITY'||t.indexOf('#EXT-X-BYTERANGE')===0) adLines.add(r); else break; } }
    const out=[]; let pd=false;
    for(let i=0;i<lines.length;i++){ if(adLines.has(i)) continue; const t=lines[i].trim();
      if(t==='#EXT-X-DISCONTINUITY'){ if(pd) continue; pd=true; } else if(t) pd=false; out.push(lines[i]); }
    if(ad.size>0){ Stats.m3u8+=ad.size; Log.ok('M3U8','净化 剥除'+ad.size+'段/共'+segs.length); return out.join('\n'); }
    return text;
  }};

  /* ═══ SECTION 13 · FETCH ═══ */
  if(window.fetch){ const o=window.fetch; window.fetch = mimic(function fetch(res, init){
    const url = typeof res==='string' ? res : (res instanceof Request ? res.url : (res && typeof res.href==='string' ? res.href : ''));
    const d = decide(url);
    if(d.blocked){ Stats.fetch++; return Promise.resolve(CFG.mockResponses ? mock(d.type) : new Response(null,{status:200})); }
    // 保留原始参数（含 Request 对象的 body/headers），不丢弃上下文
    return o.apply(this, arguments).then(resp => {
      if(CFG.m3u8Cleanse && M3U8_RE.test(url) && resp.ok)
          return resp.clone().text().then(t => { const c=M3U8.clean(t,url);
          if(c===t) return resp;
          const h=new Headers(resp.headers); h.delete('content-length'); h.delete('content-encoding');
          return new Response(c,{status:resp.status,statusText:resp.statusText,headers:h});
        }).catch(()=>resp);
      return resp;
    });
  }, 'fetch'); }

  /* ═══ SECTION 14 · XHR ═══ */
  if(typeof XMLHttpRequest!=='undefined'){
    const oO=XMLHttpRequest.prototype.open, oS=XMLHttpRequest.prototype.send, meta=new WeakMap();
    XMLHttpRequest.prototype.open = mimic(function open(m, url){
      let u; try{ u = (typeof url==='string') ? url : String(url); }catch(_){ u=''; }
      meta.set(this,{url:u,d:decide(u)}); return oO.apply(this,arguments); },'open');
    XMLHttpRequest.prototype.send = mimic(function send(body){
      const m=meta.get(this);
      if(m && m.d.blocked){ Stats.xhr++;
        if(CFG.mockResponses){ const mt=m.d.type, isTxt=(mt==='AdConfig'||mt==='AdScript');
          // JSON 回退：AdScript 类型返回空对象而非注释，避免 JSON.parse 抛错
          const rb=(mt==='AdConfig')?BODY_ADCONFIG:BODY_ADSCRIPT;
          const jsonFallback=(mt==='AdConfig')?[]:{};
          try{ Object.defineProperties(this,{ readyState:{get(){return 4;},configurable:true}, status:{get(){return 200;},configurable:true},
            statusText:{get(){return 'OK';},configurable:true},
            response:{get(){ if(this.responseType==='json'){ try{return JSON.parse(rb);}catch(_){return jsonFallback;} } return isTxt?rb:FAKE_TS; },configurable:true},
            responseText:{get(){return rb;},configurable:true}, responseURL:{get(){return m.url;},configurable:true} });
            setTimeout(()=>{ try{this.dispatchEvent(new Event('readystatechange'));}catch(_){}
              try{this.dispatchEvent(new Event('load'));}catch(_){} try{this.dispatchEvent(new Event('loadend'));}catch(_){} },CFG.mockDelayMs);
            return; }catch(_){}}
        this.abort(); return; }
      if(CFG.m3u8Cleanse && m && m.url && M3U8_RE.test(m.url)){
        const rt=Object.getOwnPropertyDescriptor(XMLHttpRequest.prototype,'responseText');
        if(rt){ let pur=null;
          const cleanse=(raw)=>{ if(pur!==null) return pur;
            if(typeof raw==='string' && raw.indexOf('#EXTM3U')!==-1){ pur=M3U8.clean(raw,m.url); return pur; } return raw; };
          Object.defineProperty(this,'responseText',{ get(){ return cleanse(rt.get.call(this)); }, configurable:true });
          const rp=Object.getOwnPropertyDescriptor(XMLHttpRequest.prototype,'response');
          if(rp) Object.defineProperty(this,'response',{ get(){ return cleanse(rp.get.call(this)); }, configurable:true }); } }
      return oS.apply(this, arguments);
    },'send');
  }

  /* ═══ SECTION 15 · BEACON / OPEN / HLS / AUTOPLAY ═══ */
  if(navigator.sendBeacon){ const o=navigator.sendBeacon;
    navigator.sendBeacon = mimic(function sendBeacon(url,data){ if(decide(url).blocked){ Stats.beacon++; return true; } return o.apply(this,arguments); },'sendBeacon'); }

  // fake window 对象补全常用属性/方法，避免访问时 TypeError
  if(CFG.blockPopups){ const o=window.open;
    const fake=Object.freeze({
      closed:true, focus(){}, blur(){}, close(){}, postMessage(){}, print(){}, stop(){},
      moveTo(){}, moveBy(){}, resizeTo(){}, resizeBy(){}, scroll(){}, scrollTo(){}, scrollBy(){},
      opener:null, name:'', innerWidth:0, innerHeight:0, outerWidth:0, outerHeight:0,
      location:{ href:'', assign(){}, replace(){}, reload(){}, toString(){return '';} }, document:null,
    });
    window.open = mimic(function open(url,t,f){ if(url && decide(url).blocked){ Stats.popup++; return fake; } return o.apply(this,arguments); },'open'); }

  if(CFG.hlsHijacker){ let H=window.Hls; Object.defineProperty(window,'Hls',{ get(){return H;}, set(v){ H=v;
    if(typeof H==='function' && !H.__bc){ H.__bc=true; try{ const ls=H.prototype.loadSource;
      H.prototype.loadSource=function(src){ Log.info('Hls','流加载: '+src); return ls.apply(this,arguments); };
      Log.ok('Hls','捕获 Hls.js 实例'); }catch(e){ Log.error('Hls',e.message); } } }, configurable:true }); }

  if(CFG.blockAutoplay){ const o=HTMLMediaElement.prototype.play;
    HTMLMediaElement.prototype.play = mimic(function play(){ try{ const s=this.currentSrc||this.src;
      if(s && decide(s).blocked){ Stats.autoplay++; return Promise.resolve(); } }catch(_){} return o.apply(this,arguments); },'play'); }

  /* ═══ SECTION 16 · SHARED malicious-class scanner (unified, used everywhere) ═══ */
  function ws(c){ return c===32||c===9||c===10||c===12||c===13; }
  // 要求 b_ 后紧跟 6 位十六进制/数字，Type 前缀要求全大写混淆样式，避免误杀 b_header / TypeSelector 等合法 class
  function isHexLike(cls, start, end){ for(let i=start;i<end;i++){ const c=cls.charCodeAt(i);
    const ok=(c>=48&&c<=57)||(c>=97&&c<=102)||(c>=65&&c<=70); if(!ok) return false; } return true; }
  function scanMalToken(cls, pre, min, max, hexBody){ const pl=pre.length; let idx=cls.indexOf(pre);
    while(idx!==-1){
      if(idx===0 || ws(cls.charCodeAt(idx-1))){
        let e=idx+pl; while(e<cls.length && !ws(cls.charCodeAt(e))) e++; const l=e-idx;
        if(l>=min && l<=max){ if(!hexBody || isHexLike(cls, idx+pl, e)) return true; } }
      idx=cls.indexOf(pre, idx+1); } return false; }
  function isMalClass(cls){ return CFG.malClassScan && !!cls && typeof cls==='string' &&
    (scanMalToken(cls,'b_',8,8,true) || scanMalToken(cls,'Type',11,13,true)); }

  /* ═══ SECTION 17 · DOM property proxies (src setters + insert hooks) ═══ */
  const sD=Object.getOwnPropertyDescriptor(HTMLScriptElement.prototype,'src');
  if(sD) Object.defineProperty(HTMLScriptElement.prototype,'src',{ get(){return sD.get.call(this);},
    set: mimic(function src(v){ if(decide(v).blocked){ Stats.script++; return; } sD.set.call(this,v); },'set src'), configurable:true, enumerable:true });
  const iD=Object.getOwnPropertyDescriptor(HTMLImageElement.prototype,'src');
  if(iD) Object.defineProperty(HTMLImageElement.prototype,'src',{ get(){return iD.get.call(this);},
    set: mimic(function src(v){ if(decide(v).blocked){ Stats.img++; return; } iD.set.call(this,v); },'set src'), configurable:true, enumerable:true });

  function isEl(n){ return !!n && n.nodeType===1; }

  function nodeBlocked(node){ try{
    if(!isEl(node)) return false;
    if(isMalClass(node.className)) return true;
    const s=node.src||node.href; if(s && decide(s).blocked) return true;
  }catch(_){} return false; }

  if(CFG.domInsertBlock){
    // replaceChild 返回被替换的旧节点，其余方法返回新节点或 undefined
    for(const method of ['appendChild','insertBefore','replaceChild']){ const o=Node.prototype[method]; if(!o) continue;
      Node.prototype[method]= mimic(function(child, ref){ if(isEl(child) && nodeBlocked(child)){
          Stats.domInsert++; return method==='replaceChild' ? ref : child; }
        return o.apply(this, arguments); }, method); }

    const filterNodes = (args) => { let hit=false; const out=[];
      for(const a of args){ if(isEl(a) && nodeBlocked(a)){ hit=true; continue; } out.push(a); }
      return { out, hit }; };
    for(const method of ['append','prepend','after','before','replaceWith']){
      for(const proto of [Element.prototype, (typeof DocumentFragment!=='undefined'?DocumentFragment.prototype:null)]){
        if(!proto || !proto[method]) continue; const o=proto[method];
        proto[method]= mimic(function(...args){ const { out, hit }=filterNodes(args);
          if(hit) Stats.domInsert++; return o.apply(this, out); }, method); } }
    if(Element.prototype.insertAdjacentElement){ const o=Element.prototype.insertAdjacentElement;
      Element.prototype.insertAdjacentElement= mimic(function insertAdjacentElement(pos, el){
        if(isEl(el) && nodeBlocked(el)){ Stats.domInsert++; return el; } return o.apply(this, arguments); },'insertAdjacentElement'); }
  }

  if(CFG.domWriteBlock){
    // 仅在片段含 script/iframe 且命中广告关键词时拦截，避免误伤正常 write
    const writeBlocked = (str) => { if(typeof str!=='string' || !str) return false;
      if(str.indexOf('<script')===-1 && str.indexOf('<iframe')===-1) return false;
      return hasAny(str, AD_KW) || str.indexOf('/000/flink')!==-1; };
    for(const method of ['write','writeln']){ const o=document[method]; if(!o) continue;
      document[method]= mimic(function(...args){ for(const s of args) if(writeBlocked(s)){ Stats.domInsert++; return; }
        return o.apply(this, args); }, method); }
  }

  /* ═══ SECTION 18 · COOKIE + GLOBAL-LOCK patrol ═══ */
  const CK=new Set(['jump_visit_count','__ad_visited']);
  const LK=['LOCK_FIXED_','SYS_REQ_','CSS_uc','LOCK_JUMP_'];
  function isLockKey(k){ if(typeof k!=='string') return false; for(const pf of LK) if(k.indexOf(pf)===0) return true; return false; }

  function patrolCookie(){ try{ for(const p of document.cookie.split(';')){ const n=p.trim().split('=')[0];
      if(CK.has(n)){ const exp='=; expires=Thu, 01 Jan 1970 00:00:00 UTC; path=/;';
        document.cookie=n+exp;
        try{ document.cookie=n+exp+' domain=.'+location.hostname+';'; }catch(_){}
        Stats.cookie++; } } }catch(_){} }

  // 低频巡查（cookiePollMs * lockPollMul），用 for...in 只枚举可枚举属性，避免高频全量扫描
  function patrolLocks(){ try{
    for(const k in window){ if(isLockKey(k)){ try{ delete window[k]; }catch(_){} } }
  }catch(_){} }

  if(CFG.sanitizeCookies){
    patrolCookie(); patrolLocks();
    setInterval(patrolCookie, CFG.cookiePollMs);
    setInterval(patrolLocks, CFG.cookiePollMs * CFG.lockPollMul);
  }

  /* ═══ SECTION 19 · Unified MutationObserver (microtask batch, shared scanner) ═══ */
  const pending=[]; let scheduled=false;
  const schedule = (typeof queueMicrotask==='function')
    ? (fn)=>queueMicrotask(fn)
    : (fn)=>Promise.resolve().then(fn);
  function flush(){ scheduled=false; if(!pending.length) return; const batch=pending.splice(0); let removed=0;
    for(const n of batch){ if(!n.isConnected) continue; try{
      if(n.tagName==='SCRIPT' && n.src && decide(n.src).blocked){ n.remove(); removed++; continue; }
      if(isMalClass(n.className)){ n.remove(); removed++; continue; }
      if(n.tagName==='IMG' && n.src && decide(n.src).blocked){ n.remove(); removed++; } }catch(_){} }
    if(removed) Stats.domRemoved+=removed; }
  const obs=new MutationObserver(muts=>{ for(const mu of muts) for(const n of mu.addedNodes) if(isEl(n)){
      // 队列上限保护：超限时立即同步刷新，防止突变风暴 OOM
      if(pending.length>=CFG.pendingMax){ Log.warn('DOM','突变队列超限，切换同步刷新'); flush(); }
      pending.push(n); }
    if(!scheduled && pending.length){ scheduled=true; schedule(flush); } });
  const start=()=>{ obs.observe(document.documentElement,{childList:true,subtree:true}); Log.debug('Init','DOM 清道夫启动'); };
  document.documentElement ? start() : addEventListener('DOMContentLoaded', start, { once:true });

  /* ═══ SECTION 20 · DIAGNOSTIC API ═══ */
  // 默认不暴露到全局；仅在 exposeGlobal 开启时挂载，且用不可枚举 Symbol 键降低指纹
  const API = { stats:()=>({ ...Stats }), cache:()=>({ size:cache.size, cap:CFG.lruSize }), decide, report, version:VERSION };
  if(CFG.exposeGlobal){
    try { Object.defineProperty(window, SYM, { value: API, enumerable:false, configurable:true, writable:false }); } catch(_) {}
  }
  Log.ok('BOOT', '◆ Blockcat v'+VERSION);
})();
