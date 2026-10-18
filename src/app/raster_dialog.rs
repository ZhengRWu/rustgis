//! 栅格图层的「图层属性」对话框（对标 ArcGIS 图层属性）。
//!
//! 外观按 Windows 系统对话框绘制：**独立的系统窗口**（原生标题栏、可缩放、右上
//! 角关闭按钮）+ 对话框底色、经典页签条、列表框、分组框与右下角
//! 确定 / 取消 / 应用 按钮，而不是应用内的浮动窗口。
//!
//! 页签：常规 / 源 / 范围 / 显示 / 符号系统 / 时间。其中「符号系统」按
//! 拉伸 / RGB 合成两种方式组织：波段选择、显示背景值、NoData、拉伸类型
//! （百分比截断 / 最小值-最大值 / 标准差 / 无）、反向、Gamma、统计展示与
//! 后台计算按钮。
//!
//! 参数在对话框内以草稿形式编辑（[`RasterProps::draft`]），点「应用」写入图层、
//! 点「取消」或关闭窗口则丢弃；「确定」= 应用 + 关闭。统计计算走后台任务
//! （见 [`super::raster_op`]），对话框只负责读取最新结果并刷新显示。

use std::time::Duration;

use egui::{
    Align2, Color32, ComboBox, Context, DragValue, FontId, Frame, Grid, Layout, Margin, Rect,
    RichText, ScrollArea, Sense, Stroke, StrokeKind, Vec2, ViewportBuilder, ViewportId, pos2, vec2,
};

use crate::geo::Aabb;
use crate::raster::{BandInfo, RasterDisplay, StretchType, format_bytes};

use super::RustGisApp;

// ─────────────────────────── 系统对话框配色 ───────────────────────────

/// 对话框底色（Win32 系统对话框的「面板灰」）。
const DIALOG_BG: Color32 = Color32::from_rgb(240, 240, 240);
/// 输入框 / 列表框底（纯白）。
const FIELD_BG: Color32 = Color32::from_rgb(255, 255, 255);
/// 按钮面（浅灰立体按钮）。
const BUTTON_FACE: Color32 = Color32::from_rgb(225, 225, 225);
/// 按钮悬停面（浅蓝）。
const BUTTON_HOVER: Color32 = Color32::from_rgb(229, 241, 251);
/// 按钮边框（立体灰）。
const BUTTON_BORDER: Color32 = Color32::from_rgb(173, 173, 173);
/// 输入框 / 列表框的凹陷边框（比按钮边框深一档）。
const FIELD_BORDER: Color32 = Color32::from_rgb(122, 122, 122);
/// 分组框 / 页签的蚀刻线。
const LINE: Color32 = Color32::from_rgb(180, 180, 180);
/// 正文（纯黑）。
const TEXT: Color32 = Color32::from_rgb(0, 0, 0);
/// 列表选中底（系统高亮蓝）。
const SELECT_BG: Color32 = Color32::from_rgb(0, 120, 215);
/// 默认按钮的强调边框。
const ACCENT: Color32 = Color32::from_rgb(0, 120, 215);

// ─────────────────────────── 对话框外框参数 ───────────────────────────

/// 属性窗口初始尺寸。
const WINDOW_SIZE: [f32; 2] = [700.0, 580.0];
/// 属性窗口最小尺寸（再小页签条与按钮就会挤在一起）。
const WINDOW_MIN: [f32; 2] = [660.0, 460.0];
/// 页签条高度。
const TAB_HEIGHT: f32 = 23.0;
/// 页签左右内边距（文字两侧留白）。
const TAB_PAD: f32 = 11.0;
/// 页签最小宽度。
const TAB_MIN_WIDTH: f32 = 56.0;
/// 底部按钮尺寸。
const FOOTER_BUTTON: [f32; 2] = [84.0, 24.0];
/// 「显示」列表框宽度。
const LIST_WIDTH: f32 = 150.0;
/// 列表框行高。
const LIST_ROW_HEIGHT: f32 = 21.0;

// ─────────────────────────────── 页签 ───────────────────────────────

/// 属性页签。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tab {
    General,
    Source,
    Extent,
    Display,
    Symbology,
    Time,
}

/// 符号系统的显示方式。
#[derive(Clone, Copy, PartialEq, Eq)]
enum StyleMode {
    /// 单波段灰度（拉伸）。
    Stretch,
    /// 红色 / 绿色 / 蓝色三波段合成。
    Rgb,
}

/// 统计值快照（只取界面要显示的部分，避免每帧克隆直方图）。
#[derive(Clone)]
struct StatsView {
    min: f64,
    max: f64,
    mean: f64,
    std_dev: f64,
    valid_count: u64,
    from_file: bool,
    estimated: bool,
}

/// 打开的「图层属性」对话框。
pub(super) struct RasterProps {
    /// 对应图层 id。
    layer: u64,
    tab: Tab,
    /// 正在编辑的显示设置草稿。
    draft: RasterDisplay,
    /// 正在编辑的图层名。
    name: String,
    /// 正在编辑的「显示」勾选状态。
    visible: bool,
    /// 首帧标记：只在创建系统窗口的那一帧设置尺寸并抢焦点。
    first_frame: bool,
}

// ─────────────────────────── 系统对话框外观 ───────────────────────────

