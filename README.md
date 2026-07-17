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

## 💡 常见问题

**Q: 拦截后视频无法播放？**  
A: 请将 `m3u8Cleanse` 设置为 `false` 并刷新。若恢复正常，说明该站点广告段与正片高度混合，需要手动适配规则。

**Q: 部分网站报错或功能异常？**  
A: 尝试逐项关闭 `blockPopups`、`blockAutoplay`、`antiDetect` 等开关，找出冲突项。也可将日志级别设为 `DEBUG` 观察拦截记录。

**Q: 与 uBlock Origin 等广告拦截扩展冲突吗？**  
A: 兼容良好，但建议只开启一侧的脚本拦截功能，避免重复拦截。Blockcat 更专注于脚本层面动态注入的对抗。
