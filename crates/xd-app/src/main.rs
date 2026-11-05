#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
//! xdview —— 小度「智能摄像机」式 PC 实时视频客户端。
//!
//! 用法：
//! ```text
//! xdview              启动界面
//! xdview --check      只解析配置并打印摘要（不开窗口，便于排查配置）
//! xdview --config X   指定配置文件
//! ```

mod app;
mod control;
mod font;
mod prefs;
mod storage;
mod theme;

use std::path::{Path, PathBuf};

fn main() -> eframe::Result {
    let mut cfg_path = PathBuf::from("config.toml");
    let mut check_only = false;
    let mut initial_cam: Option<String> = None;
    let mut emit_stats = false;
    let mut start_focus = false;
    let mut start_mini = false;

    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--check" => check_only = true,
            "--config" => {
                if let Some(p) = args.next() {
                    cfg_path = PathBuf::from(p);
                }
            }
            "--stats" => emit_stats = true,
            "--focus" => start_focus = true,
            "--mini" => {
                start_focus = true;
                start_mini = true;
            }
            "--camera" => {
                // --camera <id|序号|名称片段>
                if let Some(v) = args.next() {
                    initial_cam = Some(v);
                }
            }
            "--onvif" => {
                // --onvif <base> [user] [pass]
                let Some(base) = args.next() else {
                    eprintln!("用法: xdview --onvif http://ip:port [user] [pass]");
                    return Ok(());
                };
                let user = args.next();
                let pass = args.next();
                let creds = match (user, pass) {
                    (Some(u), Some(p)) => Some(xd_core::onvif::Credentials {
                        username: u,
                        password: p,
                    }),
                    _ => None,
                };
                let client = xd_core::onvif::OnvifClient::new(base.clone(), creds);
                println!("端点: {base}");
                match client.device_info() {
                    Ok(info) => println!("设备: {info}"),
                    Err(e) => println!("取设备信息失败: {e}"),
                }
                println!("鉴权方式: {}", client.auth_kind().as_str());
                match client.get_profiles() {
                    Ok(profiles) => {
                        for p in &profiles {
                            println!("  profile {} ({})", p.token, p.name);
                            match client.get_stream_uri(&p.token) {
                                Ok(uri) => println!("    RTSP: {uri}"),
                                Err(e) => println!("    取流地址失败: {e}"),
                            }
                            match client.get_presets(&p.token) {
                                Ok(list) if list.is_empty() => println!("    PTZ: 无预置位"),
                                Ok(list) => {
                                    let names: Vec<String> =
                                        list.iter().map(|x| x.name.clone()).collect();
                                    println!(
                                        "    PTZ: {} 个预置位 -> {}",
                                        list.len(),
                                        names.join(", ")
                                    );
                                }
                                Err(e) => println!("    PTZ: 不可用（{e}）"),
                            }
                        }
                    }
                    Err(e) => println!("取 profile 失败: {e}"),
                }
                return Ok(());
            }
            "-h" | "--help" => {
                println!(
                    "xdview [--config <path>] [--camera <id|序号|名称>] [--check] [--discover] [--stats] [--focus] [--mini]"
                );
                println!(
                    "  --camera <id|序号|名称>       启动时直连指定摄像机（如 --camera tplink）"
                );
                println!(
                    "  --stats                       每 2 秒把帧率打到 stderr（便于无头验证）"
                );
                println!("  --focus                       启动即进专注模式（只看视频）");
                println!(
                    "  --mini                        启动即进小窗模式（无边框+置顶，隐含 --focus）"
                );
                println!("  --check                       只解析配置并打印摘要");
                println!("  --discover                    WS-Discovery + 网段探测扫描 ONVIF 设备");
                println!("  --onvif <base> [user] [pass]  直接探测某个 ONVIF 端点（取 profile 与 RTSP 地址）");
                return Ok(());
            }
            "--discover" => {
                println!("WS-Discovery 探测中（3 秒）…");
                let mut list = xd_core::discovery::discover(std::time::Duration::from_secs(3))
                    .unwrap_or_else(|e| {
                        eprintln!("WS-Discovery 失败：{e}");
                        Vec::new()
                    });
                println!("WS-Discovery：{} 台", list.len());
                let nets = xd_core::discovery::local_ipv4_list();
                if nets.is_empty() {
                    eprintln!("（取不到本机私网 IPv4，跳过网段探测）");
                } else {
                    println!(
                        "网段探测中（{:?} 的 /24，端口 {:?}）…",
                        nets,
                        xd_core::discovery::PROBE_PORTS
                    );
                    for d in xd_core::discovery::scan_lan(xd_core::discovery::PROBE_PORTS) {
                        let dup = d
                            .base_url()
                            .map(|b| {
                                list.iter()
                                    .any(|x| x.base_url().as_deref() == Some(b.as_str()))
                            })
                            .unwrap_or(false);
                        if !dup {
                            list.push(d);
                        }
                    }
                }
                println!("合计发现 {} 台设备", list.len());
                for d in &list {
                    println!(
                        "  {}  {}\n     types={}\n     scopes={}",
                        d.display_name(),
                        d.endpoint,
                        d.types,
                        d.scopes
                    );
                }
                if list.is_empty() {
                    println!(
                        "(空) 确认：同网段 / 摄像机已开启 ONVIF / 防火墙放行；\
                         实在找不到可在程序里「添加设备」手填 ONVIF 地址"
                    );
                }
                return Ok(());
            }
            other => {
                eprintln!("未知参数：{other}");
            }
        }
    }

    let cfg_path = resolve_config_path(&cfg_path);
    if let Ok(text) = std::fs::read_to_string(&cfg_path) {
        if xd_core::config::has_legacy_window_keys(&text) {
            eprintln!(
                "提示：配置文件里的 win_* / mini_* 已废弃（程序不会读取）；\
                 窗口几何改存运行数据目录的 window_geom.toml，可删除这些旧键"
            );
        }
    }
    let cfg = load_or_create(&cfg_path);
    let cfg = match cfg {
        Ok(c) => c,
        Err(e) => {
            eprintln!("配置错误：{e}");
            std::process::exit(2);
        }
    };

    let storage = match storage::StoragePaths::resolve(&cfg.settings.data_dir, &cfg_path) {
        Ok(paths) => paths,
        Err(e) => {
            eprintln!("运行数据目录错误：{e}");
            std::process::exit(2);
        }
    };

    if check_only {
        if start_focus || start_mini {
            eprintln!("（--focus/--mini 只对窗口模式生效，本次 --check 诊断运行已忽略）");
        }
        println!("配置文件：{}", cfg_path.display());
        println!("运行数据目录：{}", storage.root().display());
        println!("ffmpeg：{}", cfg.settings.ffmpeg);
        println!("最大宽度：{}", cfg.settings.max_width);
        println!("断线重连：{}", cfg.settings.reconnect);
        println!("低延迟：{}", cfg.settings.low_latency);
        if cfg.settings.cache_enabled {
            println!(
                "滚动缓存：开（保留 {} 分钟，每片 {} 秒）",
                cfg.settings.cache_minutes, cfg.settings.cache_segment_secs
            );
        } else {
            println!("滚动缓存：关");
        }
        println!("摄像机：{} 路", cfg.cameras.len());
        for (i, c) in cfg.cameras.iter().enumerate() {
            let mut flags = Vec::new();
            if c.invert_pan {
                flags.push("左右反向");
            }
            if c.invert_tilt {
                flags.push("上下反向");
            }
            let extra = if flags.is_empty() {
                String::new()
            } else {
                format!(" {}", flags.join("+"))
            };
            println!(
                "  [{}] {} ({}) 协议={} onvif={} 能力=[{}]{}",
                i + 1,
                c.name,
                c.id,
                c.transport().as_str(),
                c.onvif_base().unwrap_or_else(|| "无".to_string()),
                c.capabilities.to_spec(),
                extra
            );
        }
        return Ok(());
    }

    if let Err(e) = std::fs::create_dir_all(storage.root()) {
        eprintln!(
            "无法创建运行数据目录 {}：{e}；请检查权限或修改 [settings] data_dir",
            storage.root().display()
        );
        std::process::exit(2);
    }

    // 窗口几何存独立文件，不写回 config.toml（保留用户注释与外部编辑）。
    // 位置允许负坐标：多屏副屏在左/上方时本就是负值。
    let geom = storage::WindowGeom::load(storage.root());
    // 界面偏好同样在窗口创建前读：启动路径上不出现「UI 线程读慢盘」。
    // 文件很小，但 data_dir 可能指向响应缓慢的网络位置，读也值得放在窗口之前。
    let theme = theme::Theme::load(storage.root());
    // --mini 只看小窗槽：拿主窗口槽兜底会让首帧以 1440×900 建窗再缩小，闪一下。
    let initial = if start_mini { geom.mini } else { geom.win };
    let (w, h) = if start_mini {
        initial
            .map(|r| (r.w.clamp(320, 2000) as f32, r.h.clamp(200, 2000) as f32))
            .unwrap_or_else(|| app::mini_default_size(0.0, 0.0))
    } else {
        initial
            .map(|r| (r.w.max(400) as f32, r.h.max(300) as f32))
            .unwrap_or((1440.0, 900.0))
    };
    // 小窗启动要放宽最小尺寸，否则 builder 会把 427×240 撑回 1100×700
    let min_size = if start_mini {
        [320.0, 200.0]
    } else {
        [1100.0, 700.0]
    };
    let vb = eframe::egui::ViewportBuilder::default()
        .with_inner_size([w, h])
        .with_min_inner_size(min_size)
        .with_title("xdview — 智能摄像机");
    let options = eframe::NativeOptions {
        viewport: vb,
        ..Default::default()
    };

    eframe::run_native(
        "xdview",
        options,
        Box::new(move |cc| {
            let mut view = app::XdView::new(cc, cfg, cfg_path, storage, geom, theme);
            if start_focus {
                view.start_focus_mode(&cc.egui_ctx, start_mini);
            }
            if let Some(want) = &initial_cam {
                if !view.preselect(want) {
                    eprintln!("没找到摄像机「{want}」，仍按第一路启动");
                }
            }
            if emit_stats {
                view.enable_stats();
            }
            Ok(Box::new(view))
        }),
    )
}

/// 定位 `config.toml`。
///
/// 双击 exe 启动时 CWD 是 `target\release\`，读不到项目根目录那份真实配置，
/// 会落到 load_or_create 的示例配置上（用户看到的是「门口/客厅」而不是自己调好的设备）。
/// 这里按「当前目录 → exe 同目录 → exe 上两级（target\release → 项目根）」找已存在的配置。
fn resolve_config_path(want: &Path) -> PathBuf {
    if want.exists() {
        return want.to_path_buf();
    }
    if want.is_relative() {
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                for cand in [dir.join(want), dir.join("..").join("..").join(want)] {
                    if cand.exists() {
                        return cand;
                    }
                }
            }
        }
    }
    want.to_path_buf()
}

/// 读配置；不存在就落一份示例到磁盘并直接用它。
fn load_or_create(path: &Path) -> Result<xd_core::config::AppConfig, String> {
    if path.exists() {
        return xd_core::config::load(path);
    }
    let _ = std::fs::write(path, xd_core::config::EXAMPLE);
    xd_core::config::parse(xd_core::config::EXAMPLE)
        .map_err(|e| format!("内置示例配置解析失败：{e}"))
}