/// 系统对话框的控件视觉：浅灰底、方角、1px 边框、蓝色选中。
fn system_visuals() -> egui::Visuals {
    let mut v = egui::Visuals::light();

    v.panel_fill = DIALOG_BG;
    v.window_fill = DIALOG_BG;
    v.window_stroke = Stroke::new(1.0_f32, LINE);
    v.window_corner_radius = egui::CornerRadius::ZERO;
    v.window_shadow = egui::epaint::Shadow::NONE;
    v.popup_shadow = egui::epaint::Shadow::NONE;
    v.extreme_bg_color = FIELD_BG;
    v.faint_bg_color = DIALOG_BG;
    v.override_text_color = Some(TEXT);
    v.hyperlink_color = ACCENT;
    v.text_cursor.stroke = Stroke::new(1.0_f32, TEXT);

    v.selection.bg_fill = SELECT_BG;
    v.selection.stroke = Stroke::new(1.0_f32, Color32::WHITE);

    // 非交互：对话框底色 + 蚀刻线。
    v.widgets.noninteractive.bg_fill = DIALOG_BG;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, LINE);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, TEXT);
    v.widgets.noninteractive.corner_radius = egui::CornerRadius::ZERO;

    // 按钮静止：浅灰立体面 + 灰边。
    v.widgets.inactive.bg_fill = BUTTON_FACE;
    v.widgets.inactive.weak_bg_fill = BUTTON_FACE;
    v.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, BUTTON_BORDER);
    v.widgets.inactive.fg_stroke = Stroke::new(1.0_f32, TEXT);
    v.widgets.inactive.corner_radius = egui::CornerRadius::same(2);

    // 悬停：系统浅蓝。
    v.widgets.hovered.bg_fill = BUTTON_HOVER;
    v.widgets.hovered.weak_bg_fill = BUTTON_HOVER;
    v.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, ACCENT);
    v.widgets.hovered.fg_stroke = Stroke::new(1.0_f32, TEXT);
    v.widgets.hovered.corner_radius = egui::CornerRadius::same(2);

    // 按下：加深一档。
    v.widgets.active.bg_fill = Color32::from_rgb(204, 228, 247);
    v.widgets.active.weak_bg_fill = Color32::from_rgb(204, 228, 247);
    v.widgets.active.bg_stroke = Stroke::new(1.0_f32, ACCENT);
    v.widgets.active.fg_stroke = Stroke::new(1.0_f32, TEXT);
    v.widgets.active.corner_radius = egui::CornerRadius::same(2);

    // 下拉展开态：与悬停同系。
    v.widgets.open.bg_fill = BUTTON_HOVER;
    v.widgets.open.weak_bg_fill = BUTTON_HOVER;
    v.widgets.open.bg_stroke = Stroke::new(1.0_f32, ACCENT);
    v.widgets.open.fg_stroke = Stroke::new(1.0_f32, TEXT);
    v.widgets.open.corner_radius = egui::CornerRadius::same(2);

    v
}

/// 把当前 UI 子树切成系统对话框的控件风格（视觉 + 紧凑间距）。
fn apply_system_style(ui: &mut egui::Ui) {
    *ui.visuals_mut() = system_visuals();
    let spacing = ui.spacing_mut();
    spacing.item_spacing = vec2(6.0, 4.0);
    spacing.button_padding = vec2(12.0, 3.0);
    spacing.interact_size.y = 22.0;
    spacing.combo_width = 150.0;
    spacing.indent = 14.0;
}

/// 对话框正文用的系统字体（页签 / 分组框标题）。
fn dialog_font() -> FontId {
    FontId::proportional(13.0)
}

// ─────────────────────── 原生观感的小部件 ───────────────────────

/// 经典页签条：选中页签白底并与下面的页面连成一体，未选中为对话框底色。
fn tab_strip(ui: &mut egui::Ui, pages: &[(Tab, &str)], current: &mut Tab) {
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(vec2(width, TAB_HEIGHT + 1.0), Sense::hover());
    let painter = ui.painter();
    // 页签条底边：整条页签带下面的一条蚀刻线。
    painter.hline(
        rect.x_range(),
        rect.bottom() - 0.5,
        Stroke::new(1.0_f32, LINE),
    );

    let mut x = rect.left();
    let mut selected: Option<Rect> = None;
    for (page, label) in pages {
        let galley = painter.layout_no_wrap(label.to_string(), dialog_font(), TEXT);
        let tab_width = (galley.size().x + TAB_PAD * 2.0).max(TAB_MIN_WIDTH);
        let tab_rect = Rect::from_min_size(pos2(x, rect.top()), vec2(tab_width, TAB_HEIGHT));
        let response = ui.interact(
            tab_rect,
            ui.id().with(("props_tab", *label)),
            Sense::click(),
        );

        let is_current = *current == *page;
        let fill = if is_current {
            DIALOG_BG
        } else if response.hovered() {
            BUTTON_HOVER
        } else {
            BUTTON_FACE
        };
        painter.rect_filled(tab_rect.shrink2(vec2(0.0, 0.0)), 0.0, fill);
        painter.rect_stroke(
            tab_rect,
            0.0,
            Stroke::new(1.0_f32, LINE),
            StrokeKind::Inside,
        );

        let text_pos = pos2(
            tab_rect.center().x - galley.size().x * 0.5,
            tab_rect.center().y - galley.size().y * 0.5,
        );
        painter.galley(text_pos, galley, TEXT);

        if is_current {
            selected = Some(tab_rect);
        }
        if response.clicked() {
            *current = *page;
        }
        x += tab_width;
    }

    // 选中页签把底边线盖掉，形成「凸起并连到页面」的经典效果。
    if let Some(active) = selected {
        painter.rect_filled(
            Rect::from_min_max(
                pos2(active.left() + 1.0, active.bottom() - 1.0),
                pos2(active.right() - 1.0, active.bottom() + 0.5),
            ),
            0.0,
            DIALOG_BG,
        );
    }
}

