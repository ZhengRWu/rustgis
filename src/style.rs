//! RustGIS 全局视觉样式：白底浅色主题（对标 ArcGIS 桌面的白色窗体 + 蓝色强调色）。
//!
//! 所有"设计 token"（颜色 / 描边 / 整应用 `Visuals`）统一收敛在本模块：
//! 菜单栏、图层面板、状态栏、地图画布、对话框全部从这里取色，
//! 换主题或调配色只需改这一处。
//!
//! 主题基调：
//! - 背景全白（窗口 / 面板 / 地图画布 / 对话框）；
//! - 面板之间用 1px 发丝线（hairline）分隔，不用色块对比；
//! - 控件交互态（悬停 / 按下 / 选中）统一走蓝色系；
//! - 图层默认符号化配色按白底画布选型（深于底图、互相可辨识）。

use egui::{Color32, Shadow, Stroke};

// ─────────────────────────── 设计 token：背景 ───────────────────────────

/// 全局基底背景：纯白（窗口 / 面板 / 地图画布 / 对话框）。
pub const WHITE: Color32 = Color32::from_rgb(255, 255, 255);

/// 弱填充（控件底、滚动条底）：极浅灰，白底上若有若无。
pub const SOFT_BG: Color32 = Color32::from_rgb(243, 245, 248);

/// 发丝线分隔色（1px，面板之间 / 面板与画布之间）。
pub const BORDER: Color32 = Color32::from_rgb(222, 227, 233);

/// 控件边框（比发丝线深一档，用于窗口 / 弹窗轮廓）。
pub const BORDER_STRONG: Color32 = Color32::from_rgb(199, 207, 216);

// ─────────────────────────── 设计 token：文字 ───────────────────────────

/// 主文字色：纯黑（产品约定——文字颜色非必要，全部默认使用纯黑）。
pub const TEXT: Color32 = Color32::BLACK;

/// 次级颜色（禁用的图标笔划 / 勾选标记等图形元素）。
/// 文字不使用此色：弱文字统一走纯黑（见 `visuals` 的 `override_text_color`）。
pub const TEXT_WEAK: Color32 = Color32::from_rgb(106, 117, 133);

// ─────────────────────────── 设计 token：强调色 ───────────────────────────

/// 主题强调色（ArcGIS 蓝）：超链接、文本光标、活动控件描边。
pub const ACCENT: Color32 = Color32::from_rgb(0, 113, 188);

/// 控件悬停：浅蓝底。
pub const ACCENT_HOVER_BG: Color32 = Color32::from_rgb(233, 242, 251);
/// 控件悬停：描边。
pub const ACCENT_HOVER_STROKE: Color32 = Color32::from_rgb(178, 210, 238);
/// 控件按下 / 激活：底。
pub const ACCENT_ACTIVE_BG: Color32 = Color32::from_rgb(210, 229, 247);
/// 控件按下 / 激活：描边。
pub const ACCENT_ACTIVE_STROKE: Color32 = Color32::from_rgb(124, 173, 217);

/// 选中高亮（图层行 / 列表选中 / 可选项）。
pub const SELECTION: Color32 = Color32::from_rgb(213, 230, 246);

/// 破坏性操作（右键菜单"删除"）：红。
pub const DANGER: Color32 = Color32::from_rgb(192, 76, 60);

// ───────────────── 设计 token：图层默认符号化（白底画布） ─────────────────

/// 多边形默认填充：半透明柔蓝（白底上呈淡蓝面）。
/// 用函数而非常量：`from_rgba_unmultiplied` 依赖运行时查找表，不是 const。
pub fn layer_fill() -> Color32 {
    Color32::from_rgba_unmultiplied(74, 144, 226, 80)
}

/// 多边形 / 线默认描边：ArcGIS 蓝（白底上清晰可辨）。
pub const LAYER_OUTLINE: Color32 = Color32::from_rgb(0, 113, 188);

/// 点要素默认符号：橙红（与蓝色系拉开距离，白底上醒目）。
pub const LAYER_POINT: Color32 = Color32::from_rgb(227, 92, 44);

// ─────────────────────── 设计 token：内容列表图标 ───────────────────────

