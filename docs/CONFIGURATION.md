# 配置参考

> `config.toml` 的完整字段说明。**改配置项时，本文、`config.example.toml` 与
> `crates/xd-core/src/config.rs` 的内置 `EXAMPLE` 三处要同步**（见 [../AGENTS.md](../AGENTS.md) §5）。
>
> `config.toml` 含明文密码，已加入 `.gitignore`，**不要提交**。

---

## 最小可用

```toml
[[camera]]
id = "cam1"
name = "门口"
url = "rtsp://admin:pw@192.168.1.10:554/stream1"
```

只写 `url` 就能播。`username`/`password`/`onvif` 是为了让云台和「URL 缺账号自动补」生效。

---

## `[settings]` 全局设置

```toml
[settings]
data_dir = ""           # 空值使用系统用户数据目录；例如 D:/xdview-data
ffmpeg = "ffmpeg"        # 或 "C:\\ffmpeg\\bin\\ffmpeg.exe"
max_width = 1280         # 0 = 原始分辨率
reconnect = true
low_latency = true
high_stream = true
no_proxy = true
insecure_tls = false
hwaccel = true
cache_enabled = false
cache_minutes = 30
cache_segment_secs = 60
```

| 字段 | 默认 | 说明 |
|---|---|---|
| `data_dir` | 空字符串 | 截图、录像、缓存、导出的共同根目录；空值使用系统用户数据目录，绝对路径直接使用，相对路径以配置文件目录为基准 |
| `ffmpeg` | `ffmpeg` | ffmpeg / ffprobe 可执行文件路径；不在 PATH 就写全路径 |
| `max_width` | `1280` | 解码后最大宽度（等比缩放、偶数对齐）；`0` = 原始分辨率 |
| `reconnect` | `true` | 断流自动重连（0.5s 起指数退避，最长 8s） |
| `low_latency` | `true` | 关闭 ffmpeg 内部缓冲降低延迟。**只对实时协议生效**（rtsp/rtmp/srt/udp/rtp），文件与 HLS 会跳过 |
| `high_stream` | `true` | 默认使用主码流 |
| `no_proxy` | `true` | 拉流子进程清空 `http_proxy`/`https_proxy`/`all_proxy` 及其大写形式。ffmpeg 会把裸 TCP 也塞进代理，实测直接导致 TLS 失败 |
| `insecure_tls` | `false` | 给 ffmpeg 加 `-tls_verify 0`。默认关闭；仅对已确认的证书兼容问题临时使用，不能作为所有 TLS 流的默认设置 |
| `hwaccel` | `true` | RTSP 加 `-hwaccel auto`；某台相机硬解花屏/失败时置 `false` |
| `cache_enabled` | `false` | 启动即开始滚动缓存（实时流切片落盘，只留最近 N 分钟） |
| `cache_minutes` | `30` | 缓存保留时长（分钟），更早的自动删 |
| `cache_segment_secs` | `60` | 每片时长（秒），导出粒度就是它。下限 5（更短会与 ffmpeg 文件名 1 秒粒度冲突） |

> 窗口位置/尺寸**不在本文件**：程序退出时写运行数据目录的 `window_geom.toml`
> （`win_*` 主窗口 / `mini_*` 小窗两槽，坐标允许负值以支持左侧/上方副屏）。
> 删除该文件即可重置。这样 `config.toml` 里的明文密码、手写注释和运行期外部编辑都不会被程序覆盖。
> 旧版配置若还残留 `win_*` / `mini_*` 键，程序**静默忽略**并在启动时给一行中文提示，可直接删除。

---

## `[[camera]]` 每路摄像机

可写多组，界面左侧切换。**一次只连一路。**

```toml
[[camera]]
id = "door"
name = "门口"
url = "rtsp://admin:pw@192.168.1.10:554/Streaming/Channels/101"
sub_url = "rtsp://admin:pw@192.168.1.10:554/Streaming/Channels/102"
onvif = "http://192.168.1.10"
username = "admin"
password = "pw"
caps = "ptz,zoom,snapshot,recording"
invert_pan = false
invert_tilt = false
```