/// 经典分组框：1px 蚀刻边框 + 压在边框线上的标题。
fn group_box(ui: &mut egui::Ui, title: &str, add: impl FnOnce(&mut egui::Ui)) {
    let galley = ui
        .painter()
        .layout_no_wrap(title.to_string(), dialog_font(), TEXT);
    let title_height = galley.size().y;
    let top_margin = (title_height * 0.5) as i8 + 7;
    let frame = Frame::new()
        .fill(DIALOG_BG)
        .stroke(Stroke::new(1.0_f32, LINE))
        .inner_margin(Margin {
            left: 10,
            right: 10,
            top: top_margin,
            bottom: 8,
        });
    let response = frame.show(ui, |ui| {
        ui.set_min_width((ui.available_width() - 4.0).max(120.0));
        add(ui);
    });

    let rect = response.response.rect;
    let text_pos = pos2(rect.left() + 9.0, rect.top() - title_height * 0.5);
    // 先用对话框底色盖掉标题下面的边框线，再把标题压在线上。
    ui.painter().rect_filled(
        Rect::from_min_size(
            pos2(text_pos.x - 4.0, text_pos.y),
            vec2(galley.size().x + 8.0, title_height),
        ),
        0.0,
        DIALOG_BG,
    );
    ui.painter().galley(text_pos, galley, TEXT);
}

/// 白底列表框（「显示(S)」那种）：选中行用系统高亮蓝。
fn style_list(ui: &mut egui::Ui, id: &str, rows: &[(StyleMode, &str)], current: &mut StyleMode) {
    let width = LIST_WIDTH;
    let height = LIST_ROW_HEIGHT * rows.len() as f32 + 2.0;
    let (rect, _) = ui.allocate_exact_size(vec2(width, height), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 0.0, FIELD_BG);
    painter.rect_stroke(
        rect,
        0.0,
        Stroke::new(1.0_f32, FIELD_BORDER),
        StrokeKind::Inside,
    );

    for (index, (mode, label)) in rows.iter().enumerate() {
        let row = Rect::from_min_size(
            pos2(
                rect.left() + 1.0,
                rect.top() + 1.0 + index as f32 * LIST_ROW_HEIGHT,
            ),
            vec2(rect.width() - 2.0, LIST_ROW_HEIGHT),
        );
        let response = ui.interact(row, ui.id().with((id, index)), Sense::click());
        let is_current = *current == *mode;
        if is_current {
            painter.rect_filled(row, 0.0, SELECT_BG);
        } else if response.hovered() {
            painter.rect_filled(row, 0.0, BUTTON_HOVER);
        }
        let color = if is_current { Color32::WHITE } else { TEXT };
        painter.text(
            pos2(row.left() + 6.0, row.center().y),
            Align2::LEFT_CENTER,
            *label,
            dialog_font(),
            color,
        );
        if response.clicked() {
            *current = *mode;
        }
    }
}

/// 对话框按钮（右下角的 确定 / 取消 / 应用）。
fn dialog_button(ui: &mut egui::Ui, text: &str, default: bool) -> egui::Response {
    let stroke = if default {
        Stroke::new(1.0_f32, ACCENT)
    } else {
        Stroke::new(1.0_f32, BUTTON_BORDER)
    };
    ui.add_sized(
        vec2(FOOTER_BUTTON[0], FOOTER_BUTTON[1]),
        egui::Button::new(text).stroke(stroke),
    )
}

/// 灰度色带预览：按当前 最小值/最大值、反向、Gamma 把 0…1 渲染成黑白渐变。
fn ramp_preview(ui: &mut egui::Ui, display: &RasterDisplay) {
    let width = ui.available_width().min(260.0);
    let (rect, _) = ui.allocate_exact_size(vec2(width, 20.0), Sense::hover());
    let painter = ui.painter();
    const STEPS: u32 = 96;
    let gamma = display.gamma[0].clamp(0.1, 10.0);
    let invert = display.invert[0];
    for step in 0..STEPS {
        let t = step as f32 / (STEPS - 1) as f32;
        let mut normalized = t;
        if invert {
            normalized = 1.0 - normalized;
        }
        let shade = normalized.powf(1.0 / gamma);
        let byte = (shade * 255.0 + 0.5) as u8;
        let left = rect.left() + rect.width() * step as f32 / STEPS as f32;
        let strip = Rect::from_min_size(
            pos2(left, rect.top()),
            vec2(rect.width() / STEPS as f32 + 1.0, rect.height()),
        );
        painter.rect_filled(strip, 0.0, Color32::from_rgb(byte, byte, byte));
    }
    painter.rect_stroke(
        rect,
        0.0,
        Stroke::new(1.0_f32, FIELD_BORDER),
        StrokeKind::Inside,
    );
}

/// 蚀刻横线（对话框里的分组分隔）。
fn etched_line(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 2.0), Sense::hover());
    ui.painter()
        .hline(rect.x_range(), rect.top() + 0.5, Stroke::new(1.0_f32, LINE));
}

impl RustGisApp {
    /// 打开某个栅格图层的属性对话框（双击图层行或右键「属性」）。
    pub(super) fn open_raster_props(&mut self, layer_id: u64) {
        let Some(layer) = self.layers.node(layer_id).and_then(|node| node.raster()) else {
            return;
        };
        self.raster_props = Some(RasterProps {
            layer: layer_id,
            tab: Tab::Symbology,
            draft: layer.display.clone(),
            name: layer.name.clone(),
            visible: layer.visible,
            first_frame: true,
        });
    }

