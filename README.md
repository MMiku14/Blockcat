# Blockcat
一个专门反制黑产/🟡广告和恶意行为的由ai编写的脚本

⚙️ 配置

可在脚本顶部的 `CFG` 对象中按需调整：

```javascript
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
```
使用只需要导入到相应的浏览器中的油猴或者自带的脚本功能

| 脚本路径 | 类型 | 功能 |
|---------|------|------|
| /abc/fixed_ui_*.js | 广告注入 | 从配置池拉取并注入浮窗广告 |
| /abc/fixed_jump_*.js | 跳转劫持 | 概率性强制跳转到博彩落地页 |
| /000/report_error_video/script.js | 反馈上报 | 用户举报失效视频，众包内容质检 |
| /cn/home/web/static/player/dplayer/DPlayer.min.js | 播放器 | 视频播放 |


| 端点 | 方式 | 用途 |
|------|------|------|
| /000/flink/click.php | sendBeacon/Image | 广告点击与跳转统计（param_id=202为渠道标识） |
| /000/flink/url.php | 302中转跳转 | 隐藏真实博彩落地页URL，规避来源追踪 |
| /000/flink/check_domain_v2.php | Image预加载 | 域名存活检测（10%概率触发，配合永久跳转自动切换） |
| /000/report_error_video/report.php | fetch GET | 失效视频举报收集 |
| /abc/data_*.json | fetch | 广告/跳转配置池（含图片、落地页、到期日、投放范围、权重） |

(*为6位随机16进制字符)


**四重反侦察设计**：
1. **前3次豁免**：新用户/审核人员首次访问不触发跳转，降低被标记概率
2. **概率触发**：仅15%概率跳转（老用户），非100%，规避自动化检测的确定性判定
3. **延迟触发**：3秒延迟，模拟用户正常浏览行为
4. **服务端中转**：经 `url.php` 302跳转，浏览器地址栏不直接暴露博彩URL，规避安全拦截
