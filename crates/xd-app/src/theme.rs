//! 深色 / 浅色主题：色板、egui 视觉与偏好持久化。
//!
//! 色值取自 `docs/designs/xdview/XDView 交互原型.html` 的 `:root`（夜间）与
//! `body.light`（日间）两组变量；派生色（按钮悬停、半透明描边）按原型在同底色上
//! 叠 alpha 的结果换算成实色，避免依赖合成顺序。
//!
//! 主题属于界面偏好，和窗口几何一样写在运行数据目录的 `ui_prefs.toml`，
//! 不覆写 `config.toml`（那里留着用户注释与外部编辑）。
//!
//! 视频画面层始终是深色：无论哪套主题，`stage*` 与 `on_stage_text` 都按深底
//! 可读性取值，避免浅色主题把浮层文字压成低对比。

use eframe::egui;
use std::io::Read;
use std::path::{Path, PathBuf};

const MAX_PREFS_BYTES: u64 = 4096;

/// 界面主题。默认夜间（与原型初始态一致）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Theme {
    #[default]
    Dark,
    Light,
}

impl Theme {
    pub fn is_light(self) -> bool {
        matches!(self, Self::Light)
    }

    /// 写进 `ui_prefs.toml` 的稳定取值。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Dark => "dark",
            Self::Light => "light",
        }
    }

    /// 接受 `dark` / `night` / `light` / `day`，其余一律视为未设置。
    fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "dark" | "night" => Some(Self::Dark),
            "light" | "day" => Some(Self::Light),
            _ => None,
        }
    }

    /// 切换时写进日志的中文说明。
    pub fn label(self) -> &'static str {
        match self {
            Self::Dark => "夜间模式（深色皮肤）",
            Self::Light => "日间模式（浅色皮肤）",
        }
    }

    pub fn toggle(self) -> Self {
        match self {
            Self::Dark => Self::Light,
            Self::Light => Self::Dark,
        }
    }

    /// 读取不超过 4 KiB 的 UTF-8 偏好；缺失、损坏或超限时退回夜间。
    /// 仅支持单个 `theme = "..."`、空行和 `#` 注释，可带 UTF-8 BOM。
    pub fn load(root: &Path) -> Self {
        let Ok(file) = std::fs::File::open(prefs_path(root)) else {
            return Self::default();
        };
        let mut bytes = Vec::new();
        if file
            .take(MAX_PREFS_BYTES + 1)
            .read_to_end(&mut bytes)
            .is_err()
            || bytes.len() as u64 > MAX_PREFS_BYTES
        {
            return Self::default();
        }
        let Ok(text) = std::str::from_utf8(&bytes) else {
            return Self::default();
        };
        Self::parse_preferences(text).unwrap_or_default()
    }

    fn parse_preferences(text: &str) -> Option<Self> {
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let mut theme = None;
        for line in text.lines() {
            let line = line
                .split_once('#')
                .map_or(line, |(before, _)| before)
                .trim();
            if line.is_empty() {
                continue;
            }
            let (key, value) = line.split_once('=')?;
            if key.trim() != "theme" || theme.is_some() {
                return None;
            }
            let value = value.trim().strip_prefix('"')?.strip_suffix('"')?;
            theme = Some(Self::parse(value)?);
        }
        theme
    }

    /// 只写这一行偏好；失败由调用方决定怎么提示（主题已生效，不必中断）。
    pub fn save(self, root: &Path) -> Result<(), String> {
        let path = prefs_path(root);
        let text = format!(
            "# xdview 界面偏好（程序写回；删除本文件恢复默认夜间模式）\ntheme = \"{}\"\n",
            self.as_str()
        );
        // 与窗口几何同一套原子写法：先写 .tmp 再 rename，被杀时不留半截文件。
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text).map_err(|e| format!("保存界面偏好失败：{e}"))?;
        std::fs::rename(&tmp, &path).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            format!("保存界面偏好失败：{e}")
        })
    }
}

