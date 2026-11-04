# 架构与设计

> 本文回答：**xdview 内部怎么分层、为什么这么选、能往哪扩。**
> 项目介绍见 [README](../README.md)，设备准备与接入见 [摄像机接入指南](CAMERA_SETUP.md)。

---

## 1. 分层

```
┌──────────────────────────────────────────────────────────────┐
│ crates/xd-app   (egui 桌面端)                                 │
│                                                              │
│  ┌────────────┐   ┌──────────────┐   ┌──────────────────┐   │
│  │ 设备列表    │   │ 视频画布      │   │ 云台 / 缩放 / 预置位│   │
│  └────────────┘   └──────┬───────┘   └────────┬─────────┘   │
│                          │ TextureHandle       │ Command      │
└──────────────────────────┼─────────────────────┼──────────────┘
                           │                     │
        ┌──────────────────┴───────┐   ┌─────────┴──────────────┐
        │ crates/xd-media          │   │ xd-core::onvif          │
        │                          │   │ (后台线程，不阻塞 UI)     │
        │  probe ──► ffmpeg 子进程 │   │  WS-UsernameToken       │
        │           (rawvideo)     │   │  + HTTP Digest          │
        │     │                    │   │  ContinuousMove/Stop    │
        │     ▼                    │   │  最小 HTTP/1.1 客户端     │
        │  read_exact(w*h*4)       │   └─────────┬──────────────┘
        │     │ Frame              │             │ SOAP/XML
        │     ▼                    │             ▼
        │  mpsc(容量4，满了丢帧)   │        IPC 的 ONVIF 服务
        └──────────────────────────┘
```

**数据流**：`ffprobe` 探测尺寸/编码 → 计算目标尺寸 → `ffmpeg -i <url> -f rawvideo -pix_fmt rgba -`
输出到 stdout → 读线程按 `w*h*4` 精确读帧 → `Frame` 经 mpsc（容量 4，满了丢帧）→ UI 上传 GPU 纹理。

**控制流**：UI 只发 `Command`（Move/Stop/GotoPreset/…），`control.rs` 的后台线程负责
SOAP 往返，结果以 `Feedback` 回传，全程不阻塞界面。