    /// 绘制属性对话框：每帧调用，内部维护一个**独立的系统窗口**。
    ///
    /// 窗口由 `show_viewport_immediate` 创建，因此可以获得系统原生标题栏
    /// （标题 + 最小化/最大化/关闭），并且回调不是 `'static`，可以直接借用
    /// `&mut self` 与草稿状态，不需要邮箱线程。
    pub(super) fn show_raster_props(&mut self, ctx: &Context) {
        let Some(mut props) = self.raster_props.take() else {
            return;
        };
        let layer_id = props.layer;

        // 图层已被删除 / 换成矢量：直接关掉（不再显示窗口 = 窗口销毁）。
        let Some(layer) = self.layers.node(layer_id).and_then(|node| node.raster()) else {
            return;
        };
        let bounds = layer.bounds;

        // 数据集只在取快照这段加锁（毫秒级），之后立刻释放。
        let (path, meta, georef, stats, level_count, items) = {
            let dataset = layer.dataset();
            let path = dataset.path().display().to_string();
            let meta = dataset.metadata().clone();
            let georef = dataset.georeference().clone();
            let stats: Vec<Option<StatsView>> = (0..dataset.band_count())
                .map(|index| {
                    dataset.statistics(index).map(|s| StatsView {
                        min: s.min,
                        max: s.max,
                        mean: s.mean,
                        std_dev: s.std_dev,
                        valid_count: s.valid_count,
                        from_file: s.from_file,
                        estimated: s.estimated,
                    })
                })
                .collect();
            let items = dataset.metadata_items().to_vec();
            (path, meta, georef, stats, dataset.level_count(), items)
        };

        let bands = meta.bands.clone();
        let band_count = meta.band_count.max(1);
        let nodata = meta.nodata;
        let job_status = self.raster_job_status();
        let title = self.tf("dialog.rasterProps", &[&props.name]);

        // 窗口尺寸 / 抢焦点只在首帧做，之后交给系统（用户可自由缩放）。
        let first_frame = props.first_frame;
        props.first_frame = false;
        let mut builder = ViewportBuilder::default()
            .with_title(&title)
            .with_min_inner_size(WINDOW_MIN)
            .with_resizable(true)
            .with_close_button(true);
        if first_frame {
            builder = builder
                .with_inner_size(WINDOW_SIZE)
                .with_active(true)
                .with_app_id("rustgis-raster-props");
        }

        let mut close = false; // 取消 / 关闭窗口
        let mut apply = false; // 应用（确定也等于应用）
        let mut request_stats = false;
        let mut focus = first_frame;

        let viewport_id = ViewportId::from_hash_of("rustgis_raster_props");
        ctx.show_viewport_immediate(viewport_id, builder, |ctx, _class| {
            if focus {
                focus = false;
                ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            }
            // 系统标题栏的关闭按钮（X）等价于「取消」。
            if ctx.input(|i| i.viewport().close_requested()) {
                close = true;
            }

            // ── 底部按钮行（固定在窗口底部，不随页面滚动）──
            egui::TopBottomPanel::bottom("raster_props_footer")
                .frame(Frame::new().fill(DIALOG_BG).inner_margin(Margin {
                    left: 10,
                    right: 10,
                    top: 6,
                    bottom: 8,
                }))
                .show(ctx, |ui| {
                    apply_system_style(ui);
                    ui.with_layout(Layout::right_to_left(egui::Align::Center), |ui| {
                        if dialog_button(ui, self.t("props.apply"), false).clicked() {
                            apply = true;
                        }
                        if dialog_button(ui, self.t("props.cancel"), false).clicked() {
                            close = true;
                        }
                        if dialog_button(ui, self.t("props.ok"), true).clicked() {
                            apply = true;
                            close = true;
                        }
                    });
                });

            // ── 页签条 + 页面 ──
            egui::CentralPanel::default()
                .frame(Frame::new().fill(DIALOG_BG).inner_margin(Margin {
                    left: 8,
                    right: 8,
                    top: 8,
                    bottom: 2,
                }))
                .show(ctx, |ui| {
                    apply_system_style(ui);
                    let pages = [
                        (Tab::General, self.t("props.tab.general")),
                        (Tab::Source, self.t("props.tab.source")),
                        (Tab::Extent, self.t("props.tab.extent")),
                        (Tab::Display, self.t("props.tab.display")),
                        (Tab::Symbology, self.t("props.tab.symbology")),
                        (Tab::Time, self.t("props.tab.time")),
                    ];
                    tab_strip(ui, &pages, &mut props.tab);

                    Frame::new()
                        .fill(DIALOG_BG)
                        .stroke(Stroke::new(1.0_f32, LINE))
                        .inner_margin(Margin::same(10))
                        .show(ui, |ui| {
                            ScrollArea::vertical()
                                .auto_shrink([false, false])
                                .show(ui, |ui| match props.tab {
                                    Tab::General => {
                                        general_tab(ui, self, &mut props, &path, &meta);
                                    }
                                    Tab::Source => {
                                        source_tab(ui, self, &path, &meta, level_count, &items);
                                    }
                                    Tab::Extent => extent_tab(ui, self, &bounds, &georef),
                                    Tab::Display => display_tab(ui, self, &mut props),
                                    Tab::Symbology => symbology_tab(
                                        ui,
                                        self,
                                        &mut props,
                                        &bands,
                                        band_count,
                                        nodata,
                                        &stats,
                                        job_status.as_deref(),
                                        &mut request_stats,
                                    ),
                                    Tab::Time => {
                                        ui.add_space(8.0);
                                        ui.label(self.t("props.timeNone"));
                                    }
                                });
                        });
                });
        });

        if close {
            // 取消 / 关闭：草稿丢弃（「确定」已经先应用过一次）。
            if apply {
                self.apply_raster_props(
                    layer_id,
                    props.draft.clone(),
                    props.name.clone(),
                    props.visible,
                );
            }
            return;
        }

        if apply {
            self.apply_raster_props(
                layer_id,
                props.draft.clone(),
                props.name.clone(),
                props.visible,
            );
        }
        if request_stats {
            self.start_statistics_for_display(layer_id);
        }
        // 统计任务进行中：持续唤醒，让进度与统计结果实时出现在对话框里。
        self.start_statistics_for_display(layer_id);
        let wake = if job_status.is_some() { 100 } else { 250 };
        ctx.request_repaint_after(Duration::from_millis(wake));
        self.raster_props = Some(props);
    }