/// 图层组图标：暖琥珀色文件夹（与蓝色系图层符号区分，一眼可辨"这是组"）。
pub const GROUP_ICON: Color32 = Color32::from_rgb(226, 176, 96);

// ─────────────────────────────── 工具函数 ───────────────────────────────

/// 1px 发丝线描边（面板边缘分隔用）。
pub fn hairline() -> Stroke {
    Stroke::new(1.0_f32, BORDER)
}

/// 柔和下拉投影：把白色窗口 / 弹窗从白色画布上"抬"出来。
fn soft_shadow(offset_y: i8, blur: u8) -> Shadow {
    Shadow {
        offset: [0, offset_y],
        blur,
        spread: 0,
        color: Color32::from_black_alpha(28),
    }
}

// ─────────────────────────────── Visuals ───────────────────────────────

/// 构建整应用 `Visuals`：白底 + 蓝色强调 + 发丝线分隔。
///
/// 在 egui 内置浅色主题之上逐项覆盖，未列出的字段（圆角 / 间距 / 弱文字色等）
/// 保留 egui 浅色主题的合理默认值。
pub fn visuals() -> egui::Visuals {
    let mut v = egui::Visuals::light();

    // ── 背景全白（面板 / 窗口 / 弹窗；弹窗与窗口共用 window_fill） ──
    v.panel_fill = WHITE;
    v.window_fill = WHITE;
    v.window_stroke = Stroke::new(1.0_f32, BORDER_STRONG);
    v.window_shadow = soft_shadow(3, 10);
    v.popup_shadow = soft_shadow(2, 6);

    // ── 强调色（ArcGIS 蓝）：超链接 / 文本光标 ──
    v.hyperlink_color = ACCENT;
    v.text_cursor.stroke = Stroke::new(1.5_f32, ACCENT);

    // ── 文字统一纯黑（产品约定）：覆盖全局默认文字色，标签 / 菜单 / 输入框 /
    //    控件文字全部为纯黑；egui 的 `.weak()` 与禁用文字在纯黑基础上灰化，
    //    层级只靠深浅区分，不引入其他文字颜色 ──
    v.override_text_color = Some(TEXT);

    // ── 选中高亮（列表 / 可选项 / 文本选中） ──
    v.selection.bg_fill = SELECTION;
    v.selection.stroke = Stroke::new(1.0_f32, ACCENT);

    // ── 弱底色（网格条纹 / 代码块底） ──
    v.faint_bg_color = SOFT_BG;
    v.code_bg_color = SOFT_BG;

    // ── 非交互控件：主文字色 + 发丝线分隔 ──
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, TEXT);
    v.widgets.noninteractive.bg_stroke = hairline();

    // ── 按钮 / 勾选框静止态：浅灰底 + 发丝线描边 ──
    v.widgets.inactive.bg_fill = SOFT_BG;
    v.widgets.inactive.weak_bg_fill = SOFT_BG;
    v.widgets.inactive.bg_stroke = hairline();
    v.widgets.inactive.fg_stroke = Stroke::new(1.0_f32, TEXT);

    // ── 悬停：浅蓝底 ──
    v.widgets.hovered.bg_fill = ACCENT_HOVER_BG;
    v.widgets.hovered.weak_bg_fill = ACCENT_HOVER_BG;
    v.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, ACCENT_HOVER_STROKE);
    v.widgets.hovered.fg_stroke = Stroke::new(1.0_f32, TEXT);

    // ── 按下 / 激活：加深蓝底 ──
    v.widgets.active.bg_fill = ACCENT_ACTIVE_BG;
    v.widgets.active.weak_bg_fill = ACCENT_ACTIVE_BG;
    v.widgets.active.bg_stroke = Stroke::new(1.0_f32, ACCENT_ACTIVE_STROKE);
    v.widgets.active.fg_stroke = Stroke::new(1.0_f32, TEXT);

    // ── 菜单展开态：与悬停同系 ──
    v.widgets.open.bg_fill = ACCENT_HOVER_BG;
    v.widgets.open.weak_bg_fill = ACCENT_HOVER_BG;
    v.widgets.open.bg_stroke = Stroke::new(1.0_f32, ACCENT_HOVER_STROKE);
    v.widgets.open.fg_stroke = Stroke::new(1.0_f32, TEXT);

    v
}
