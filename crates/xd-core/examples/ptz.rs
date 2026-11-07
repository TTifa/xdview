//! ONVIF 云台调试工具：列预置位、执行一次小幅转动、或跳到某个预置位。
//!
//! 用法：
//! ```text
//! cargo run -p xd-core --example ptz -- <base> <user> <pass> list
//! cargo run -p xd-core --example ptz -- <base> <user> <pass> move 0.2 0 [0.5]
//! cargo run -p xd-core --example ptz -- <base> <user> <pass> zoom 0.5
//! cargo run -p xd-core --example ptz -- <base> <user> <pass> goto 客厅1
//! cargo run -p xd-core --example ptz -- <base> <user> <pass> set 门口全景
//! cargo run -p xd-core --example ptz -- <base> <user> <pass> rm 门口全景
//! cargo run -p xd-core --example ptz -- <base> <user> <pass> status
//! cargo run -p xd-core --example ptz -- <base> <user> <pass> verify
//! ```
//!
//! `verify` 是**不依赖画面**的云台验证：读 `GetStatus` 的归一化角度 → 转一下 → 再读，
//! 用角度变化判定电机是否响应；随后同样方式验证变焦（Zoom 位置）。设备开了「智能追踪」也能用。
//!
//! 存在的意义：GUI 上按方向键不好自动化，排查「云台到底动没动」时这条命令最快。

use std::time::Duration;
use xd_core::onvif::{Credentials, OnvifClient, PtzStatus, Velocity};
use xd_core::{diagnostic_eprintln as eprintln, diagnostic_println as println};

/// 发一次 ContinuousMove、保持 `hold` 毫秒，然后读状态 —— **任何退出路径都会尝试 Stop**。
///
/// 直接写 `continuous_move()?; sleep(); get_status()?; stop()?` 会在读状态失败时提前返回，
/// 把 Stop 整个跳过，镜头就一直转下去了。租约（`Timeout`）取 `hold + 500ms`：
/// 即使后面读状态卡住，设备侧也会自己停。
fn move_hold_stop(
    client: &OnvifClient,
    profile: &str,
    v: Velocity,
    hold: Duration,
) -> Result<PtzStatus, String> {
    client.continuous_move(profile, v, Some(hold + Duration::from_millis(500)))?;
    std::thread::sleep(hold);
    let status = client.get_status(profile);
    // 无论上面读没读成功都要停：这是"移动"的收尾，不是可选步骤
    if let Err(e) = client.stop(profile) {
        eprintln!("[警告] Stop 失败：{e}（镜头可能仍在移动，请手动复位）");
    }
    status
}