    /// 把对话框草稿写回图层（名称 / 显隐 / 显示设置）。
    fn apply_raster_props(
        &mut self,
        layer_id: u64,
        draft: RasterDisplay,
        name: String,
        visible: bool,
    ) {
        if let Some(node) = self.layers.node_mut(layer_id) {
            node.set_name(name);
        }
        self.layers.set_visible(layer_id, visible);
        self.layers.set_raster_display(layer_id, draft);
        self.dirty = true;
    }
}

// ─────────────────────────── 各页签内容 ───────────────────────────

/// 「常规」页：名称、类型、数据源、显隐。
fn general_tab(
    ui: &mut egui::Ui,
    app: &RustGisApp,
    props: &mut RasterProps,
    path: &str,
    meta: &crate::raster::RasterMetadata,
) {
    group_box(ui, app.t("props.name"), |ui| {
        Grid::new("raster_props_general")
            .num_columns(2)
            .spacing([12.0, 8.0])
            .show(ui, |ui| {
                ui.label(app.t("props.name"));
                ui.text_edit_singleline(&mut props.name);
                ui.end_row();

                ui.label(app.t("props.layerType"));
                ui.label(app.t("props.typeRaster"));
                ui.end_row();

                ui.label(app.t("props.visible"));
                ui.checkbox(&mut props.visible, app.t("props.show"));
                ui.end_row();
            });
    });

    ui.add_space(8.0);
    group_box(ui, app.t("props.sourcePath"), |ui| {
        Grid::new("raster_props_general_source")
            .num_columns(2)
            .spacing([12.0, 6.0])
            .show(ui, |ui| {
                ui.label(app.t("props.sourcePath"));
                ui.label(RichText::new(path).small());
                ui.end_row();

                ui.label(app.t("props.bands"));
                ui.label(meta.band_count.to_string());
                ui.end_row();

                ui.label(app.t("props.dataType"));
                ui.label(app.tf(
                    "props.bitsFormat",
                    &[meta.sample_type.label(), &meta.bits_per_sample.to_string()],
                ));
                ui.end_row();
            });
    });
}

/// 数字显示：最多 6 位小数并去掉多余的 0；极小值退回科学计数法。
fn trim_num(value: f64) -> String {
    if !value.is_finite() {
        return value.to_string();
    }
    let mut text = format!("{value:.6}");
    if text.contains('.') {
        text = text.trim_end_matches('0').trim_end_matches('.').to_owned();
    }
    if (text == "0" || text == "-0") && value.abs() > 0.0 {
        return format!("{value:e}");
    }
    text
}

/// 「源」页：完整元数据（宽高 / 波段 / 压缩 / 分块 / 文件大小 / BigTIFF / 概览）。
fn source_tab(
    ui: &mut egui::Ui,
    app: &RustGisApp,
    path: &str,
    meta: &crate::raster::RasterMetadata,
    level_count: usize,
    metadata_items: &[(String, String)],
) {
    group_box(ui, app.t("props.sourcePath"), |ui| {
        Grid::new("raster_props_source")
            .num_columns(2)
            .spacing([12.0, 6.0])
            .show(ui, |ui| {
                let mut row = |key: &str, value: String| {
                    ui.label(app.t(key));
                    ui.label(value);
                    ui.end_row();
                };
                row("props.sourcePath", path.to_string());
                row("props.width", format!("{} × {}", meta.width, meta.height));
                let layout = app.t(if meta.planar {
                    "props.layoutPlanar"
                } else {
                    "props.layoutChunky"
                });
                row(
                    "props.bands",
                    app.tf("props.bandLayout", &[&meta.band_count.to_string(), layout]),
                );
                row(
                    "props.dataType",
                    app.tf(
                        "props.bitsFormat",
                        &[meta.sample_type.label(), &meta.bits_per_sample.to_string()],
                    ),
                );
                row("props.compression", meta.compression.label());
                row(
                    "props.block",
                    app.tf(
                        "props.blockFormat",
                        &[
                            meta.block.label(),
                            &meta.block_width.to_string(),
                            &meta.block_height.to_string(),
                            &meta.blocks_across.to_string(),
                            &meta.blocks_down.to_string(),
                            &meta.total_blocks().to_string(),
                        ],
                    ),
                );
                row("props.fileSize", format_bytes(meta.file_size));
                row(
                    "props.bigTiff",
                    app.t(if meta.big_tiff {
                        "props.yes"
                    } else {
                        "props.no"
                    })
                    .to_string(),
                );
                row(
                    "props.overviews",
                    app.tf(
                        "props.overviewFormat",
                        &[
                            &meta.internal_overviews.to_string(),
                            yes_no(app, meta.has_external_overviews),
                            &level_count.to_string(),
                        ],
                    ),
                );
                row(
                    "props.nodata",
                    match meta.nodata {
                        Some(value) => format!("{value}"),
                        None => app.t("props.none").to_string(),
                    },
                );
                row("props.photometric", meta.photometric.clone());
                for band in &meta.bands {
                    let nodata = band
                        .nodata
                        .map(|value| value.to_string())
                        .unwrap_or_else(|| app.t("props.none").to_string());
                    row(
                        "props.bandInfo",
                        app.tf(
                            "props.bandLine",
                            &[
                                &(band.index + 1).to_string(),
                                &band.name,
                                band.color.label(),
                                &band.bits.to_string(),
                                &nodata,
                            ],
                        ),
                    );
                }
                for (name, value) in metadata_items {
                    row("props.metadata", format!("{name} = {value}"));
                }
            });
    });
}

