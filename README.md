# xdview

智能摄像机播放器 —— 实时预览、云台、截图、录像、本地滚动缓存。

通过标准 **ONVIF + RTSP** 接入摄像机。支持情况取决于具体型号、固件与设备开放的能力；接入前请确认设备支持。

**目录**：[特性](#特性) · [快速开始](#快速开始) · [使用](#使用) · [配置](#配置) · [已验证设备](#已验证设备) · [文档](#文档)

---

## 特性

**接入**
- 启动自动连接；流地址留空时用 ONVIF `GetStreamUri` 自动取
- WS-Discovery + 网段探测发现设备；扫描不到时可手填 ONVIF 地址
- ONVIF 鉴权双栈：WS-UsernameToken + HTTP Digest（MD5），兼容 TP-Link / 海康 / 大华
- 账号写在配置字段里即可，程序自动补进 RTSP 地址（显示时打码）

**画面**
- ffmpeg 解码 → GPU 纹理；RTSP 默认走 TCP；支持硬件解码
- 低延迟参数按协议分流（文件/HLS 不吃低延迟参数，避免丢帧）
- 断流自动重连；主/子码流一键切换；实时统计（fps / 分辨率 / 解码吞吐）
- 预览可暂停／继续，录像与滚动缓存不受影响；截图保存当前画面，继续后等待新帧
- 深色 / 浅色皮肤一键切换（顶栏月/日图标），偏好写运行数据目录，不改写 config.toml

**控制**
- 云台四向、变焦、预置位一键跳转；方向可按机型反向校正
- 截图落 BMP、手动录像落 MKV（`-c copy` 不重编码，正常停止后检查文件完整性）

**录制**
- **滚动缓存（DVR）**：实时流按片落盘，只留最近 N 分钟，「导出最近 10 分钟」拼成单文件
- 不重编码，减少编码开销；缓存清理时处理空残片

**工程**
- `xd-core` / `xd-media` 零第三方依赖；UI 只依赖 `eframe`
- 中文字体自动加载（含 TTC → TTF 抽取）
- 完整命令行自检：`--check` / `--discover` / `--onvif` / `--camera` / `--stats`

---

## 快速开始

### 前置

1. **Rust stable**（MSVC 目标；各依赖的最低版本要求以实际构建结果为准）
2. **Visual Studio Build Tools**，勾选「使用 C++ 的桌面开发」
3. **ffmpeg / ffprobe** 在 PATH 里（或配置里写全路径）

### 构建

```bat
build.bat
```

> 脚本定位 MSVC/Windows SDK 并设置构建环境；也可在等价 Developer Shell 中执行 `cargo build --release -p xdview`。
> 工具链准备见 [docs/TESTING.md](docs/TESTING.md#2-windows-构建环境)；Git 的 GNU `link.exe` 抢占问题见 [踩坑记录](docs/踩坑记录.md)。

### 运行

```bat
target\release\xdview.exe
```

首次运行在当前目录生成 `config.toml`，改完重启即可。

---

## 使用

### 命令行

| 命令 | 作用 |
|---|---|
| `xdview.exe` | 启动界面，自动连第一路 |
| `xdview.exe --check` | 只解析配置，打印设备清单/协议/能力/方向标记 |
| `xdview.exe --discover` | WS-Discovery + 网段探测扫描 ONVIF 设备（不支持 WS-Discovery 的设备靠网段探测） |
| `xdview.exe --onvif http://ip:2020 user pw` | 测 ONVIF 端点：设备型号 / Profile / RTSP / 云台 |
| `xdview.exe --camera tplink --stats` | 直连某一路（id/序号/名称片段），每 2 秒打印帧率 |
| `xdview.exe --focus` | 启动即进专注模式（只看视频；双击画面进入，Esc / 悬浮按钮退出） |
| `xdview.exe --mini` | 启动即进小窗模式（无边框 + 置顶，隐含 `--focus`；画面任意处可拖动，记住位置和尺寸） |

排错时比开窗口快得多的两个：

```bat
cargo run -p xd-media --example probe_url -- "rtsp://user:pw@ip:554/stream1" 5   :: 地址能不能出画面
cargo run -p xd-core  --example ptz      -- http://ip:2020 user pw verify        :: 云台电机响不响应
```

### 界面

左侧设备列表（扫描、添加设备手填 ONVIF、接入、连接/断开），中央视频画布，右侧统计 + 云台 + 预置位 +
截图/录像/滚动缓存，底部日志。三栏宽度可拖拽分界线调节。

**专注模式（只看视频）**：双击画面或顶栏「专注」进入——隐藏所有面板，视频占满窗口；
「小窗」= 无边框 + 置顶的紧凑形态，按视频比例给默认尺寸，画面任意处可拖动
（右上悬浮按钮行除外）；滚轮放大后画面用于平移，需先双击或点「复位」还原再拖动窗口，
位置与尺寸自动记住（主窗口 / 小窗两槽），下次启动原样恢复；几何写运行数据目录的
`window_geom.toml`，不改写含明文密码的 `config.toml`，删除该文件即可重置。
「全屏」与「小窗」互斥：小窗态点全屏会先回到标准专注窗口再全屏，退出全屏后仍是专注态，
需要再点「小窗」回到紧凑形态（避免无边框置顶窗口切全屏时画面只占左上角）。

**扫描**：点击「扫描局域网」查看结果，接入新设备或切换已加入的设备；支持重新扫描和手动添加，关闭后保留结果。

**主题**：顶栏右侧「夜间 / 日间」图标按钮在两套皮肤间切换。

---

## 配置

最小配置只有一段：

```toml
[[camera]]
id = "cam1"
name = "门口"
url = "rtsp://admin:pw@192.168.1.10:554/stream1"
onvif = "http://192.168.1.10"
username = "admin"
password = "pw"
```

完整字段（全局设置、每路摄像机、各品牌地址、方向校正、缓存参数）见
**[docs/CONFIGURATION.md](docs/CONFIGURATION.md)**。

> `config.toml` 和本地配置变体含明文密码，已加入 `.gitignore`；不要提交真实配置、签名 URL 或画面。
> 截图、录像、缓存与导出默认写入系统用户数据目录（Windows：`%LOCALAPPDATA%\xdview`），可用 `[settings] data_dir` 指定其他位置；详见 [运行数据目录](docs/CONFIGURATION.md#运行数据目录)。
> 开启设备服务、填写账号和排错步骤见 [摄像机接入指南](docs/CAMERA_SETUP.md)。

---

## 已验证设备

以下为 2026-10-31 的历史验证结果，不代表同品牌全部型号可用，也不代表本次重新完成了真机测试。

| 设备 | 实测 | 接法 |
|---|---|---|
| 小度 **DuerOS-IPC-3484** | 1280x720 / 20.0 fps，ONVIF + 云台 | [接入小度](docs/CAMERA_SETUP.md#小度摄像机) |
| TP-Link **TL-IPC43AW-COLOR** | 1280x720 / 15.3 fps，ONVIF + 8 预置位 | [接入 TP-Link](docs/CAMERA_SETUP.md#tp-link-摄像机) |
| 海康 / 大华 / 宇视 | 仅提供常见地址格式，未列为已验证 | [docs/CONFIGURATION.md](docs/CONFIGURATION.md) |

---

## 文档

| 文档 | 内容 |
|---|---|
| [docs/CAMERA_SETUP.md](docs/CAMERA_SETUP.md) | 设备准备、账号、接入和排错 |
| [docs/CONFIGURATION.md](docs/CONFIGURATION.md) | 配置完整参考 |
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) | 架构分层、设计取舍、扩展点、已知限制 |
| [docs/TESTING.md](docs/TESTING.md) | 测试清单、端到端怎么跑、真机怎么验 |
| [docs/踩坑记录.md](docs/踩坑记录.md) | 真实踩过的坑与修法 |
| [docs/README.md](docs/README.md) | 文档索引与写作约定 |
| [AGENTS.md](AGENTS.md) | **改代码前先读**：核心约束、基础门禁、专题阅读入口 |

---

## 说明

- 设备接入请遵守当地法律法规与厂商服务条款，仅用于自己拥有权限的设备。

本项目采用 [MIT 许可证](LICENSE)。