fn main() -> Result<(), String> {
    run().map_err(|e| xd_core::onvif::mask_url_in_text(&e))
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 4 {
        return Err(
            "用法: ptz <base> <user> <pass> <list|status|verify|move pan tilt [zoom]|zoom v|goto 名称|set 名称|rm 名称|token>"
                .to_string(),
        );
    }
    let client = OnvifClient::new(
        args[0].clone(),
        Some(Credentials {
            username: args[1].clone(),
            password: args[2].clone(),
        }),
    );

    let info = client.device_info()?;
    println!("设备: {info}（鉴权 {}）", client.auth_kind().as_str());

    let profiles = client.get_profiles()?;
    let profile = &profiles[0].token;
    println!("使用 profile: {profile}");

    let presets = client.get_presets(profile)?;
    for p in &presets {
        println!("  预置位 {} -> {}", p.token, p.name);
    }

    match args[3].as_str() {
        "list" => {}
        "status" => {
            let st = client.get_status(profile)?;
            println!(
                "当前位置 pan={:+.4} tilt={:+.4} zoom={:+.4} 移动中={}",
                st.pan, st.tilt, st.zoom, st.moving
            );
        }
        "verify" => {
            let before = client.get_status(profile)?;
            println!("转动前 pan={:+.4} tilt={:+.4}", before.pan, before.tilt);

            println!("下发 ContinuousMove pan=0.8，持续 900ms …");
            let during = move_hold_stop(
                &client,
                profile,
                Velocity {
                    pan: 0.8,
                    tilt: 0.0,
                    zoom: 0.0,
                },
                Duration::from_millis(900),
            )?;
            println!(
                "转动中 pan={:+.4} tilt={:+.4} 移动中={}",
                during.pan, during.tilt, during.moving
            );
            std::thread::sleep(Duration::from_millis(600));
            let after = client.get_status(profile)?;
            println!("停止后 pan={:+.4} tilt={:+.4}", after.pan, after.tilt);

            let moved = (during.pan - before.pan).abs();
            println!("pan 变化量 = {moved:.4}");
            if moved > 0.01 {
                println!("结论：✅ 云台确实响应了（角度发生变化）");
            } else {
                println!("结论：❌ 角度没变，云台可能未响应或该 profile 不支持 PTZ");
            }

            // ---- 变焦验证：读 Zoom 位置 → zoom+ → 再读 ----
            println!();
            println!("验证变焦：zoom=0.8，持续 900ms …");
            let z0 = client.get_status(profile)?.zoom;
            let z1 = move_hold_stop(
                &client,
                profile,
                Velocity {
                    pan: 0.0,
                    tilt: 0.0,
                    zoom: 0.8,
                },
                Duration::from_millis(900),
            )?
            .zoom;
            std::thread::sleep(Duration::from_millis(300));
            let z2 = client.get_status(profile)?.zoom;
            println!("zoom 前={z0:+.4} 动中={z1:+.4} 停后={z2:+.4}");
            let dz = (z1 - z0).abs().max((z2 - z0).abs());
            if dz > 0.01 {
                println!("结论：✅ 变焦响应了（Zoom 位置变化 {dz:.4}）");
            } else {
                println!("结论：❌ Zoom 位置没变——设备可能无电动变焦（定焦镜头），或该 profile 不支持 Zoom");
            }
        }
        "zoom" => {
            let z: f32 = args
                .get(4)
                .and_then(|s| s.parse().ok())
                .ok_or("zoom 需要 -1.0..=1.0 的数值")?;
            println!("变焦 zoom={z} …");
            let st = move_hold_stop(
                &client,
                profile,
                Velocity {
                    pan: 0.0,
                    tilt: 0.0,
                    zoom: z,
                },
                Duration::from_millis(600),
            )?;
            println!("已停止，当前位置 zoom={:+.4}", st.zoom);
        }
        "set" => {
            let name = args.get(4).ok_or("set 需要预置位名称")?;
            let token = client.set_preset(profile, name, None)?;
            println!("已保存预置位「{name}」→ token {token}");
        }
        "rm" => {
            let want = args.get(4).ok_or("rm 需要预置位名称或 token")?;
            let target = presets
                .iter()
                .find(|p| &p.name == want || &p.token == want)
                .ok_or_else(|| format!("没有名为「{want}」的预置位"))?;
            client.remove_preset(profile, &target.token)?;
            println!("已删除预置位 {}（{}）", target.name, target.token);
        }
        "move" => {
            let pan: f32 = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(0.0);
            let tilt: f32 = args.get(5).and_then(|s| s.parse().ok()).unwrap_or(0.0);
            let zoom: f32 = args.get(6).and_then(|s| s.parse().ok()).unwrap_or(0.0);
            println!("转动 pan={pan} tilt={tilt} zoom={zoom} …");
            // 只动 400ms 就停，够判断电机是否响应，又不会把镜头甩飞
            move_hold_stop(
                &client,
                profile,
                Velocity { pan, tilt, zoom },
                Duration::from_millis(400),
            )?;
            println!("已停止");
        }
        "goto" => {
            let want = args.get(4).ok_or("goto 需要预置位名称或 token")?;
            let target = presets
                .iter()
                .find(|p| &p.name == want || &p.token == want)
                .ok_or_else(|| format!("没有名为「{want}」的预置位"))?;
            println!("跳到预置位 {}（{}）", target.name, target.token);
            client.goto_preset(profile, &target.token)?;
            println!("完成");
        }
        other => return Err(format!("未知动作：{other}")),
    }
    Ok(())
}