/// 「范围」页：包围盒、像元大小、坐标系。
fn extent_tab(
    ui: &mut egui::Ui,
    app: &RustGisApp,
    bounds: &Aabb,
    georef: &crate::raster::GeoReference,
) {
    // GDAL GeoTransform：gt[1] 为列方向像元宽，gt[5] 为行方向像元高（北向上为负）。
    let pixel = georef
        .transform
        .as_ref()
        .map(|affine| {
            let gt = affine.geotransform();
            (gt[1], gt[5])
        })
        .unwrap_or((1.0, 1.0));

    group_box(ui, app.t("props.tab.extent"), |ui| {
        Grid::new("raster_props_extent")
            .num_columns(2)
            .spacing([12.0, 6.0])
            .show(ui, |ui| {
                let mut row = |key: &str, value: String| {
                    ui.label(app.t(key));
                    ui.label(value);
                    ui.end_row();
                };
                row(
                    "props.extentX",
                    format!("{} … {}", trim_num(bounds.min[0]), trim_num(bounds.max[0])),
                );
                row(
                    "props.extentY",
                    format!("{} … {}", trim_num(bounds.min[1]), trim_num(bounds.max[1])),
                );
                row(
                    "props.pixelSize",
                    format!("{} × {}", trim_num(pixel.0.abs()), trim_num(pixel.1.abs())),
                );
                row("props.crs", georef.crs_label());
                row("props.gcp", georef.gcp_count.to_string());
                if georef.is_missing() {
                    for warning in &georef.warnings {
                        row("props.warning", warning.clone());
                    }
                }
            });
    });
}

/// 「显示」页：重采样方式（固定最近邻）与显隐。
fn display_tab(ui: &mut egui::Ui, app: &RustGisApp, props: &mut RasterProps) {
    group_box(ui, app.t("props.resampling"), |ui| {
        ui.checkbox(&mut props.visible, app.t("props.show"));
        ui.add_space(4.0);
        etched_line(ui);
        ui.add_space(4.0);
        ui.label(app.t("props.resampling"));
        ui.label(RichText::new(app.t("props.nearest")).small());
        ui.add_space(6.0);
        ui.label(RichText::new(app.t("props.resamplingHint")).small());
    });
}

/// 「符号系统」页：拉伸 / RGB 合成（按系统对话框的列表框 + 分组框组织）。
#[allow(clippy::too_many_arguments)]
fn symbology_tab(
    ui: &mut egui::Ui,
    app: &mut RustGisApp,
    props: &mut RasterProps,
    bands: &[BandInfo],
    band_count: usize,
    nodata: Option<f64>,
    stats: &[Option<StatsView>],
    job_status: Option<&str>,
    request_stats: &mut bool,
) {
    let mut mode = current_mode(&props.draft);

    ui.horizontal_top(|ui| {
        // ── 左：显示方式（对标系统对话框的「显示(S)」列表框）──
        ui.vertical(|ui| {
            ui.label(RichText::new(app.t("props.show")).strong());
            ui.add_space(2.0);
            let mut rows = vec![(StyleMode::Stretch, app.t("props.styleStretch"))];
            if band_count >= 3 {
                rows.push((StyleMode::Rgb, app.t("props.styleRgb")));
            }
            style_list(ui, "raster_style_list", &rows, &mut mode);
        });

        ui.add_space(8.0);

        // ── 右：参数分组框 ──
        ui.vertical(|ui| {
            ui.set_min_width(420.0);
            if mode != current_mode(&props.draft) {
                apply_mode(&mut props.draft, mode, band_count);
            }
            match mode {
                StyleMode::Stretch => stretch_style(ui, app, props, bands, nodata),
                StyleMode::Rgb => {
                    rgb_style(ui, app, props, bands, band_count, nodata);
                }
            }

            ui.add_space(6.0);
            group_box(ui, app.t("props.statistics"), |ui| {
                ui.horizontal(|ui| {
                    ui.label(app.t("props.statsDataset"));
                    if ui.button(app.t("props.computeStats")).clicked() {
                        *request_stats = true;
                    }
                });
                if let Some(status) = job_status {
                    ui.label(RichText::new(status).small());
                }
                // 只列出当前显示实际用到的波段，避免单波段图层出现空白的「绿色/蓝色」行。
                let mut rows: Vec<(String, usize)> = Vec::new();
                match mode {
                    StyleMode::Rgb => {
                        for (index, key) in
                            [(0usize, "props.red"), (1, "props.green"), (2, "props.blue")]
                        {
                            let band = props.draft.bands[index];
                            if rows.iter().any(|(_, listed)| *listed == band) {
                                continue;
                            }
                            rows.push((app.t(key).to_owned(), band));
                        }
                    }
                    StyleMode::Stretch => {
                        let band = props.draft.bands[0];
                        let name = bands
                            .get(band)
                            .map(|info| info.name.clone())
                            .unwrap_or_default();
                        rows.push((name, band));
                    }
                }
                for (label, index) in rows {
                    ui.horizontal(|ui| {
                        ui.label(label);
                        match stats.get(index).and_then(|s| s.as_ref()) {
                            Some(view) => {
                                let mut line = app.tf(
                                    "props.statsLine",
                                    &[
                                        &format!("{:.4}", view.min),
                                        &format!("{:.4}", view.max),
                                        &format!("{:.4}", view.mean),
                                        &format!("{:.4}", view.std_dev),
                                    ],
                                );
                                if view.from_file {
                                    line.push_str(app.t("props.statsFromFile"));
                                }
                                if view.estimated {
                                    line.push_str(app.t("props.statsEstimate"));
                                }
                                ui.label(RichText::new(line).small());
                                ui.weak(view.valid_count.to_string());
                            }
                            None => {
                                ui.weak(app.t("props.statsMissing"));
                            }
                        }
                    });
                }
            });
        });
    });
}