| 字段 | 必填 | 说明 |
|---|---|---|
| `id` | 否 | 标识（缺省 `cam1`/`cam2`…），也是缓存目录名与文件名前缀 |
| `name` | 否 | 显示名（缺省同 `id`） |
| `url` | 是 | 主码流地址。**留空**则启动后用 ONVIF `GetStreamUri` 自动取 |
| `sub_url` | 否 | 子码流地址；不写则主/子码流切换无效 |
| `onvif` | 否 | ONVIF 入口：主机 `http://ip`、带端口 `http://ip:2020` 或完整 `.../onvif/ptz_service`，程序自动归一化 |
| `username` / `password` | 否 | ONVIF 鉴权；**RTSP 地址里没写账号时也会用它补进 URL**（含百分号编码），日志中打码为 `***` |
| `caps` | 否 | `ptz,zoom,audio,talk,snapshot,recording` 或 `all`；不写且无 `onvif` 时自动关闭云台 |
| `invert_pan` | 否 | 云台左右反向（设备开了镜像/反向或吊装时常见） |
| `invert_tilt` | 否 | 云台上下反向 |

**方向怎么定**：先跑 `cargo run -p xd-core --example ptz -- <base> <user> <pass> verify`，
看角度往哪边走再决定翻哪个轴。

---

## 常见设备地址

| 品牌 | 主码流 | 子码流 | ONVIF |
|---|---|---|---|
| 海康 | `rtsp://user:pw@ip:554/Streaming/Channels/101` | `…/102` | `http://ip` |
| 大华 | `rtsp://user:pw@ip:554/cam/realmonitor?channel=1&subtype=0` | `subtype=1` | `http://ip` |
| TP-Link Tapo / VIGI | `rtsp://user:pw@ip:554/stream1` | `…/stream2` | `http://ip:2020` |
| TP-Link 国行 TL-IPC | `rtsp://ip:554/stream1`（认证按设备设置） | `…/stream2` | `http://ip:2020`，用**设备密码** |
| 小度 DuerOS-IPC | 留空，由 ONVIF 自动取 | 同左 | `http://ip:8080` |
| 宇视/通用 | `rtsp://user:pw@ip:554/media/video1` | `video2` | `http://ip` |

各品牌的账号与服务准备见 [摄像机接入指南](CAMERA_SETUP.md)，最终以具体型号和固件为准。

---

## 密码里的特殊字符

`username`/`password` 写在配置字段里最省心：程序会做百分号编码再拼进 URL。
手写进 `url` 时，`@`、`:`、`/`、`#`、空格这类字符会把地址截断。

## 运行数据目录

默认不向项目或安装目录写入画面、缓存和导出：

| 平台 | 默认根目录 |
|---|---|
| Windows | %LOCALAPPDATA%\xdview；变量缺失时使用 %USERPROFILE%\AppData\Local\xdview |
| macOS | ~/Library/Application Support/xdview |
| 其他平台 | $XDG_DATA_HOME/xdview；未设置时使用 ~/.local/share/xdview |

macOS/其他平台仅列路径规则，不代表 GUI 和子进程生命周期已完成跨平台验证。
根目录下按 snapshots/、recordings/、cache/<设备 id>/、exports/ 分类；设备配置导出为根目录下的 devices_export.toml（含明文密码）。
实际路径在启动日志和 --check 中显示。无法定位系统用户目录时会提示设置 data_dir，不回退到当前目录。

如需外置磁盘，可在 [settings] 设置 data_dir = "D:/xdview-data"；留空使用系统目录。相对值如 media 表示配置文件旁的 media/，不随启动目录变化。路径中的 ~、环境变量不会展开，Windows TOML 路径建议使用正斜杠。

本次调整不会移动或删除旧 snapshots/、recordings/、cache/、exports/。停止录制和缓存后可自行归档；若要继续使用旧目录，显式将 data_dir 指向旧根目录。命令行 export_cache 接受的输入/输出路径仍由调用者指定。
