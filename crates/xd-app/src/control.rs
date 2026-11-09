//! ONVIF 云台控制与设备发现的后台线程。
//!
//! 这些操作都是阻塞的（HTTP 超时 5s、组播发现 2s），绝不能放在 UI 线程里跑，
//! 否则一次网络抖动整个界面就卡死。这里用「命令通道 + 结果通道」的经典结构解耦。
//!
//! # 会话隔离
//!
//! 命令与回执都带 [`Session`]（设备 ID + 连接代次）。切换设备或断开时调用方用
//! [`ControlLink::revoke`] 递增代次，在途的旧回执会被 UI 丢弃 —— 否则先连 A 再切到 B 时，
//! A 的 `GetStreamUri` 会被写进 B 的配置、用 B 的账号补凭据并起播，画面和账号就串了。
//!
//! # 抢占通道
//!
//! `Stop` / `Disconnect` 走单独的优先通道，并且在扫描期间也会被处理。扫描最长 10 秒，
//! 排在它后面的 Stop 会让「松开方向键 → 停止」延迟到扫描结束，镜头就一直转。
//!
//! # 运动租约
//!
//! `ContinuousMove` 一律带 [`MOVE_LEASE`]（设备侧 `Timeout`），按住方向键时由 UI 按
//! [`LEASE_RENEW`] 续租。这样即使 UI 崩溃、进程被杀或 Stop 丢失，镜头最多多转一个租约
//! 就自己停下，不会无限转下去。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use xd_core::discovery::{self, discover, DiscoveredDevice};
use xd_core::onvif::{Credentials, OnvifClient, Preset, Profile, Velocity};

/// 单次 `ContinuousMove` 的运动租约（下发到设备的 `Timeout`）。
pub const MOVE_LEASE: Duration = Duration::from_secs(2);

/// 续租间隔。必须明显小于 [`MOVE_LEASE`]，否则会「停一下再启动」产生顿挫。
pub const LEASE_RENEW: Duration = Duration::from_millis(800);

/// 两个命令通道都空时的阻塞上限，决定抢占命令的最大响应延迟。
const IDLE_POLL: Duration = Duration::from_millis(50);

/// 命令/回执所属的设备会话：`camera_id` 区分设备，`generation` 区分同一设备的多次连接。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub camera_id: String,
    /// 连接代次。切换设备或断开时递增，见 [`ControlLink::revoke`]。
    pub generation: u64,
}

#[derive(Debug)]
pub enum Command {
    /// 组播扫描局域网内的 ONVIF 设备（与具体设备会话无关）
    Discover {
        timeout_ms: u64,
    },
    Connect {
        session: Session,
        base: String,
        credentials: Option<Credentials>,
        /// 连上后顺便用 GetStreamUri 把 RTSP 地址要回来（省得手抄厂商私有路径）
        resolve_stream: bool,
    },
    Move(Session, Velocity),
    /// 停止云台。
    ///
    /// 走抢占通道，且**刻意不带会话**：它必须在任何情况下生效，包括会话已作废时 ——
    /// 会话作废往往意味着 UI 已经切走或断开，那时更该把镜头停下，这正是兜底的意义。
    Stop,
    /// 退出专用确认：只有这条 Stop 实际执行完才置位。
    StopForExit(Arc<AtomicBool>),
    GotoPreset(Session, String),
    /// 在当前位置保存预置位（参数为名称）
    SetPreset(Session, String),
    /// 删除预置位（参数为 PresetToken）
    RemovePreset(Session, String),
    RefreshPresets(Session),
    /// 断开并先停止云台。同样走抢占通道、同样不绑会话（理由同 [`Command::Stop`]）。
    Disconnect,
}