/// 拉伸方式（单波段灰度）的参数。
fn stretch_style(
    ui: &mut egui::Ui,
    app: &RustGisApp,
    props: &mut RasterProps,
    bands: &[BandInfo],
    nodata: Option<f64>,
) {
    group_box(ui, app.t("props.styleStretch"), |ui| {
        Grid::new("raster_props_stretch_mode")
            .num_columns(2)
            .spacing([12.0, 6.0])
            .show(ui, |ui| {
                ui.label(app.t("props.band"));
                band_combo(ui, "props.stretch_band", &mut props.draft.bands[0], bands);
                ui.end_row();
            });
        ui.add_space(6.0);
        ramp_preview(ui, &props.draft);

        // 灰度：三个通道共用同一波段，参数统一到通道 0 再回填。
        ui.add_space(8.0);
        stretch_controls(ui, app, props, &[0]);
        revert_channels(&mut props.draft);
    });

    ui.add_space(6.0);
    background_group(ui, app, props, nodata);
}

/// RGB 合成方式的参数。
fn rgb_style(
    ui: &mut egui::Ui,
    app: &RustGisApp,
    props: &mut RasterProps,
    bands: &[BandInfo],
    band_count: usize,
    nodata: Option<f64>,
) {
    group_box(ui, app.t("props.styleRgbTitle"), |ui| {
        Grid::new("raster_props_channels")
            .num_columns(2)
            .spacing([16.0, 6.0])
            .show(ui, |ui| {
                ui.label(RichText::new(app.t("props.channel")).small());
                ui.label(RichText::new(app.t("props.band")).small());
                ui.end_row();

                for (index, key) in [(0usize, "props.red"), (1, "props.green"), (2, "props.blue")] {
                    ui.label(app.t(key));
                    band_combo(ui, key, &mut props.draft.bands[index], bands);
                    ui.end_row();
                }

                ui.label(app.t("props.alpha"));
                ui.horizontal(|ui| {
                    let mut has_alpha = props.draft.alpha_band.is_some();
                    if ui.checkbox(&mut has_alpha, "").changed() {
                        props.draft.alpha_band = if has_alpha {
                            Some((band_count - 1).min(3))
                        } else {
                            None
                        };
                    }
                    if let Some(alpha) = props.draft.alpha_band.as_mut() {
                        band_combo(ui, "props.alpha", alpha, bands);
                    }
                });
                ui.end_row();
            });
    });

    ui.add_space(6.0);
    group_box(ui, app.t("props.stretchType"), |ui| {
        stretch_controls(ui, app, props, &[0, 1, 2]);
    });

    ui.add_space(6.0);
    background_group(ui, app, props, nodata);
}

/// 拉伸公共参数：类型 / 百分比截断 / 最小值 / 最大值 / 反向 / Gamma。
fn stretch_controls(
    ui: &mut egui::Ui,
    app: &RustGisApp,
    props: &mut RasterProps,
    channels: &[usize],
) {
    ui.horizontal(|ui| {
        ui.label(app.t("props.stretchType"));
        let mut stretch = props.draft.stretch;
        ComboBox::from_id_salt("raster_props_stretch")
            .selected_text(app.t(stretch_key(stretch)))
            .show_ui(ui, |ui| {
                for kind in StretchType::ALL {
                    ui.selectable_value(&mut stretch, kind, app.t(stretch_key(kind)));
                }
            });
        props.draft.stretch = stretch;
        if stretch == StretchType::PercentClip {
            ui.label(app.t("props.percentClip"));
            ui.add(
                DragValue::new(&mut props.draft.percent_clip)
                    .range(0.0..=49.0)
                    .speed(0.1)
                    .fixed_decimals(2),
            );
        }
    });

    ui.add_space(4.0);

    Grid::new("raster_props_channel_range")
        .num_columns(5)
        .spacing([10.0, 6.0])
        .show(ui, |ui| {
            ui.label("");
            ui.label(RichText::new(app.t("props.min")).small());
            ui.label(RichText::new(app.t("props.max")).small());
            ui.label(RichText::new(app.t("props.invert")).small());
            ui.label(RichText::new(app.t("props.gamma")).small());
            ui.end_row();

            for &index in channels {
                if channels.len() > 1 {
                    let key = ["props.red", "props.green", "props.blue"][index.min(2)];
                    ui.label(app.t(key));
                } else {
                    ui.label("");
                }
                ui.add(
                    DragValue::new(&mut props.draft.min[index])
                        .speed(0.01)
                        .fixed_decimals(4),
                );
                ui.add(
                    DragValue::new(&mut props.draft.max[index])
                        .speed(0.01)
                        .fixed_decimals(4),
                );
                ui.checkbox(&mut props.draft.invert[index], "");
                ui.add(
                    DragValue::new(&mut props.draft.gamma[index])
                        .range(0.1..=10.0)
                        .speed(0.00001)
                        .fixed_decimals(5),
                );
                ui.end_row();
            }
        });
    ui.label(RichText::new(app.t("props.stretchHint")).small());
}

