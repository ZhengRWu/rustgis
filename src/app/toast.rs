//! 右下角通知（toast）。
//!
//! 后台任务（统计 / 金字塔构建）成功结束后不再占用状态栏，而是在地图右下角
//! 弹一条通知：淡入、悬停冻结倒计时、到期淡出。外观沿用桌面工业软件的成熟
//! 做法——浅色卡片 + 1px 发丝边框 + 方角 + 左侧一条状态色标，底部一条随寿命
//! 缩短的进度条，右上角一个悬停高亮的关闭按钮。
//!
//! 布局顺序：通知压在地图之上、模态对话框之下（调用位置见
//! `RustGisApp::update`），因此不会遮挡需要用户确认的窗口。

use std::time::Duration;

use egui::{
    Align2, Area, Color32, Context, CornerRadius, CursorIcon, Frame, Id, Label, Layout, Margin,
    Order, Rect, RichText, Sense, Shadow, Stroke, StrokeKind, Ui, Vec2, pos2, vec2,
};

use crate::style;

// ─────────────────────────── 尺寸与时间 ───────────────────────────

/// 卡片宽度（物理像素的 egui 逻辑点）。
/// 固定宽度让多条通知右边缘对齐，视觉上像一列系统通知。
const CARD_WIDTH: f32 = 320.0;
/// 卡片左边距 11 + 右边距 8 + 左右各 1px 边框 ⇒ 内容宽度。
const CONTENT_WIDTH: f32 = CARD_WIDTH - 21.0;
/// 一条通知的存活时间（秒）。
const TTL: f64 = 6.0;
/// 淡入时长。
const FADE_IN: f64 = 0.14;
/// 淡出时长（最后这段时间里透明度线性降到 0）。
const FADE_OUT: f64 = 0.40;
/// 同屏最多保留的条数，超出时丢弃最旧的一条。
const MAX_TOASTS: usize = 4;
/// 卡片到窗口右下角的间距。
const SCREEN_MARGIN: f32 = 14.0;
/// 左侧状态色标宽度。
const ACCENT_BAR: f32 = 3.0;
/// 卡片间距。
const GAP: f32 = 8.0;

// ─────────────────────────── 数据 ───────────────────────────

/// 一条通知。
struct Toast {
    /// 唯一 id（`Area` 内不需要，但保留用于将来做「点击定位到条目」）。
    #[allow(dead_code)]
    id: u64,
    /// 标题：通常是被操作对象的名称（例如影像文件名）。
    title: String,
    /// 正文：结果文案（例如「统计完成：3 个波段」）。
    body: String,
    /// 弹出的时刻（`ctx.input(|i| i.time)`），用于算淡入。
    spawned: f64,
    /// 剩余存活秒数（悬停时冻结）。
    remaining: f64,
    /// 上一帧的时间戳，用于算帧间隔。
    last: f64,
    /// 当前透明度（0..=1）。
    alpha: f32,
}

impl Toast {
    fn new(id: u64, now: f64, title: String, body: String) -> Self {
        Self {
            id,
            title,
            body,
            spawned: now,
            remaining: TTL,
            last: now,
            alpha: 0.0,
        }
    }

    /// 推进时间轴。
    ///
    /// 透明度取「淡入」与「最后 [`FADE_OUT`] 秒淡出」两者的较小值；鼠标悬停时
    /// 冻结倒计时（与系统通知一致，方便用户读完长文案）。
    fn tick(&mut self, now: f64, hovered: bool) {
        // 窗口最小化或被调试器挂起后 dt 可能非常大，钳住上限，避免通知瞬间过期。
        let dt = (now - self.last).clamp(0.0, 0.25);
        self.last = now;
        if !hovered {
            self.remaining = (self.remaining - dt).max(0.0);
        }
        let age = (now - self.spawned).max(0.0);
        let fade_in = (age / FADE_IN).clamp(0.0, 1.0) as f32;
        let fade_out = (self.remaining / FADE_OUT).clamp(0.0, 1.0) as f32;
        self.alpha = fade_in.min(fade_out);
    }
}

/// 通知队列（应用级单例，挂在 `RustGisApp` 上）。
#[derive(Default)]
pub struct ToastHost {
    items: Vec<Toast>,
    next_id: u64,
}

