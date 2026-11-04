# AGENTS.md — 项目协作规范

> 改代码前先读本文；按任务阅读下方专题文档。详细资料只在对应文档维护。

## 0. 项目与阅读入口

**xdview** 是 PC 摄像机播放器，提供 ONVIF 云台、截图、录像和滚动缓存。
当前以标准 ONVIF + RTSP 接入，支持情况以型号、固件和实测为准。

| 模块 | 职责 |
|---|---|
| xd-core | 设备模型、配置、ONVIF 与发现 |
| xd-media | ffmpeg 管线、帧、录像、缓存与进程管理 |
| xd-app（Cargo package / 二进制：xdview） | eframe 界面、后台控制与生命周期 |

| 任务 | 改动前阅读 |
|---|---|
| 首次上手 | [README.md](README.md)；检查工作区与相关近期工作日志 |
| 调整模块、依赖、线程、进程或解析器 | [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)（目录地图、设计与实现约定） |
| 写测试、构建或做真机验证 | [docs/TESTING.md](docs/TESTING.md)（环境、分层用例与命令） |
| 改配置 | [docs/CONFIGURATION.md](docs/CONFIGURATION.md) |
| 排查环境或设备兼容问题 | [docs/踩坑记录.md](docs/踩坑记录.md)；对应设备接入文档见 [docs/README.md](docs/README.md) |

本文区分强制边界与默认选择，不代表现有实现已全部满足。保留已有改动，记录相关缺口，不扩大任务范围。

## 1. 强制边界与默认选择

**强制边界**

- 媒体传输、解复用和编解码交给 ffmpeg；不自写 H.26x 解码器。ONVIF、HTTP、WS-Discovery 属于控制与发现。
- 唯一 GUI 框架为 eframe（含 egui/epaint）；UI 线程不执行阻塞网络、耗时文件操作或进程/线程等待。
- 所有生产子进程均需隐藏非交互窗口、可取消并回收；Windows 检查 Job 归属，父进程异常退出后不能继续后台写盘。
- 设备命令与回执绑定设备及连接代次，丢弃迟到结果；云台移动必须有有限超时与停止兜底。
- 真实凭据、签名 URL 和画面不入库；所有日志入口（含 ffmpeg stderr、诊断 CLI）脱敏。配置变体、截图、导出及附件均在此范围。
- Rust 代码交付执行 §4 门禁，不新增 warning；已有失败如实记录，不能声明全绿。

**默认选择与例外**

- xd-core、xd-media 默认无第三方依赖，xd-media 可依赖工作区内的 xd-core；辅助依赖不等同于第二套 GUI。
- 新依赖比较标准库实现的复杂度/边界风险，与依赖的维护、许可证、传递依赖和构建成本；零依赖不自动更安全。
- 复杂协议、XML、TLS、认证优先考虑成熟实现并尽量隔离到适配 crate。调整核心依赖边界时，在方案/PR 说明理由与验证。
- eframe 版本、平台支持与设备经验见专题文档；变更需核对适用版本，常规实现选择不额外增加审批步骤。

## 2. 编码与错误处理

- 注释和文档以中文为主，解释意图、限制与取舍；协议术语及标识符保留原文。
- 公开 API 写文档注释，模块写模块说明；涉及资源、输入限制和失败行为时明确契约。
- UI/CLI 提供中文错误及下一步。简单函数可用 `Result<T, String>`；库内部需要分类处理时允许有类型错误，在展示边界转换。
- 可恢复路径不用 `unwrap()/expect()`；不可信输入须检查索引、尺寸运算和分配上限，不能以吞错或默认成功掩盖失败。
- 解析器声明支持子集，拒绝畸形/截断输入；配置保证往返等价且安全写回，外部输入生成的路径限定在目标目录。
- 配置键用 `snake_case`、CLI 参数用 `--kebab-case`、UI 文案用中文；保持改动集中。

## 3. 测试原则

- 覆盖边界、失败分支和资源回收；测试名描述断言，单元测试通常与实现同文件，必须标 `#[test]`。
- 外部协议变更有真实本地 socket 联调，不依赖单次 read 收齐；异步场景验证迟到结果与取消。
- ffmpeg/真机端到端放 `tests/*.rs`，默认 `#[ignore]`；本地生成视频与真机用例分别执行。
- 可自动化的 bug 先加修复前会失败的回归测试；无法稳定自动化时记录最小复现、前后证据和未覆盖部分。
- 生命周期、解析和设备验证细则按 §0 链接阅读；不要只靠参数字符串断言或旧日志证明行为正确。

## 4. 基础门禁与交付

Rust 代码或依赖变更，在正确的 MSVC/SDK 环境执行：

```powershell
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --release -p xdview
# 仅在构建成功后执行
.\target\release\xdview.exe --config config.example.toml --check
```

- 按改动范围追加 [docs/TESTING.md](docs/TESTING.md) 的验证；build.bat 或等价工具链环境均可。
- 纯文档检查 diff、命令及链接；纯样式还需截图，涉及逻辑/依赖的 UI 改动仍执行 Rust 门禁。
- 结果逐项写“通过 / 失败 / 未执行（原因）”；环境错误不笼统忽略，区分已有失败和新增失败。
- 缺少设备/工具时完成可运行部分并列出剩余项，不将跳过当通过，不以旧二进制替代本次构建。

## 5. 文档同步与工作日志

| 变更 | 同步位置 |
|---|---|
| 配置项 | docs/CONFIGURATION.md + config.example.toml + config.rs 的 EXAMPLE |
| CLI 参数 | README.md 使用节 + main.rs 的 --help |
| API 行为/签名 | 对应 API 文档；文件或职责变化另更新 docs/ARCHITECTURE.md 目录地图 |
| 环境/设备经验 | docs/踩坑记录.md 或对应接入文档 + 必要的代码注释；验证方法变化同步 docs/TESTING.md |
| 研究原始资料 | 仅存 docs/local/（不入库），区分观测/推断/未知；公开设备说明归 docs/CAMERA_SETUP.md |

- 新文档登记 [docs/README.md](docs/README.md)；同类正文只维护一处，其他位置链接引用。
- 经验注明日期、设备/固件、工具版本与复现方式；未知项明示，不把历史观测写成跨版本定律。
- 实质改动后追加 .workbuddy/memory/YYYY-MM-DD.md：改了什么、为什么、验证结果及未执行项。
- 日志与审查报告也要脱敏；长期结论归 docs，源码和最新验证优先于旧日志。