#[derive(Debug)]
pub enum Feedback {
    /// 扫描结果，与具体设备无关
    Devices(Vec<DiscoveredDevice>),
    Connected {
        session: Session,
        device_info: String,
        profiles: Vec<Profile>,
    },
    StreamUri {
        session: Session,
        uri: String,
    },
    Presets(Session, Vec<Preset>),
    /// 成功类的异步回执（如预置位保存/删除）
    Notice(Session, String),
    /// 连接失败：设备在列表里，但 ONVIF 鉴权或取流地址失败。
    /// 与 [`Self::Error`] 区分开，UI 才能把「连接中」改成失败态。
    ConnectFailed {
        session: Session,
        /// 原连接请求是否依赖 ONVIF 获取媒体地址；为 false 时失败只影响控制能力。
        resolve_stream: bool,
        message: String,
    },
    /// 扫描失败（与具体设备无关）。UI 据此结束「扫描中」并显示原因。
    ScanFailed {
        message: String,
    },
    /// 云台等设备命令出错：只记日志，不影响连接状态。
    Error {
        session: Option<Session>,
        message: String,
    },
}

impl Feedback {
    /// 回执所属会话。`None` 表示全局回执（扫描结果/扫描失败），总是有效。
    pub fn session(&self) -> Option<&Session> {
        match self {
            Feedback::Devices(_)
            | Feedback::ScanFailed { .. }
            | Feedback::Error { session: None, .. } => None,
            Feedback::Connected { session, .. }
            | Feedback::StreamUri { session, .. }
            | Feedback::ConnectFailed { session, .. }
            | Feedback::Presets(session, _)
            | Feedback::Notice(session, _) => Some(session),
            Feedback::Error {
                session: Some(s), ..
            } => Some(s),
        }
    }
}

pub struct ControlLink {
    cmd: Sender<Command>,
    /// 抢占通道：`Stop` / `Disconnect` 走这里，避免排在长任务后面。
    urgent: Sender<Command>,
    rx: Receiver<Feedback>,
    generation: u64,
}

impl ControlLink {
    pub fn new() -> ControlLink {
        let (cmd_tx, cmd_rx) = channel::<Command>();
        let (urgent_tx, urgent_rx) = channel::<Command>();
        let (out_tx, out_rx) = channel::<Feedback>();
        thread::Builder::new()
            .name("onvif".to_string())
            .spawn(move || worker(cmd_rx, urgent_rx, out_tx))
            .expect("无法创建 ONVIF 线程");

        ControlLink {
            cmd: cmd_tx,
            urgent: urgent_tx,
            rx: out_rx,
            generation: 0,
        }
    }

    /// 下发退出用 Stop，返回独立回执；UI 后续帧查询，不等待网络。
    pub fn request_stop(&self) -> Arc<AtomicBool> {
        let done = Arc::new(AtomicBool::new(false));
        self.send(Command::StopForExit(Arc::clone(&done)));
        done
    }

    /// 让在途命令与回执全部作废，返回新会话的标识。
    ///
    /// 切换设备、断开连接时必须调用：否则旧设备上迟到的 `StreamUri` / `Presets`
    /// 会被当成新设备的结果写进配置并起播。
    pub fn revoke(&mut self, camera_id: impl Into<String>) -> Session {
        self.generation = self.generation.wrapping_add(1);
        Session {
            camera_id: camera_id.into(),
            generation: self.generation,
        }
    }

    /// 当前会话标识（不改变代次）。
    pub fn session(&self, camera_id: impl Into<String>) -> Session {
        Session {
            camera_id: camera_id.into(),
            generation: self.generation,
        }
    }

    pub fn send(&self, cmd: Command) {
        // 停止/断开必须能插到扫描、连接前面，否则松开方向键后镜头还会继续转
        let tx = match &cmd {
            Command::Stop | Command::StopForExit(_) | Command::Disconnect => &self.urgent,
            _ => &self.cmd,
        };
        let _ = tx.send(cmd);
    }

    pub fn try_recv(&self) -> Option<Feedback> {
        self.rx.try_recv().ok()
    }
}

/// worker 的可变状态：一次只维护一个 ONVIF 会话。
struct Worker {
    client: Option<OnvifClient>,
    profile: Option<String>,
    active: Option<Session>,
}

impl Worker {
    /// 命令是否属于当前会话。过期命令一律丢弃。
    fn matches(&self, session: &Session) -> bool {
        self.active.as_ref() == Some(session)
    }

    /// 尽力停止当前会话的云台；退出确认由独立 StopForExit 命令负责。
    fn stop(&self) {
        if let (Some(c), Some(p)) = (self.client.as_ref(), self.profile.as_deref()) {
            let _ = c.stop(p);
        }
    }