fn prefs_path(root: &Path) -> PathBuf {
    root.join("ui_prefs.toml")
}

/// 原型的两套色板，字段语义与 CSS 变量一一对应（外加派生的按钮/浮层色）。
///
/// `Copy`：所有字段都是 `Color32`，按值传递比借用更省心，`self.pal` 读取不会
/// 和同一段里的 `&mut self` 调用打架。
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    /// 是否为浅色皮肤（决定 egui 基础视觉用 light 还是 dark）。
    pub light: bool,
    pub bg: egui::Color32,
    pub panel: egui::Color32,
    pub panel2: egui::Color32,
    pub inset: egui::Color32,
    pub border: egui::Color32,
    pub border_strong: egui::Color32,
    pub text: egui::Color32,
    pub dim: egui::Color32,
    pub faint: egui::Color32,
    pub ok: egui::Color32,
    pub warn: egui::Color32,
    pub bad: egui::Color32,
    pub info: egui::Color32,
    /// 日志正文（比 `text` 稍柔和）
    pub msg: egui::Color32,
    /// 选中设备卡片的底色
    pub active_bg: egui::Color32,
    /// 强调态按钮底色（专注 / 添加设备 / 分段「开」）
    pub accent_fill: egui::Color32,
    /// 强调态描边
    pub accent_stroke: egui::Color32,
    pub danger_fill: egui::Color32,
    pub danger_stroke: egui::Color32,
    pub warn_fill: egui::Color32,
    pub warn_stroke: egui::Color32,
    /// 控件悬停底色
    pub hover_fill: egui::Color32,
    pub toast_fill: egui::Color32,
    /// 分栏拖拽时的提示线
    pub link: egui::Color32,
    /// 视频画面层底色（两种主题都用深色）
    pub stage: egui::Color32,
    /// 画面层上的次要文字
    pub stage_dim: egui::Color32,
    /// 画面层上的提示文字
    pub stage_faint: egui::Color32,
    /// 画面层上的悬浮按钮文字（原型 `.focus-exit` 用纯白，两套主题一致）
    pub on_stage_text: egui::Color32,
    /// 画面层上的高亮文字（悬浮按钮激活态）
    pub stage_ok: egui::Color32,
    /// 画面层上的描边（连接遮罩转圈、悬浮按钮边框）
    pub stage_border: egui::Color32,
}

impl Palette {
    pub fn of(theme: Theme) -> Self {
        match theme {
            Theme::Dark => Self::dark(),
            Theme::Light => Self::light(),
        }
    }

    /// 夜间：与原型 `:root` 完全一致。
    pub fn dark() -> Self {
        Self {
            light: false,
            bg: egui::Color32::from_rgb(9, 10, 12),
            panel: egui::Color32::from_rgb(16, 19, 24),
            panel2: egui::Color32::from_rgb(21, 26, 34),
            inset: egui::Color32::from_rgb(12, 15, 20),
            border: egui::Color32::from_rgb(30, 36, 48),
            border_strong: egui::Color32::from_rgb(42, 50, 66),
            text: egui::Color32::from_rgb(230, 233, 239),
            dim: egui::Color32::from_rgb(138, 146, 163),
            faint: egui::Color32::from_rgb(86, 94, 110),
            ok: egui::Color32::from_rgb(86, 200, 140),
            warn: egui::Color32::from_rgb(232, 178, 74),
            bad: egui::Color32::from_rgb(226, 96, 96),
            info: egui::Color32::from_rgb(92, 168, 232),
            msg: egui::Color32::from_rgb(183, 190, 203),
            active_bg: egui::Color32::from_rgb(19, 27, 24),
            accent_fill: egui::Color32::from_rgb(26, 52, 38),
            accent_stroke: egui::Color32::from_rgb(58, 110, 84),
            danger_fill: egui::Color32::from_rgb(48, 26, 26),
            danger_stroke: egui::Color32::from_rgb(120, 60, 60),
            warn_fill: egui::Color32::from_rgb(40, 34, 20),
            warn_stroke: egui::Color32::from_rgb(120, 96, 40),
            hover_fill: egui::Color32::from_rgb(26, 32, 43),
            toast_fill: egui::Color32::from_rgb(22, 28, 38),
            link: egui::Color32::from_rgb(70, 120, 170),
            stage: egui::Color32::from_rgb(13, 16, 21),
            stage_dim: egui::Color32::from_rgb(138, 146, 163),
            stage_faint: egui::Color32::from_rgb(86, 94, 110),
            on_stage_text: egui::Color32::WHITE,
            stage_ok: egui::Color32::from_rgb(138, 224, 178),
            stage_border: egui::Color32::from_rgb(42, 50, 66),
        }
    }