impl ToastHost {
    /// 弹出一条通知。`title` 一般是被操作对象名，`body` 是结果文案。
    pub fn push(&mut self, now: f64, title: impl Into<String>, body: impl Into<String>) {
        self.next_id = self.next_id.wrapping_add(1);
        self.items
            .push(Toast::new(self.next_id, now, title.into(), body.into()));
        if self.items.len() > MAX_TOASTS {
            self.items.remove(0);
        }
    }

    /// 每帧画一次。`bottom_inset` 是窗口底部被状态栏占掉的高度，
    /// 通知据此抬到状态栏之上（不遮挡坐标 / 比例尺）。没有通知时零开销返回。
    pub fn show(&mut self, ctx: &Context, bottom_inset: f32) {
        if self.items.is_empty() {
            return;
        }
        let now = ctx.input(|input| input.time);
        let lift = SCREEN_MARGIN + bottom_inset.max(0.0);

        Area::new(Id::new("rustgis.toasts"))
            .order(Order::Foreground)
            .anchor(Align2::RIGHT_BOTTOM, vec2(-SCREEN_MARGIN, -lift))
            .interactable(true)
            .show(ctx, |ui| {
                ui.set_width(CARD_WIDTH);
                ui.spacing_mut().item_spacing = vec2(0.0, GAP);
                // 队列顺序即自上而下的顺序：最新的一条贴着右下角。
                for toast in &mut self.items {
                    draw_toast(ui, toast, now);
                }
            });

        // 到期（或点过关闭、已淡出到 0）就移除。
        self.items
            .retain(|toast| toast.remaining > 0.0 || toast.alpha > 0.0);

        // 淡入淡出与进度条都需要连续重绘；通知不多，50 ms 足够顺滑。
        if !self.items.is_empty() {
            ctx.request_repaint_after(Duration::from_millis(50));
        }
    }
}

// ─────────────────────────── 绘制 ───────────────────────────

/// 画一条卡片，并顺手推进它的时间轴。
fn draw_toast(ui: &mut Ui, toast: &mut Toast, now: f64) {
    let alpha = toast.alpha.clamp(0.0, 1.0);
    // 淡入淡出统一走「同色不同透明度」，避免整块内容在过渡期间跳色。
    let faded = |color: Color32| color.gamma_multiply(alpha);

    let frame = Frame::new()
        .fill(faded(style::WHITE))
        .stroke(Stroke::new(1.0_f32, faded(style::BORDER_STRONG)))
        .corner_radius(CornerRadius::same(2))
        .shadow(Shadow {
            offset: [0, 2],
            blur: 10,
            spread: 0,
            color: Color32::from_black_alpha((30.0 * alpha) as u8),
        })
        .inner_margin(Margin {
            // 左边留出色标的位置。
            left: (ACCENT_BAR as i8) + 8,
            right: 8,
            top: 8,
            bottom: 10,
        });

    let response = frame.show(ui, |ui| {
        ui.set_width(CONTENT_WIDTH);
        ui.horizontal(|ui| {
            check_badge(ui, alpha);
            ui.add_space(6.0);
            // 先占住右侧的关闭按钮，标题只能用剩下的宽度（否则长文件名会把 ✕ 顶出卡片）。
            ui.with_layout(Layout::right_to_left(egui::Align::Center), |ui| {
                if close_button(ui, alpha) {
                    // 触发淡出，而不是立刻消失（`retain` 会在透明度归零后清掉）。
                    toast.remaining = toast.remaining.min(FADE_OUT);
                }
                ui.with_layout(Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.add(
                        Label::new(
                            RichText::new(&toast.title)
                                .size(12.5)
                                .strong()
                                .color(faded(style::TEXT)),
                        )
                        .truncate(),
                    );
                });
            });
        });
        ui.add_space(3.0);
        ui.add(
            Label::new(
                RichText::new(&toast.body)
                    .size(12.0)
                    .color(faded(style::TEXT_WEAK)),
            )
            .wrap(),
        );
    });

    let rect = response.response.rect;
    let painter = ui.painter();
    // 左侧状态色标：整张卡片高，是这条通知的「状态条」。
    painter.rect_filled(
        Rect::from_min_size(rect.min, vec2(ACCENT_BAR, rect.height())),
        0.0,
        faded(style::ACCENT),
    );
    // 底部寿命条：剩余时间越短越短（悬停时停住），一眼看出还会停留多久。
    let frac = (toast.remaining / TTL).clamp(0.0, 1.0) as f32;
    if frac > 0.0 {
        let track = Rect::from_min_size(
            pos2(rect.left() + ACCENT_BAR, rect.bottom() - 2.0),
            vec2((rect.width() - ACCENT_BAR) * frac, 2.0),
        );
        painter.rect_filled(track, 0.0, faded(style::ACCENT).gamma_multiply(0.45));
    }

    toast.tick(now, response.response.hovered());
}