    /// 断开：先尝试 Stop（断开不等于希望镜头继续转），再清空会话。
    fn teardown(&mut self) {
        self.stop();
        self.clear();
    }

    /// 清空会话但不发 Stop（连接失败等还没有运动发生的场合）。
    fn clear(&mut self) {
        self.client = None;
        self.profile = None;
        self.active = None;
    }
}

enum Next {
    Cmd(Command),
    Idle,
    Closed,
}

fn worker(cmd_rx: Receiver<Command>, urgent_rx: Receiver<Command>, tx: Sender<Feedback>) {
    let mut w = Worker {
        client: None,
        profile: None,
        active: None,
    };

    loop {
        match next_command(&cmd_rx, &urgent_rx) {
            Next::Cmd(cmd) => w.apply(cmd, &tx, &urgent_rx),
            Next::Idle => continue,
            Next::Closed => break,
        }
    }
}

/// 取下一条命令：抢占通道优先；两个通道都空时最多阻塞 [`IDLE_POLL`]。
fn next_command(cmd_rx: &Receiver<Command>, urgent_rx: &Receiver<Command>) -> Next {
    if let Ok(c) = urgent_rx.try_recv() {
        return Next::Cmd(c);
    }
    match cmd_rx.try_recv() {
        Ok(c) => return Next::Cmd(c),
        Err(TryRecvError::Disconnected) => return Next::Closed,
        Err(TryRecvError::Empty) => {}
    }
    match urgent_rx.recv_timeout(IDLE_POLL) {
        Ok(c) => Next::Cmd(c),
        Err(RecvTimeoutError::Timeout) => Next::Idle,
        Err(RecvTimeoutError::Disconnected) => Next::Closed,
    }
}