    /// 日间：与原型 `body.light` 一致；强调/危险底色按同色 alpha 叠白底换算。
    pub fn light() -> Self {
        Self {
            light: true,
            bg: egui::Color32::from_rgb(242, 244, 247),
            panel: egui::Color32::from_rgb(255, 255, 255),
            panel2: egui::Color32::from_rgb(244, 246, 249),
            inset: egui::Color32::from_rgb(237, 240, 244),
            border: egui::Color32::from_rgb(220, 226, 234),
            border_strong: egui::Color32::from_rgb(195, 204, 216),
            text: egui::Color32::from_rgb(28, 37, 52),
            dim: egui::Color32::from_rgb(93, 103, 118),
            faint: egui::Color32::from_rgb(154, 163, 178),
            ok: egui::Color32::from_rgb(30, 158, 99),
            warn: egui::Color32::from_rgb(180, 122, 22),
            bad: egui::Color32::from_rgb(210, 74, 74),
            info: egui::Color32::from_rgb(46, 125, 189),
            msg: egui::Color32::from_rgb(65, 75, 92),
            active_bg: egui::Color32::from_rgb(237, 247, 241),
            accent_fill: egui::Color32::from_rgb(228, 243, 236),
            accent_stroke: egui::Color32::from_rgb(165, 216, 192),
            danger_fill: egui::Color32::from_rgb(253, 242, 242),
            danger_stroke: egui::Color32::from_rgb(242, 184, 184),
            warn_fill: egui::Color32::from_rgb(246, 239, 227),
            warn_stroke: egui::Color32::from_rgb(225, 202, 162),
            hover_fill: egui::Color32::from_rgb(233, 237, 243),
            toast_fill: egui::Color32::from_rgb(255, 255, 255),
            link: egui::Color32::from_rgb(46, 125, 189),
            stage: egui::Color32::from_rgb(13, 16, 21),
            stage_dim: egui::Color32::from_rgb(183, 190, 203),
            stage_faint: egui::Color32::from_rgb(138, 146, 163),
            on_stage_text: egui::Color32::WHITE,
            stage_ok: egui::Color32::from_rgb(138, 224, 178),
            stage_border: egui::Color32::from_rgb(42, 50, 66),
        }
    }
}