/// 灰度模式的通道 1/2 跟随通道 0。
fn revert_channels(display: &mut RasterDisplay) {
    for index in 1..3 {
        display.min[index] = display.min[0];
        display.max[index] = display.max[0];
        display.invert[index] = display.invert[0];
        display.gamma[index] = display.gamma[0];
    }
}

/// 「背景与 NoData」分组框。
fn background_group(
    ui: &mut egui::Ui,
    app: &RustGisApp,
    props: &mut RasterProps,
    nodata: Option<f64>,
) {
    group_box(ui, app.t("props.background"), |ui| {
        ui.horizontal(|ui| {
            let mut enabled = props.draft.background.is_some();
            if ui
                .checkbox(&mut enabled, app.t("props.background"))
                .changed()
            {
                props.draft.background = enabled.then_some([0, 0, 0]);
            }
            if let Some(color) = props.draft.background.as_mut() {
                for channel in color.iter_mut() {
                    ui.add(DragValue::new(channel).range(0..=255).speed(1));
                }
                let (rect, _) = ui.allocate_exact_size(Vec2::splat(16.0), Sense::hover());
                ui.painter().rect_filled(
                    rect,
                    0.0,
                    Color32::from_rgb(color[0], color[1], color[2]),
                );
                ui.painter().rect_stroke(
                    rect,
                    0.0,
                    Stroke::new(1.0_f32, FIELD_BORDER),
                    StrokeKind::Inside,
                );
            }
        });

        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label(app.t("props.nodataAs"));
            match nodata {
                Some(value) => {
                    ui.label(format!("({value})"));
                }
                None => {
                    ui.label(app.t("props.none"));
                }
            }
            ui.checkbox(
                &mut props.draft.nodata_transparent,
                app.t("props.transparent"),
            );
        });
    });
}

/// 当前草稿对应的显示方式。
fn current_mode(display: &RasterDisplay) -> StyleMode {
    if display.bands[0] == display.bands[1] && display.bands[1] == display.bands[2] {
        StyleMode::Stretch
    } else {
        StyleMode::Rgb
    }
}

/// 切换显示方式（灰度 = 三通道取同一波段；RGB = 前三个波段）。
fn apply_mode(display: &mut RasterDisplay, mode: StyleMode, band_count: usize) {
    match mode {
        StyleMode::Stretch => {
            let band = display.bands[0].min(band_count.saturating_sub(1));
            display.bands = [band, band, band];
        }
        StyleMode::Rgb => {
            if band_count >= 3 {
                display.bands = [0, 1, 2];
            }
        }
    }
}

/// 波段下拉框。
fn band_combo(ui: &mut egui::Ui, id: &str, current: &mut usize, bands: &[BandInfo]) {
    let text = bands
        .get(*current)
        .map(|band| band.name.clone())
        .unwrap_or_else(|| format!("Band_{}", *current + 1));
    ComboBox::from_id_salt(("raster_band", id))
        .selected_text(text)
        .show_ui(ui, |ui| {
            for (index, band) in bands.iter().enumerate() {
                ui.selectable_value(current, index, &band.name);
            }
        });
}

/// 拉伸类型 → i18n key。
fn stretch_key(stretch: StretchType) -> &'static str {
    match stretch {
        StretchType::None => "props.stretchNone",
        StretchType::MinMax => "props.stretchMinMax",
        StretchType::PercentClip => "props.stretchPercent",
        StretchType::StdDev => "props.stretchStdDev",
    }
}

fn yes_no(app: &RustGisApp, value: bool) -> &'static str {
    app.t(if value { "props.yes" } else { "props.no" })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_properties_tab_draws_headless() {
        let ctx = egui::Context::default();
        let mut app = RustGisApp::new(&ctx);

        for (name, display) in [
            ("props-gray8.tif", RasterDisplay::for_band_count(1, false)),
            ("props-rgb8.tif", RasterDisplay::for_band_count(3, false)),
        ] {
            let path = if display.bands == [0, 1, 2] {
                crate::raster::fixture_tests::sample_rgb_geotiff(name)
            } else {
                crate::raster::fixture_tests::sample_geotiff(name)
            };
            let layer = crate::raster::RasterLayer::open(&path, name.to_owned()).unwrap();
            let id = app.layers.add_raster(layer);

            app.open_raster_props(id);
            let props = app.raster_props.as_mut().expect("属性弹框应打开");
            assert_eq!(props.tab, Tab::Symbology);

            for tab in [
                Tab::General,
                Tab::Source,
                Tab::Extent,
                Tab::Display,
                Tab::Symbology,
                Tab::Time,
            ] {
                app.open_raster_props(id);
                let props = app.raster_props.as_mut().expect("属性弹框应打开");
                props.tab = tab;
                props.draft = display.clone();
                let _ = ctx.run(headless_input(), |ctx| app.show_raster_props(ctx));
                assert!(app.raster_props.is_some(), "属性弹框应保持打开");
            }

            // 「应用」把草稿写回图层：名称、可见性、显示设置都跟随。
            app.apply_raster_props(id, display.clone(), "改名后".to_owned(), false);
            let layer = app
                .layers
                .node(id)
                .and_then(|node| node.raster())
                .expect("图层应仍在树中");
            assert_eq!(layer.name, "改名后");
            assert!(!layer.visible);
            assert_eq!(layer.display.bands, display.bands);

            app.layers.remove(id);
        }
    }

    fn headless_input() -> egui::RawInput {
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1024.0, 768.0),
            )),
            ..Default::default()
        }
    }
}