运行数据路径由 xd-app 的 storage.rs 统一解析；默认根目录与启动/项目目录分离，自定义相对路径以配置文件目录为基准。具体规则见 [配置参考](CONFIGURATION.md#运行数据目录)。

**录制/缓存**是独立的 ffmpeg 进程（`-c copy` 不重编码）：
手动录像产出单个 MKV；滚动缓存走 `segment` 复用器切片并自动清理。


### 1.1 目录地图

```
Cargo.toml                  workspace（3 个成员 crate）
build.bat                   Windows MSVC 构建入口（可用等价环境，见 TESTING.md）
config.example.toml         配置样例（含各品牌接入示例）
config.toml                 本机配置，gitignored
.gitignore                  忽略配置、画面、缓存、本地研究资料与工作记录
LICENSE                     MIT 许可证

crates/xd-core/             零依赖 · 协议与模型
  src/model.rs              Camera/Capabilities/Transport、凭据注入、云台方向校正
  src/config.rs             TOML 子集解析/序列化（配置解析 + 设备导入导出）+ 内置 EXAMPLE
  src/onvif.rs              SOAP 客户端：WS-UsernameToken + HTTP Digest、PTZ、GetStreamUri
  src/discovery.rs          WS-Discovery + 本机私网网段 ONVIF 探测
  src/http.rs               最小 HTTP/1.1（含 chunked）
  src/sha1.rs / md5.rs      哈希（各自带 RFC 向量测试）
  src/rtc_uri.rs            历史 RTC URI 语法模型（未接入私有信令）
  tests/onvif_stub.rs       起真 TcpListener 的联调测试
  examples/ptz.rs           云台调试 CLI（list / move / goto / status / verify）

crates/xd-media/            零依赖 · ffmpeg 管线
  src/ffmpeg.rs             参数构造（按协议分流）、ffprobe、NetOptions
  src/stream.rs             拉流→解码→Frame 流（重连、日志降噪）
  src/cache.rs              滚动缓存（segment 切片/清理/导出）
  src/record.rs             手动录像（-c copy 到 MKV）
  src/sys.rs                Windows 作业对象（子进程随父进程结束）
  src/frame.rs              RGBA 帧 + BMP 写出
  tests/pipeline.rs         真实 ffmpeg 解码端到端（ignore）
  tests/cache.rs            切片/导出/优雅收尾端到端（ignore，可用 XD_TEST_RTSP）
  examples/probe_url.rs     单地址探针（排错首选）
  examples/export_cache.rs  命令行导出缓存片段

crates/xd-app/              egui 界面
  src/main.rs               CLI 参数（--check/--discover/--onvif/--camera/--stats/--focus/--mini）
  src/app.rs                主界面 + 生命周期（on_exit 收尾）
  src/control.rs            ONVIF 后台线程（不阻塞 UI）
  src/font.rs               中文字体加载 + TTC→TTF 抽取
  src/storage.rs            系统用户数据目录、自定义路径与输出路径边界、窗口几何独立文件

docs/                       公开文档见 docs/README.md
  local/                    原始研究报告、现场记录与 APK（仅本地，不入库）
.analysis/                  逆向工作目录（APK 抽取产物与临时脚本，gitignored，可随时用 tools/re 重建）
.workbuddy/memory/          工作日志（约定见 ../AGENTS.md §5）
tools/README.md             本地研究工具范围说明
tools/re/                   研究脚本（仅本地，不入库）
tools/fetch_stream_url.ps1  历史诊断工具（仅本地，不入库）
```

---

## 2. 设计取舍

| 当前选择 | 取舍 |
|---|---|
| 标准 ONVIF + RTSP | 以设备开放能力接入，避免依赖小度私有 RTC 的 appId、token 签发及信令 |
| ffmpeg 子进程 | 复用媒体传输、解复用、编解码和平台硬解；Rust 负责模型、控制和生命周期 |
| 轻量核心 + eframe UI | 当前 xd-core、xd-media 无第三方依赖，UI 固定 eframe 0.31.1；新增依赖仍需评估解析边界、维护成本和平台支持 |
| HTTP/TOML/XML 子集 | 只覆盖当前使用场景；需明确不支持的语法，复杂协议可拆独立适配 crate，不因依赖数量扩大自研范围 |

依赖与职责调整遵循 [../AGENTS.md](../AGENTS.md) §1。升级 eframe 时核对目标版本 API 并验证，不把某次版本经验当成永久限制。

---

## 3. 能力与模块

| 能力 | 实现 |
|---|---|
| 播放与解码 | xd-media 调用 ffmpeg，将 RGBA 帧交给 xd-app 显示 |
| 设备控制 | xd-core 的 ONVIF 客户端，xd-app 后台线程负责调度 |
| 能力显隐 | xd-core::model::Capabilities |
| 录像与滚动缓存 | 独立 ffmpeg 进程，xd-media 管理输出和回收 |
| RTC URI 语法 | 历史数据模型；不代表已实现云端登录、信令或通话 |

---

## 4. 实现与维护约定

### 4.1 当前实现

- ONVIF 默认调用约定服务路径，404/405 时从 GetCapabilities 读取 XAddr 回退；部分设备将服务集中在 /onvif/service。
- 鉴权先 WS-UsernameToken，收到 401 + Digest 质询后以 MD5 / MD5-sess 重试；SHA-256 Digest 尚未实现。
- RTSP 地址缺账号时由配置补充百分号编码的凭据；显示入口已有 mask_credentials，原始 stderr 等入口仍需单独核查。
- invert_pan / invert_tilt 提供按机型的方向校正；真机判定方法见 [TESTING.md](TESTING.md#4-真机验证与排错)。
- sys.rs 提供 Windows Job 归属能力；缓存已有 q 收尾路径。所有进程入口是否覆盖、录像是否正常收尾，应结合源码和回归测试确认。

### 4.2 解析与敏感数据

以下是后续改动的设计要求，不表示现有实现已全部满足；核心约束仍以 [../AGENTS.md](../AGENTS.md) 为准。

- 自写解析器需明确支持子集。畸形、截断、类型错误不能静默变为成功；网络读写设置总期限和大小上限。
- 配置解析与序列化需验证往返等价，覆盖转义、中文、空值、BOM 和失败输入；写回配置避免破坏旧文件。
- 所有日志入口都需脱敏，包括 ffmpeg stderr、错误字符串和诊断 CLI。隐藏 URL userinfo 及 token/secret 等签名参数；只处理主动连接日志不够。
- 文件名来自设备 ID 等外部输入时，必须净化并保证输出位于目标目录内。
- .gitignore 只是防线之一；新增输出文件名或目录时核查忽略规则，保留 `config.example.toml` 等公开样例。

### 4.3 线程与设备会话

- ONVIF 放在 control.rs 后台线程，媒体使用 ffmpeg 子进程与读线程；耗时导出/停止也不能阻塞绘制。
- 设备相关命令和反馈需绑定设备标识及连接代次。切换、断开后丢弃迟到结果，不能把旧设备结果写入当前选择。
- 区分可丢弃的帧/日志和必须处理的连接状态/停止命令；队列满不等于接收方关闭。
- 云台移动设置设备侧有限超时；持续按住时可续期。Stop 不应排在长扫描之后；错误、松手、断开和退出路径均需有界停止兜底。

### 4.4 进程生命周期

- 优先集中封装启动与所有权。Windows 生产进程设置 `CREATE_NO_WINDOW`，启动后检查 `kill_with_parent/Job` 归属结果；归属失败须终止并回收子进程、报告错误，不能只调用后假定成功。
- ffmpeg、ffprobe、导出进程全部覆盖；`output()/status()` 也会启动进程。超时必须终止并 wait，不能只停止等待结果、留下线程或进程。
- 保存进程和 Job 的资源所有者；使用共享 Job 或 RAII 关闭句柄，避免每次重连泄漏句柄。归属存在启动窗口时说明限制并验证异常退出行为。
- 写媒体文件的正常停止：允许 stdin 接收 q，有限等待写完尾部，再兜底 kill + wait。MKV 不能替代正常收尾。
- `App::on_exit` 显式收尾，Drop 作幂等兜底；不要假定崩溃会运行析构。非 Windows 的空实现不能宣称已提供同等保证；新增支持平台时补相应实现和验证。
- 测试/辅助工具可以不复用生产封装，但仍需限制运行时间、隐藏非交互窗口并清理其启动的进程。

---

## 5. 扩展点

| 想做什么 | 改哪里 |
|---|---|
| 接 WebRTC（WHEP）拉流 | 新增 `xd-transport`：WHEP 只要求 `POST offer → 收 answer` + ICE，可用 `webrtc-rs`，帧输出对齐 `xd_media::Frame` |
| 自有信令适配 | 历史 rtc_uri 仅为语法模型；实际认证、会话与媒体协商需独立设计和验证 |
| 双向对讲 | ONVIF Profile T `StartAudioEncoder` + 反向 RTP；UI 侧 `caps.talk` 已预留 |
| 多路宫格 | `Vec<Stream>` + 各自 `TextureHandle`，`fit_rect` 改成网格布局 |
| 事件/移动侦测订阅 | ONVIF `CreatePullPointSubscription`，复用 `xd-core::http` + `onvif::soap_envelope` |
| 本地缓存按事件保存 | `xd-media::cache` 的片段已按时间切好，接移动侦测事件打标记即可 |

---

## 6. 已知限制

**产品范围**

- 不提供小度云端登录、私有 RTC 信令或通话。
- 仅通过设备开放的标准协议接入，不提供厂商 App 的专有设置与 AI 功能。

**实现层面**

- ONVIF 仅支持 `http://`（不带 TLS）；HTTPS 的相机需在设备侧关闭 TLS 或另接 TLS 栈。
- ONVIF 鉴权不支持 **SHA-256 Digest**（部分 VIGI 把摘要算法设成 SHA-256），会明确报错并提示去 Web UI 改回 MD5。
- 未实现双向语音对讲（需要 ONVIF Profile T 的 back-channel / 反向 RTP）。
- 多路同时预览未做（当前一次连一路；`xd-media::Stream` 本身可多实例）。
- rawvideo 每帧一次 RGBA 拷贝，1080p 约 8MB/帧；4K 下建议把 `max_width` 压到 1920。
- RTSP 已在真机验证出帧，但本地无法自测握手（ffmpeg 的 `-rtsp_flags listen` 是单客户端模式），
  遇到问题先跑 `probe_url` 看 ffmpeg 报错。

**设备侧限制**

是否开放 ONVIF/RTSP、并发连接数、认证方式、音频能力与云台行为都依赖型号和固件。
接入入口见 [CAMERA_SETUP.md](CAMERA_SETUP.md)，以设备返回值和本次验证为准。