/// 由色板生成 egui 视觉：控件底色/描边/文字全部走主题，深色下与旧实现逐项等价。
pub fn build_visuals(pal: &Palette) -> egui::Visuals {
    let mut v = if pal.light {
        egui::Visuals::light()
    } else {
        egui::Visuals::dark()
    };
    v.panel_fill = pal.panel;
    v.window_fill = pal.panel;
    v.window_stroke = egui::Stroke::new(1.0_f32, pal.border_strong);
    v.extreme_bg_color = pal.inset;
    v.faint_bg_color = pal.panel2;
    v.override_text_color = Some(pal.text);
    v.hyperlink_color = pal.info;
    v.selection.bg_fill = pal.accent_fill;
    v.selection.stroke = egui::Stroke::new(1.0_f32, pal.ok);
    v.window_corner_radius = egui::CornerRadius::same(8);
    v.widgets.noninteractive.bg_fill = pal.panel2;
    v.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0_f32, pal.border);
    v.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0_f32, pal.dim);
    v.widgets.noninteractive.corner_radius = egui::CornerRadius::same(6);
    v.widgets.inactive.bg_fill = pal.panel2;
    v.widgets.inactive.weak_bg_fill = pal.panel2;
    v.widgets.inactive.bg_stroke = egui::Stroke::new(1.0_f32, pal.border);
    v.widgets.inactive.fg_stroke = egui::Stroke::new(1.0_f32, pal.text);
    v.widgets.inactive.corner_radius = egui::CornerRadius::same(6);
    v.widgets.hovered.bg_fill = pal.hover_fill;
    v.widgets.hovered.weak_bg_fill = pal.hover_fill;
    v.widgets.hovered.bg_stroke = egui::Stroke::new(1.0_f32, pal.border_strong);
    v.widgets.hovered.fg_stroke = egui::Stroke::new(1.0_f32, pal.text);
    v.widgets.hovered.corner_radius = egui::CornerRadius::same(6);
    v.widgets.active.bg_fill = pal.accent_fill;
    v.widgets.active.weak_bg_fill = pal.accent_fill;
    v.widgets.active.bg_stroke = egui::Stroke::new(1.0_f32, pal.ok);
    v.widgets.active.fg_stroke = egui::Stroke::new(1.0_f32, pal.text);
    v.widgets.active.corner_radius = egui::CornerRadius::same(6);
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("xdview-prefs-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn preference_rejects_truncated_and_malformed_input() {
        let root = temp_root("malformed");
        for text in [
            "theme = \"light",
            "theme = light\"",
            "theme = \"\"light\"\"",
            "theme = light",
            "theme = \"light\"\ntheme = \"light\"\n",
            "theme = \"light\"\nbroken\n",
            "theme = \"light\" extra\n",
        ] {
            std::fs::write(prefs_path(&root), text).unwrap();
            assert_eq!(Theme::load(&root), Theme::Dark, "应拒绝：{text:?}");
        }
        std::fs::write(prefs_path(&root), b"theme = \"light\"\n\xff").unwrap();
        assert_eq!(Theme::load(&root), Theme::Dark);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn preference_accepts_comments_bom_and_bounded_size() {
        let root = temp_root("bounded");
        std::fs::write(
            prefs_path(&root),
            "\u{feff}# 注释\r\ntheme = \"light\" # 日间\r\n",
        )
        .unwrap();
        assert_eq!(Theme::load(&root), Theme::Light);
        let prefix = "theme = \"light\"\n#";
        let at_limit = format!("{prefix}{}", "x".repeat(4096 - prefix.len()));
        std::fs::write(prefs_path(&root), &at_limit).unwrap();
        assert_eq!(Theme::load(&root), Theme::Light, "4 KiB 应可读取");
        std::fs::write(prefs_path(&root), format!("{at_limit}x")).unwrap();
        assert_eq!(Theme::load(&root), Theme::Dark, "超过上限应回退夜间");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn theme_preference_roundtrips_and_tolerates_broken_files() {
        let root = temp_root("roundtrip");
        assert_eq!(Theme::load(&root), Theme::Dark, "缺文件时用夜间");
        Theme::Light.save(&root).unwrap();
        assert_eq!(Theme::load(&root), Theme::Light);
        Theme::Dark.save(&root).unwrap();
        assert_eq!(Theme::load(&root), Theme::Dark);
        assert!(!prefs_path(&root).with_extension("toml.tmp").exists());

        std::fs::write(prefs_path(&root), "theme = \"紫色\"\n").unwrap();
        assert_eq!(Theme::load(&root), Theme::Dark, "无法识别的值退回夜间");
        std::fs::write(prefs_path(&root), "garbage\n").unwrap();
        assert_eq!(Theme::load(&root), Theme::Dark);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 目标不可写（这里用一个同名目录占位）时保存要报错，读取要退回夜间且不崩。
    #[test]
    fn save_reports_error_when_target_is_not_a_file() {
        let root = temp_root("blocked");
        std::fs::create_dir_all(prefs_path(&root)).unwrap();
        let err = Theme::Light
            .save(&root)
            .expect_err("目标不是文件时必须报错");
        assert!(
            err.contains("保存界面偏好失败"),
            "错误信息应说明是哪一步失败"
        );
        assert_eq!(Theme::load(&root), Theme::Dark, "读不到就退回夜间");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn theme_parse_accepts_documented_spellings_only() {
        assert_eq!(Theme::parse("light"), Some(Theme::Light));
        assert_eq!(Theme::parse(" Day "), Some(Theme::Light));
        assert_eq!(Theme::parse("NIGHT"), Some(Theme::Dark));
        assert_eq!(Theme::parse("auto"), None);
        assert_eq!(Theme::Dark.toggle(), Theme::Light);
        assert_eq!(Theme::Light.toggle(), Theme::Dark);
    }

    #[test]
    fn palettes_match_prototype_values() {
        // 原型 :root 与 body.light 的关键变量，改动色板时先改这里
        let dark = Palette::dark();
        assert_eq!(dark.bg, egui::Color32::from_rgb(9, 10, 12));
        assert_eq!(dark.text, egui::Color32::from_rgb(230, 233, 239));
        assert_eq!(dark.ok, egui::Color32::from_rgb(86, 200, 140));
        let light = Palette::light();
        assert_eq!(light.bg, egui::Color32::from_rgb(242, 244, 247));
        assert_eq!(light.panel, egui::Color32::WHITE);
        assert_eq!(light.text, egui::Color32::from_rgb(28, 37, 52));
        assert_eq!(light.ok, egui::Color32::from_rgb(30, 158, 99));
        assert_eq!(light.active_bg, egui::Color32::from_rgb(237, 247, 241));
    }

    /// 浅色主题必须是「亮底深字」，深色主题相反；画面层两种主题都保持深底。
    #[test]
    fn palette_keeps_text_readable_on_its_background() {
        for pal in [Palette::dark(), Palette::light()] {
            assert!(contrast(pal.text, pal.bg) >= 7.0, "正文对比度不足");
            assert!(contrast(pal.dim, pal.bg) >= 4.0, "次要文字对比度不足");
            assert!(contrast(pal.msg, pal.bg) >= 4.5, "日志正文对比度不足");
            assert!(contrast(pal.ok, pal.bg) >= 3.0, "强调色对比度不足");
            assert!(luminance(pal.stage) < 0.02, "画面层应始终是深底");
            assert!(
                contrast(pal.stage_dim, pal.stage) >= 4.5,
                "画面层次要文字不可读"
            );
            // 最弱一级提示文字沿用原型的 --faint，本身就是低对比（原型约 2.9:1）
            assert!(
                contrast(pal.stage_faint, pal.stage) >= 2.8,
                "画面层提示文字不可读"
            );
        }
        assert!(luminance(Palette::light().bg) > luminance(Palette::light().text));
        assert!(luminance(Palette::dark().bg) < luminance(Palette::dark().text));
    }

    /// WCAG 相对对比度（1..=21）。
    fn contrast(a: egui::Color32, b: egui::Color32) -> f32 {
        let (la, lb) = (luminance(a), luminance(b));
        let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
        (hi + 0.05) / (lo + 0.05)
    }

    fn luminance(c: egui::Color32) -> f32 {
        let f = |v: u8| {
            let s = f32::from(v) / 255.0;
            if s <= 0.039_28 {
                s / 12.92
            } else {
                ((s + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * f(c.r()) + 0.7152 * f(c.g()) + 0.0722 * f(c.b())
    }
}