/// 勾选徽标：蓝色圆角方块 + 白色对勾（「任务成功」的通用记号）。
fn check_badge(ui: &mut Ui, alpha: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(14.0), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 2.0, style::ACCENT.gamma_multiply(alpha));
    let center = rect.center();
    let stroke = Stroke::new(1.6_f32, style::WHITE.gamma_multiply(alpha));
    // 对勾两段折线：左下短、右上长，比字形贴图更清晰。
    let start = center + vec2(-3.4, 0.2);
    let elbow = center + vec2(-1.0, 2.6);
    let end = center + vec2(3.6, -2.4);
    painter.line_segment([start, elbow], stroke);
    painter.line_segment([elbow, end], stroke);
}

/// 关闭按钮（✕）：悬停时出现浅灰底与边框，点一下触发淡出。
fn close_button(ui: &mut Ui, alpha: f32) -> bool {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(16.0), Sense::click());
    let hovered = response.hovered();
    let painter = ui.painter();
    if hovered {
        painter.rect_filled(rect, 2.0, style::SOFT_BG.gamma_multiply(alpha));
        painter.rect_stroke(
            rect,
            2.0,
            Stroke::new(1.0_f32, style::BORDER.gamma_multiply(alpha)),
            StrokeKind::Inside,
        );
    }
    let glyph = if hovered { style::TEXT } else { style::TEXT_WEAK };
    let stroke = Stroke::new(1.2_f32, glyph.gamma_multiply(alpha));
    let center = rect.center();
    let arm = 3.5;
    painter.line_segment(
        [center + vec2(-arm, -arm), center + vec2(arm, arm)],
        stroke,
    );
    painter.line_segment(
        [center + vec2(arm, -arm), center + vec2(-arm, arm)],
        stroke,
    );
    if hovered {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    response.clicked()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 无头跑一帧：`show` 自己从 `RawInput::time` 取时间，所以可以用它推进时间轴。
    fn paint_at(ctx: &Context, host: &mut ToastHost, time: f64) {
        let input = egui::RawInput {
            time: Some(time),
            ..Default::default()
        };
        // 测试只关心 `show` 的副作用（推进时间轴、移除过期项）。
        let _ = ctx.run(input, |ctx| host.show(ctx, 0.0));
    }

    #[test]
    fn toast_fades_in_then_expires() {
        let ctx = Context::default();
        let mut host = ToastHost::default();
        host.push(0.0, "S2A_T32TQR_20261016_RGB_10m.tif", "统计完成：3 个波段");
        assert_eq!(host.items.len(), 1);

        // 首帧是淡入起点：透明度 0，但还不能移除。
        let mut time = 0.0;
        paint_at(&ctx, &mut host, time);
        assert_eq!(host.items.len(), 1, "淡入阶段不应移除");

        // 淡入结束后完全可见。
        time += 0.2;
        paint_at(&ctx, &mut host, time);
        assert_eq!(host.items[0].alpha, 1.0);

        // 连续小步推进到超过存活时间：淡出到 0 后自动移除。
        while time < TTL + FADE_OUT + 0.5 {
            time += 0.05;
            paint_at(&ctx, &mut host, time);
        }
        assert!(host.items.is_empty(), "到期后应自动移除");
    }

    #[test]
    fn newest_toast_drops_the_oldest_beyond_the_limit() {
        let mut host = ToastHost::default();
        for index in 0..MAX_TOASTS + 2 {
            host.push(0.0, format!("layer{index}"), "统计完成：1 个波段");
        }
        assert_eq!(host.items.len(), MAX_TOASTS);
        // 队列首条应是最旧存活的一条（layer0、layer1 已被丢弃）。
        assert_eq!(host.items[0].title, "layer2");
    }
}
