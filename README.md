# Blockcat
一个专门反制黑产/🟡广告和恶意行为的由ai编写的脚本

⚙️ 配置

可在脚本顶部的 `CFG` 对象中按需调整：

```javascript
const CFG = {
  log: true,               // 是否开启日志
  logLevel: 'INFO',        // 日志级别：DEBUG / INFO / WARN / ERROR / SUCCESS
  mockResponses: true,     // 拦截后是否返回模拟响应（无副作用，建议开启）
  m3u8Cleanse: true,       // 是否清理 M3U8 广告段
  antiDetect: true,        // 是否开启反检测
  blockPopups: true,       // 是否拦截弹窗广告
  blockAutoplay: true,     // 是否阻止广告资源自动播放
  sanitizeCookies: true,   // 是否定时清理广告 Cookie
  hlsHijacker: true,       // 是否劫持 Hls.js 加载源监控
  strictNonStdPort: false, // 是否严格拦截第三方非标端口
  cheapTldBlock: false,    // 是否拦截廉价 TLD（.xyz .casa 等）
  blockPunycode: true,     // 是否拦截 Punycode 域名
  blockCloud: false,       // 是否拦截 AWS/Azure 临时域名
  lruSize: 800,            // LRU 缓存大小
  cookiePollMs: 4000,      // Cookie 巡逻间隔 (ms)
  pendingMax: 5000,        // DOM 突变批处理阈值
  exposeGlobal: false,     // 是否暴露调试 API 到 window.__bc__
};
```
使用只需要导入到相应的浏览器中的油猴或者自带的脚本功能

关键行为脚本清单

| 脚本路径 | 类型 | 风险 | 功能 |
|---------|------|------|------|
| /abc/fixed_ui_*.js | 广告注入 | L5 | 从配置池拉取并注入浮窗广告 |
| /abc/fixed_jump_*.js | 跳转劫持 | L5 | 概率性强制跳转到博彩落地页 |
| /000/report_error_video/script.js | 反馈上报 | L3 | 用户举报失效视频，众包内容质检 |
| /cn/home/web/static/player/dplayer/DPlayer.min.js | 播放器 | L2 | 视频播放 |

**重要**：站点**未使用任何标准第三方统计**（无百度统计 `hm.baidu.com`、无CNZZ、无Google Analytics），而是改用**自建私有追踪端点** `/000/flink/`，避免通过公共统计平台暴露站点关联与真实身份。

### 追踪与上报端点

| 端点 | 方式 | 用途 |
|------|------|------|
| /000/flink/click.php | sendBeacon/Image | 广告点击与跳转统计（param_id=202为渠道标识） |
| /000/flink/url.php | 302中转跳转 | 隐藏真实博彩落地页URL，规避来源追踪 |
| /000/flink/check_domain_v2.php | Image预加载 | 域名存活检测（10%概率触发，配合永久跳转自动切换） |
| /000/report_error_video/report.php | fetch GET | 失效视频举报收集 |
| /abc/data_*.json | fetch | 广告/跳转配置池（含图片、落地页、到期日、投放范围、权重） |

(*为6位随机16进制字符)