impl Worker {
    fn apply(&mut self, cmd: Command, tx: &Sender<Feedback>, urgent: &Receiver<Command>) {
        match cmd {
            Command::Discover { timeout_ms } => {
                let handle = thread::spawn(move || run_discover(timeout_ms));
                // 扫描最长 10 秒；期间持续响应抢占命令，否则「松开方向键」要等扫描结束
                while !handle.is_finished() {
                    if let Ok(cmd) = urgent.recv_timeout(IDLE_POLL) {
                        self.apply_urgent(cmd);
                    }
                }
                match handle.join() {
                    Ok(Ok(list)) => {
                        let _ = tx.send(Feedback::Devices(list));
                    }
                    Ok(Err(())) => {
                        let _ = tx.send(Feedback::ScanFailed {
                            message: "扫描失败：WS-Discovery 无应答，网段探测也未命中".into(),
                        });
                    }
                    Err(_) => {
                        let _ = tx.send(Feedback::ScanFailed {
                            message: "扫描线程异常退出".into(),
                        });
                    }
                }
            }
            Command::Connect {
                session,
                base,
                credentials,
                resolve_stream,
            } => {
                let c = OnvifClient::new(base, credentials);
                let info = c
                    .device_info()
                    .unwrap_or_else(|e| format!("(设备信息读取失败: {e})"));
                match c.get_profiles() {
                    Ok(profiles) => {
                        self.active = Some(session.clone());
                        self.profile = profiles.first().map(|p| p.token.clone());
                        if resolve_stream {
                            if let Some(p) = self.profile.clone() {
                                match c.get_stream_uri(&p) {
                                    Ok(uri) => {
                                        let _ = tx.send(Feedback::StreamUri {
                                            session: session.clone(),
                                            uri,
                                        });
                                    }
                                    Err(e) => {
                                        let _ = tx.send(Feedback::ConnectFailed {
                                            session: session.clone(),
                                            resolve_stream,
                                            message: format!("取流地址失败: {e}"),
                                        });
                                    }
                                }
                            }
                        }
                        self.client = Some(c);
                        let _ = tx.send(Feedback::Connected {
                            session,
                            device_info: info,
                            profiles,
                        });
                    }
                    Err(e) => {
                        self.clear();
                        let _ = tx.send(Feedback::ConnectFailed {
                            session,
                            resolve_stream,
                            message: format!("ONVIF 连接失败: {e}"),
                        });
                    }
                }
            }
            Command::Move(session, v) => {
                if !self.matches(&session) {
                    return;
                }
                if let (Some(c), Some(p)) = (self.client.as_ref(), self.profile.as_deref()) {
                    // 带租约下发：设备侧到期自动停，UI 按住时按 LEASE_RENEW 续租。
                    // 不带 Timeout 时，一旦 Stop 没送到（网络异常/进程退出）镜头会一直转。
                    if let Err(e) = c.continuous_move(p, v, Some(MOVE_LEASE)) {
                        let _ = tx.send(Feedback::Error {
                            session: Some(session),
                            message: e,
                        });
                    }
                }
            }
            // Stop 无条件执行：会话过期说明 UI 已切走或断开，此时更该把镜头停下
            Command::Stop => self.stop(),
            Command::StopForExit(done) => {
                self.stop();
                done.store(true, Ordering::Release);
            }
            Command::GotoPreset(session, token) => {
                if !self.matches(&session) {
                    return;
                }
                if let (Some(c), Some(p)) = (self.client.as_ref(), self.profile.as_deref()) {
                    if let Err(e) = c.goto_preset(p, &token) {
                        let _ = tx.send(Feedback::Error {
                            session: Some(session),
                            message: e,
                        });
                    }
                }
            }
            Command::SetPreset(session, name) => {
                if !self.matches(&session) {
                    return;
                }
                if let (Some(c), Some(p)) = (self.client.as_ref(), self.profile.as_deref()) {
                    match c.set_preset(p, &name, None) {
                        Ok(token) => {
                            let _ = tx.send(Feedback::Notice(
                                session.clone(),
                                format!("已保存预置位「{name}」（token {token}）"),
                            ));
                            if let Ok(list) = c.get_presets(p) {
                                let _ = tx.send(Feedback::Presets(session, list));
                            }
                        }
                        Err(e) => {
                            let _ = tx.send(Feedback::Error {
                                session: Some(session),
                                message: format!("保存预置位失败: {e}"),
                            });
                        }
                    }
                }
            }
            Command::RemovePreset(session, token) => {
                if !self.matches(&session) {
                    return;
                }
                if let (Some(c), Some(p)) = (self.client.as_ref(), self.profile.as_deref()) {
                    match c.remove_preset(p, &token) {
                        Ok(()) => {
                            let _ =
                                tx.send(Feedback::Notice(session.clone(), "已删除预置位".into()));
                            if let Ok(list) = c.get_presets(p) {
                                let _ = tx.send(Feedback::Presets(session, list));
                            }
                        }
                        Err(e) => {
                            let _ = tx.send(Feedback::Error {
                                session: Some(session),
                                message: format!("删除预置位失败: {e}"),
                            });
                        }
                    }
                }
            }
            Command::RefreshPresets(session) => {
                if !self.matches(&session) {
                    return;
                }
                if let (Some(c), Some(p)) = (self.client.as_ref(), self.profile.as_deref()) {
                    match c.get_presets(p) {
                        Ok(list) => {
                            let _ = tx.send(Feedback::Presets(session, list));
                        }
                        Err(e) => {
                            let _ = tx.send(Feedback::Error {
                                session: Some(session),
                                message: format!("读取预置位失败: {e}"),
                            });
                        }
                    }
                }
            }
            // 断开：先 Stop 再清空，避免「断开后镜头还在转」
            Command::Disconnect => self.teardown(),
        }
    }

    /// 只处理抢占命令（其余命令理论上不会走优先通道，遇到就忽略）。
    fn apply_urgent(&mut self, cmd: Command) {
        match cmd {
            Command::Stop => self.stop(),
            Command::StopForExit(done) => {
                self.stop();
                done.store(true, Ordering::Release);
            }
            Command::Disconnect => self.teardown(),
            _ => {}
        }
    }
}

