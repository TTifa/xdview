# 摄像机接入指南

xdview 通过设备开放的 ONVIF 和 RTSP 服务接入。先确认具体型号、固件和供电方式支持这些服务；App 能看画面不等于提供第三方取流能力。

本文提炼自 2026-10-31 的接入记录，未在本次文档整理中重新验证设备。所有 IP、账号和密码均为示例，请替换成本机值。

## 通用步骤

1. 将 PC 与摄像机接入可互通的局域网，确认未被访客网络、AP 隔离或防火墙隔开。
2. 在设备 App 或管理页面开启第三方接入/ONVIF，设置设备专用账号；账号未必等于厂商云账号。
3. 启动 xdview 扫描设备。未发现时，在界面“添加设备”中填写 ONVIF 地址、账号与密码；扫描失败不能单独证明不支持。
4. 流地址可留空，由 ONVIF GetStreamUri 获取；只有 RTSP 地址时也可直接配置播放，但云台仍需要 ONVIF。
5. 使用样例修改配置后，先检查配置和取流，再检查云台；字段定义见 [配置参考](CONFIGURATION.md)。

```toml
[[camera]]
id = "cam1"
name = "摄像机示例"
url = ""
onvif = "http://192.168.1.64:2020"
username = "example_user"
password = "example_password"
caps = "ptz,snapshot,recording"
```

端口取设备实际值；不支持云台的机型应移除 ptz。不要将示例密码用于真实设备。

## 小度摄像机

- 在设备设置中查找“第三方平台接入（ONVIF）”或类似入口；是否出现取决于型号、固件与 App 版本。
- 开启后设置独立访问账号，并记录页面给出的 IP/端口。没有该入口时，先确认厂商支持情况，不能据此假定可以用私有接口代替。
- DuerOS-IPC-3484、固件 1.0.0 的历史验证使用 ONVIF 8080 端口，完成取流和 PTZ；该端口不推广到其他型号。
- 本项目不实现小度云端账号登录、私有 RTC 信令或通话。日常接入不需要抓取 App 的会话 token 或签名流地址。

## TP-Link 摄像机

先分清设备系列；Tapo、VIGI 与国行 TL-IPC 的设置入口和账号体系可能不同。

| 系列 | 账号与服务准备 | 常见地址参考 |
|---|---|---|
| Tapo | 查找 App 中的“摄像机账户”，设置独立设备账号，并确认型号支持第三方接入 | RTSP /stream1、/stream2；ONVIF 常见 2020 |
| VIGI | 在管理页面检查 ONVIF 服务与认证设置 | 按设备实际端口；常见 RTSP /stream1、/stream2 |
| 国行 TL-IPC | 历史 TL-IPC43AW-COLOR 使用设备用户名和设备密码；不能直接套用海外 Tapo 的入口 | 已测机型 ONVIF 2020、RTSP /stream1、/stream2 |

上述路径仅用于定位，最终以设备说明或 ONVIF 返回值为准。电池机、门铃、并发路数和音频能力等限制也要按型号确认。

本项目暂不支持 SHA-256 Digest；若设备允许选择认证算法，可使用其支持的 MD5 模式。不要将某台设备的免鉴权现象当作其他设备的默认行为。

## 验证与排错

在项目根目录、完成当前版本构建后执行。以下为 PowerShell 命令，替换示例地址与账号；含凭据的命令和输出不要直接分享到问题附件。

```powershell
.\target\release\xdview.exe --config config.toml --check
.\target\release\xdview.exe --onvif http://192.168.1.64:2020 example_user example_password
cargo run -p xd-media --example probe_url -- "rtsp://example_user:example_password@192.168.1.64:554/stream1" 5
```

| 现象 | 检查 |
|---|---|
| 扫描不到 | 服务是否开启、设备实际端口、同网段与隔离设置；尝试手填 ONVIF 地址 |
| 401 / NotAuthorized | 是否使用设备专用账号、密码是否正确、设备要求的认证算法 |
| 有画面但云台不可用 | 是否支持 PTZ、ONVIF 地址和账号是否正确、设备是否正被其他客户端控制 |
| 云台方向相反 | 关闭追踪/巡航后验证方向，再设置 invert_pan / invert_tilt |
| 卡顿或连接被拒 | 设备并发流限制、其他客户端、网络与主/子码流选择 |
| 端点返回 404/405 | 查询 GetCapabilities 的对应服务 XAddr，不假定所有服务共用路径 |

云台验证会实际移动设备。受控方向、松手停止、追踪干扰及退出行为的验证方法见 [测试与验证](TESTING.md#4-真机验证与排错)；历史排错记录见 [踩坑记录](踩坑记录.md)。
