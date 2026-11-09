# 测试与验证

> 本文维护构建环境、按改动范围追加的验证和排错命令。
> 基础质量门禁只在 [../AGENTS.md](../AGENTS.md) §4 维护；测试编写原则见该文 §3。

## 1. 测试分层

| 层 | 位置 | 依赖与运行方式 |
|---|---|---|
| 单元 | 各 src/*.rs 的测试模块 | 默认运行；少量现有测试会调用本机 ffmpeg 或读取系统字体，详见 §5 |
| 联调 | crates/xd-core/tests/onvif_stub.rs 及发现模块的本地桩 | 本地 TcpListener，不依赖真实设备 |
| 本地端到端 | crates/xd-media/tests/pipeline.rs、cache.rs 中本地用例 | 需要 ffmpeg/ffprobe，默认 ignored |
| 真机端到端 | cache.rs 的 stop_finalizes_last_segment，及人工 PTZ/UI 验证 | 需要约定测试设备、地址和凭据；单独执行 |

“默认测试通过”不等于 ignored 或真机验证通过。测试数量以本次实际输出为准，本文不保存容易过期的通过计数。

## 2. Windows 构建环境

使用 x64 MSVC + Windows SDK。可运行根目录 build.bat，也可使用等价 Developer Shell/CI 环境；确认 PATH 优先解析 MSVC 的 link.exe，LIB/INCLUDE 指向配套工具链。

- build.bat 默认构建 release，含 pause，并使用 setlocal；运行后不会为后续终端命令永久配置环境，自动化不能依赖交互暂停。
- 调试构建在已配置环境执行 cargo build；cargo build --debug 不是有效构建命令。
- 脚本当前采用 GBK + CRLF；调整编码/环境探测前阅读 [踩坑记录.md](踩坑记录.md) §1、§8。
- 遇到 lock file 拒绝访问等问题，先检查进程占用、权限与构建产物，保留错误和退出码；不要一概视为无害噪声。新测试未出现时先确认 #[test] 与构建目标，再按需清理对应 crate 的产物。

以下命令使用 PowerShell，在项目根目录、已配置工具链环境执行。地址和账号均为占位值；实际凭据和签名 URL 不进入共享输出，诊断日志先脱敏。

## 3. 本地验证与追加检查

Rust 代码或依赖变化先执行 [基础门禁](../AGENTS.md#4-基础门禁与交付)，构建成功后使用样例配置自检。

```powershell
# 本地生成视频：解码出帧，不连接真机
cargo test -p xd-media --test pipeline -- --ignored --nocapture
# 本地切片和导出；显式排除真机收尾用例
cargo test -p xd-media --test cache -- --ignored --nocapture --skip stop_finalizes_last_segment
# Windows 专用父/子进程，验证句柄稳定与父进程异常退出；不操作用户应用
cargo test -p xd-media --test process -- --ignored --exact job_lifecycle
```

| 改动范围 | 追加验证 |
|---|---|
| ffmpeg 参数或帧尺寸 | pipeline；按影响覆盖奇数尺寸、缩放、实际像素/帧边界和协议参数分流 |
| 进程生命周期 | 超时/取消必须终止且 wait；强杀测试父进程后确认子进程结束，确认 Job 失败与句柄回收路径；使用专用测试进程 |
| ONVIF / PTZ / 设备切换 | socket 桩验证鉴权、服务类别端点、迟到回执、切换/断开及停止失败路径；不依赖单次 read 收齐报文。桩须按 `Content-Length` 读满请求体再应答并 `shutdown(Write)`：带未读数据直接关闭会发 RST，客户端丢响应（2026-11-04 实测偶发 2s 读超时） |
| 缓存 / 录像 / 导出 | 本地 cache 用例；验证正常收尾、最后片段、实际时长、可播性、片段次序、时间窗和路径转义 |
| UI | 相关状态与失败场景、截图检查布局；确认导出和停止不冻结 UI，断流不被误标为实时画面 |
| 运行数据目录 | 默认用户目录、自定义绝对/相对路径、从其他目录启动、恶意设备 ID、写入权限失败；--check 不创建目录 |
| 窗口几何 | 调整普通窗口后最大化或最小化并退出，再启动核对恢复；在左侧/上方副屏保存位置后分别验证副屏连接与断开，包括小窗和不同显示缩放比例 |
| 配置 / XML / HTTP | 往返、转义、中文、空值、BOM、合法语法变体、截断与慢响应；专用测试配置配合 --check |

上表是按风险选取的验收范围，不表示已有测试全部覆盖。具体设计契约见 [ARCHITECTURE.md](ARCHITECTURE.md) §4。

设备列表改动还需使用合成地址检查空列表、发现及两种手填页签、取消和重复校验、添加只保存与保存并播放、编辑名称及连接信息、删除当前和其他设备、保存失败重试；深浅主题下检查窄栏、小窗口、长名称及地址，确认菜单点击不触发行播放。空设备配置应能保存并重新载入，空设备导入仍应被拒绝。

本地 cache 追加用例覆盖手动录像 q 收尾后的真实可解码性/时长、拒绝收尾的进程超时回收、
导出取消/超时及保留已有目标。拒绝收尾使用临时 cmd fixture，不连接设备。
`export_deadline_covers_inherited_stderr` 使用包装脚本启动继承 stderr 的专用后代，
验证根进程退出后仍响应超时/取消、后代结束且已有目标不被替换；`export_pipe_holder_fixture` 只供它启动。
pipeline 的奇数 BMP 用例检查缩放后的实际像素与行边界；HTTP 单元测试使用真实本地 socket
分次发送，覆盖 Content-Length/chunked 截断、读满不等待关闭和慢速响应总截止时间。
无长度响应分别验证 EOF 成功和持续保持连接的明确失败；不以 XML 结束标签替代 HTTP 定界。
background 单元测试覆盖取消代理、迟到资源在后台析构、串行任务执行顺序及未消费失败回执仍计为 pending；真实界面压力、
缓存磁盘慢响应和设备断网仍需人工补充，不由这些单元测试替代。
录像代理另覆盖同设备保留、切换设备/断开取消；关闭状态覆盖失败后仍有媒体 pending 时再次点击直接放行。
Windows job_lifecycle 在独立 fixture 中加入普通外层 Job，验证嵌套启动；初始化失败重试由单元测试覆盖。
Windows `atomic_write_preserves_protected_and_inherited_dacl` 单元测试通过系统 PowerShell 设置及读取合成文件 DACL，
比较保存前后的访问规则与继承标志；默认测试因此也依赖 Windows PowerShell。
`unreadable_dacl_refuses_save_without_changing_original_or_leaving_temporary` 通过 OWNER RIGHTS 拒绝读取 DACL，
验证保存明确失败、原内容保留且临时文件回收，不能把权限读取失败当作目标不存在。
缓存单元测试覆盖重叠导出引用计数、目录别名、其他目录正常清理及启动失败释放保留。
本地 ignored 用例 `export_keeps_expired_segments_until_ffmpeg_finishes` 在包装脚本内设置读取屏障，
让真实清理与导出并发，验证源片段保留、实际导出时长及结束后恢复清理。
URL 脱敏单元测试覆盖合法标点密码、相邻／嵌套地址及嵌套在敏感查询值中的地址，歧义地址组整段遮盖。

## 4. 真机验证与排错

### 4.1 地址和 ONVIF

```powershell
cargo run -p xd-media --example probe_url -- "rtsp://user:pw@ip:554/stream1" 5
.\target\release\xdview.exe --onvif http://ip:2020 user pw
.\target\release\xdview.exe --camera tplink --stats
```

扫描和日常使用命令见 [README 使用节](../README.md#使用)。网段探测当前以本机私网 /24 为范围，未命中时可手填 ONVIF 入口，不能据此断定设备不支持 ONVIF。

### 4.2 云台

优先读 GetStatus，并按以下顺序验证：

1. 关闭测试设备的智能追踪/巡航，记录静置基线，排除其他客户端正在控制。
2. 进行受控的小幅正反向动作，核对位置变化方向与 invert_pan/invert_tilt 配置。
3. 验证松手/Stop 后停止；错误与退出路径也要核查，不能只验证成功移动。
4. 重复动作检查可重复性。追踪同样会改变角度，“角度变了”不能单独证明本次命令生效；无状态能力时记录替代证据与限制。

```powershell
cargo run -p xd-core --example ptz -- http://ip:2020 user pw status
# 会实际移动、尝试变焦；先完成上述环境准备
cargo run -p xd-core --example ptz -- http://ip:2020 user pw verify
```

verify 只是排错工具，不替代方向、停止和错误路径的完整验收；无电动变焦的机型不能据此判定整个 PTZ 链路失败。

### 4.3 缓存与录像

```powershell
# 仅在该次测试期间设置地址，结束后移除
$env:XD_TEST_RTSP = "rtsp://user:pw@ip:554/stream1"
try {
    cargo test -p xd-media --test cache -- --ignored --nocapture stop_finalizes_last_segment
} finally {
    Remove-Item Env:XD_TEST_RTSP -ErrorAction SilentlyContinue
}

# 真机缓存至少运行 2 分钟后导出，并核对实际录制区间与时长
$dataDir = Join-Path $env:LOCALAPPDATA "xdview" # 自定义 data_dir 时改为实际根目录
$cacheDir = Join-Path $dataDir "cache\tplink"
$outFile = Join-Path $dataDir "exports\out.mkv"
cargo run -p xd-media --example export_cache -- $cacheDir 10 $outFile
ffprobe -v error -show_entries format=duration -of default=nw=1:nk=1 $outFile
```

未设置 XD_TEST_RTSP 时，现有真机用例会直接返回；应记录“未执行”，不能将测试框架显示成功作为真机验证证据。手动录像也需单独验证停止后的文件，缓存用例通过不能替代录像验证。

### 4.4 UI 和统计

--stats 每 2 秒输出分辨率、累计帧数、fps 与解码吞吐；吞吐是 RGBA 管道字节量，不是网络码率。
对比相邻采样的帧数增量和采样时刻；已有帧后断流时，旧 fps 或未出现 [stall] 不能证明仍在出帧。截图同时检查连接状态、设备归属和布局。

日志区检查完整 `YYYY-MM-DD HH:MM:SS`，时间、级别与正文逐列左对齐；短消息不能居中，
长消息仍截断于正文列。深浅主题、不同缩放及调整侧栏宽度后复查时间列没有省略号。
连接后顶栏不再显示状态圆点、分辨率与编码；右侧实时统计仍显示分辨率与编码。
未连接、连接中及失败时保留顶栏状态文字。

预置位删除按钮应显示绘制的叉号，深浅主题均可辨，悬停提示要说明待删的预置位；
下方说明不使用依赖字体的特殊符号。删除仍等待设备回执后刷新列表。

预览暂停：首帧前不显示画面内暂停按钮；出帧后点击暂停图标应保持画面与截图像素不变，显示「已暂停」，
后台帧数仍增加，录像／缓存继续增长。点击「继续预览」后显示新画面，不补播暂停期间的帧。
普通、专注、小窗与深浅主题都要检查按钮和提示不被裁切；专注／小窗左下角按钮不应触发拖窗。
小窗右上按钮为 22px 高、8px 外边距；左下为 22px 方形暂停／播放图标，悬停说明当前动作。
普通模式暂停／播放图标位于实际画面左下角（有黑边时仍在图像内部），操作条不再重复放按钮，
实时／暂停／等待文字保持可见；悬浮图标按深色画面层配色。点击图标不应触发视频平移或双击动作。
在 320px 最小宽度下检查三个右上按钮（含放大后的复位）不溢出，图标点击只切换暂停状态。
暂停期间主动断开、切设备、切码流或清晰度后恢复预览；被动断线显示非实时提示，自动重连后仍保持暂停，
直至用户继续。自动测试验证显示帧与纹理在暂停中不变，以及恢复接收分辨率变化的新帧。
点击继续后、尚未收到新帧时必须显示「等待新画面」，不能立即标成实时；重连仅收到 `Opened` 时同样保持
非实时提示。本地 pipeline 另用单帧图片触发 EOF，验证 `Opened → Frame → Interrupted → Opened → Frame`。
媒体队列满时状态不得丢弃，取消时不能依赖消费队列才退出；相关单测默认执行。
有 RTSP 地址的设备即使 ONVIF 先失败，预览仍保持连接中且断开可用；无地址、依赖 ONVIF 取流的设备失败时
应结束连接中并显示错误。分别覆盖 ONVIF 回执在媒体启动前后到达，以及切换会话后的迟到回执。

主题验证：向运行数据目录写 `ui_prefs.toml`（`theme = "light"`）后启动，核对浅色皮肤、
顶栏图标为太阳、视频区仍为深底且浮层文字可读；删除该文件后启动应回到夜间。
偏好损坏（如 `theme = "紫色"`）也按夜间启动，不阻断运行。

滚动缓存区对照原型检查深浅两套主题：关闭时参数与导出区整体淡化，滑动开关保持可点击；
保留时长、切片粒度仅展示当前配置，悬停提示只读，不弹菜单、不改配置。
在右栏最窄与默认宽度下核对两列参数框和通栏导出按钮不裁切；开启后状态与导出行为沿用原实现。

主题写入是异步的：点一次主题按钮后立刻关窗，正常盘上应在 1 秒内退出且文件里是最后选择的主题；
把 `data_dir` 指到不可写位置（只读目录）时切主题，界面应正常切换，日志出现
「主题偏好保存失败」且提示条说明下次启动可能恢复原主题；关窗等待期间写入失败时窗口应保留，
提示可见后可再次关闭。连续快速切换后只保留最后一种，过期失败回执不应触发提示。
保存或媒体收尾持续阻塞超过六秒时，自动关闭请求不应重新进入等待；该上限统一约束保存与退出收尾。

`--stats` 另输出一行 `[dbg]` 布局探针：`screen`（客户区）、`left_w/right_w`（分栏宽度）、
`zoom`（数字变焦）、`mode`（Normal/Focus/Mini）、`fs`（视口全屏真值）；
`--focus` / `--mini` 启动即进对应模式（`--mini` 隐含 `--focus`），
配合探针 + WM_CLOSE 正常退出可免点击验证三态。窗口几何另查运行数据目录的
`window_geom.toml`（`win_*` / `mini_*` 两槽，正常退出后才写入）。

真机复现、已修复并于 **23:41 确认正常**的三项：
① 小窗点「还原」退出专注后主窗口尺寸没恢复；
② 退出专注偶发位置漂移（修法见 `restore_win_geometry` / `pending_restore`）；
③ 小窗点「全屏」满屏黑、只剩左上角小块画面（winit 的 Borderless 全屏不保证
触发尺寸事件，egui 仍按小窗尺寸绘制）。修法见 `toggle_fullscreen`：进全屏前恢复标准窗口
属性并显式 `InnerSize(monitor_size)`；全屏与小窗互斥。

23:41 扩大拖动区，23:53 真机发现**滚轮放大与双击复位失效**：在 overlay 里注册覆盖画面的
`ui.interact`（即便 `Sense::drag()` 不响应 click）仍会抢占 hit-test，使视频 `resp` 的
`hovered()` / `double_clicked()` 失效。最终改法：**overlay 不再注册任何拖动 interact**
（顶部条只纯绘制），拖窗复用 `resp.dragged()`——小窗且 `zoom <= 1`、拖动起点不在按钮行时
发 `StartDrag`；放大后 `resp` 走平移，需先复位再拖窗口。
**待真机复测**：滚轮缩放、双击复位、画面中央/右下拖动、按钮行点击不误触、
放大后拖动是平移、静止 5 秒后直接按住画面仍能拖动、无画面时也能拖动。

已知未自动化的验证项（本机合成输入被系统输入隔离挡，需真机人工确认）：
Esc 优先级（弹窗→全屏→专注）、mini+全屏组合退出后的尺寸、`--mini` 首帧是否可见大窗口、
窗口最大化/最小化后退出是否仍记住原尺寸、多屏负坐标（副屏在左/上方）下的位置记忆。

## 5. 现有覆盖主题与边界

| 测试来源 | 已有主题 |
|---|---|
| xd-core 单元 | SHA-1/MD5 向量、Digest、URL/HTTP、XML、配置、RTC URI、发现报文与本地探测桩 |
| onvif_stub.rs | Profiles、移动/变焦、StreamUri、预置位增删、404→XAddr 回退 |
| xd-media 单元 | 参数分流、ffprobe 路径、尺寸、BMP、录像路径、缓存清理/筛选/清单 |
| xd-media ignored | 本地解码、切片、拼接，以及需真机的最后片段收尾 |
| xdview | 数据目录解析与路径边界、等比适配、时长/时间戳、日志标签、TTC 抽取与系统字体 |

- ffmpeg.rs 的代理环境测试会调用 ffmpeg 和 Windows cmd；“默认测试”目前不保证完全没有外部工具依赖。
- 系统字体用例无字体时会跳过，不能等同所有平台中文显示已验证；TTC 抽取属于 xd-app 的 font.rs。
- 已有覆盖主题不意味着每个失败分支都有断言；测试函数遗漏 #[test] 也不会由普通测试自动补齐。

## 6. 证据与结果记录

逐项记录通过、失败、未执行及原因，遵循 [../AGENTS.md](../AGENTS.md) §4。
真机证据注明日期、型号/固件、操作系统、ffmpeg 版本/构建与命令；缺失信息写未知。
环境失败与代码失败分别记录，源码和本次运行结果优先于旧日志；不要为取得全绿改动无关模块。