/// WS-Discovery 与网段探测并行：前者是标准协议；后者兜底不实现 WS-Discovery 的设备
/// （小度/TP-Link 等，见 docs 接入指引）。`Err(())` 表示两种方式都没命中。
fn run_discover(timeout_ms: u64) -> Result<Vec<DiscoveredDevice>, ()> {
    let t = Duration::from_millis(timeout_ms.clamp(300, 10_000));
    let ws = thread::spawn(move || discover(t));
    let probe = thread::spawn(|| discovery::scan_lan(discovery::PROBE_PORTS));
    let ws_res = ws.join().ok().and_then(|r| r.ok());
    let ws_failed = ws_res.is_none();
    let mut list = ws_res.unwrap_or_default();
    if let Ok(extra) = probe.join() {
        for d in extra {
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
    if list.is_empty() && ws_failed {
        Err(())
    } else {
        Ok(list)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 回执的会话归属决定 UI 是否丢弃：连接失败必须带会话（否则迟到回执会串台），
    /// 扫描失败必须为 None（否则会被当作别的设备的回执直接丢掉）。
    #[test]
    fn feedback_session_routing() {
        let s = Session {
            camera_id: "cam-a".into(),
            generation: 1,
        };
        assert_eq!(
            Feedback::ConnectFailed {
                session: s.clone(),
                resolve_stream: true,
                message: "ONVIF 连接失败".into()
            }
            .session(),
            Some(&s)
        );
        assert_eq!(
            Feedback::ScanFailed {
                message: "x".into()
            }
            .session(),
            None
        );
        assert_eq!(Feedback::Devices(Vec::new()).session(), None);
    }

    #[test]
    fn revoke_bumps_generation() {
        let mut link = ControlLink::new();
        let first = link.revoke("cam-a");
        let second = link.revoke("cam-a");
        assert_eq!(first.generation + 1, second.generation);
        assert_ne!(first, second, "同设备的两次连接必须能区分");
    }

    #[test]
    fn delayed_socket_feedback_from_first_a_is_stale_after_a_b_a_and_delete() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::sync::mpsc;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (arrived_tx, arrived_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut request = Vec::new();
            let mut buf = [0; 1024];
            loop {
                let n = socket.read(&mut buf).unwrap();
                assert_ne!(n, 0);
                request.extend_from_slice(&buf[..n]);
                if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    let header = String::from_utf8_lossy(&request[..end]);
                    let length = header
                        .lines()
                        .find_map(|line| {
                            let (key, value) = line.split_once(':')?;
                            key.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().ok())
                                .flatten()
                        })
                        .unwrap();
                    if request.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            arrived_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(3)).unwrap();
            drop(listener);
            socket
                .write_all(b"HTTP/1.1 500 Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .unwrap();
            socket.shutdown(std::net::Shutdown::Write).unwrap();
        });
        let mut link = ControlLink::new();
        let first = link.revoke("a");
        link.send(Command::Connect {
            session: first.clone(),
            base: format!("http://{address}"),
            credentials: None,
            resolve_stream: true,
        });
        arrived_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        let _b = link.revoke("b");
        let current = link.revoke("a");
        release_tx.send(()).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let feedback = loop {
            if let Some(feedback) = link.try_recv() {
                break feedback;
            }
            assert!(std::time::Instant::now() < deadline, "未收到本地桩回执");
            thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(feedback.session(), Some(&first));
        assert_ne!(feedback.session(), Some(&current));
        let deleted = link.revoke("");
        assert_ne!(feedback.session(), Some(&deleted));
        server.join().unwrap();
    }

    #[test]
    fn session_differs_by_camera() {
        let link = ControlLink::new();
        assert_ne!(link.session("cam-a"), link.session("cam-b"));
    }

    #[test]
    fn global_feedback_has_no_session() {
        assert!(Feedback::Devices(Vec::new()).session().is_none());
        assert!(Feedback::Error {
            session: None,
            message: "x".into()
        }
        .session()
        .is_none());
    }

    #[test]
    fn stale_move_is_dropped() {
        let mut w = Worker {
            client: None,
            profile: None,
            active: None,
        };
        let (tx, rx) = channel::<Feedback>();
        let (_utx, urx) = channel::<Command>();
        let current = Session {
            camera_id: "a".into(),
            generation: 1,
        };
        w.active = Some(current.clone());
        // 过期会话（generation 更旧）的命令不应产生任何回执
        let stale = Session {
            camera_id: "a".into(),
            generation: 0,
        };
        w.apply(Command::Move(stale, Velocity::default()), &tx, &urx);
        w.apply(Command::GotoPreset(current, "t".into()), &tx, &urx);
        assert!(rx.try_recv().is_err(), "过期/无客户端的命令不应产生回执");
    }
}
