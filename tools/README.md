# 辅助工具范围

公开项目的日常诊断入口是 Cargo examples：

| 工具 | 用途 |
|---|---|
| xd-media 的 probe_url | 检查单个媒体地址与解码 |
| xd-core 的 ptz | 检查 ONVIF 云台状态、预置位与受控移动 |
| xd-media 的 export_cache | 导出已有滚动缓存 |

命令和验证前提见 [测试与验证](../docs/TESTING.md)。

## 仅本地保留的研究工具

既有 tools/re/ 与 tools/fetch_stream_url.ps1 已被忽略，供原作者研究归档使用，不是构建或运行前提。原始样本和现场记录保留在 docs/local/，中间产物保留在 .analysis/；全新克隆不会包含这些文件。

这些工具可能输出原始日志、设备标识或签名地址。不要将其输出作为公开日志、问题附件或发布资产；公开仓库不提供获取厂商私有会话凭据的操作流程。
