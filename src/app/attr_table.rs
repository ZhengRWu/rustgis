//! 属性表（对标 QGIS 的「Open Attribute Table」）。
//!
//! 两种形态：默认**底部停靠面板**（拖动上沿改变高度），点工具条上的分离按钮后变成
//! **独立系统窗口**，可自由拖动与缩放；窗口标题栏的 ✕ 与面板里的 ✕ 等价，都会收起
//! 属性表，把空间还给地图。整体走工业风 — 深石墨表头、等宽数字、1px 网格、斑马纹与
//! 冷蓝高亮，数据密度高于通用控件。
//!
//! 渲染是只读的：行数据来自 [`Layer::attrs`]（导入时已按字段铺平）。过滤 / 排序
//! 结果按「(过滤文本, 排序)」缓存，滚动只绘制可见行区间（行高固定），因此十万级
//! 要素也能流畅滚动。
//!
//! 布局与交互在 [`mod tests`] 里用无头 `Context` 做了回归，改动本文件后要先过测试。

use egui::{
    Align, Align2, Color32, FontId, Layout, Pos2, Rect, RichText, Sense, Stroke, TextEdit,
    TopBottomPanel, Ui, Vec2, ViewportBuilder, ViewportId,
};

use crate::geo::proj::display_aabb;
use crate::geo::{Aabb, Layer};
use crate::i18n::Language;
use crate::style;

use super::RustGisApp;
use super::widgets::icon_button;

// ─────────────────────── 工业风配色 ───────────────────────

/// 面板底色：白，与图层树一致。
const PANEL_BG: Color32 = Color32::WHITE;
/// 表头底色：石墨灰（工业设备面板色）。
const HEADER_BG: Color32 = Color32::from_rgb(52, 60, 68);
/// 表头文字：冷白。
const HEADER_TEXT: Color32 = Color32::from_rgb(228, 234, 240);
/// 表头列分隔线（比底色略亮）。
const HEADER_SEP: Color32 = Color32::from_rgb(88, 99, 110);
/// 排序箭头。
const SORT_ARROW: Color32 = Color32::from_rgb(120, 190, 255);
/// 网格线：冷灰蓝，比通用分隔线淡。
const GRID: Color32 = Color32::from_rgb(210, 217, 224);
/// 斑马纹（隔行）。
const ZEBRA: Color32 = Color32::from_rgb(246, 249, 251);
/// 悬停行底色。
const ROW_HOVER: Color32 = Color32::from_rgb(232, 241, 250);
/// 选中行底色。
const ROW_SELECTED_BG: Color32 = Color32::from_rgb(219, 235, 247);
/// 选中行边框。
const ROW_SELECTED_EDGE: Color32 = Color32::from_rgb(58, 122, 178);
/// 单元格文字。
const CELL_TEXT: Color32 = Color32::from_rgb(40, 46, 52);

// ─────────────────────── 布局常量 ───────────────────────

/// 单行高度（工业风密度：比通用按钮矮）。
const ROW_H: f32 = 20.0;
/// 面板底部的状态条高度。
const STATUS_H: f32 = 20.0;
/// 表头高度。
const HEADER_H: f32 = 22.0;
/// 单元格左右内边距。
const CELL_PAD: f32 = 6.0;
/// FID 列宽。
const FID_W: f32 = 58.0;
/// 列宽下限 / 上限。
const MIN_COL_W: f32 = 56.0;
const MAX_COL_W: f32 = 260.0;
/// 按内容估宽时单字符的近似宽度（CJK 与西文混排折中）。
const CHAR_W: f32 = 7.0;
/// 单元格字号。
const CELL_FONT_SIZE: f32 = 11.5;

// ─────────────────────── 分离窗口 ───────────────────────

/// 属性表分离窗口的固定 id（同一时刻只有一个属性表，换图层时复用同一个窗口）。
fn window_id() -> ViewportId {
    ViewportId::from_hash_of("rustgis_attribute_table")
}
/// 分离窗口首次出现时的尺寸（之后由用户拖动 / 缩放决定）。
const WINDOW_W: f32 = 980.0;
const WINDOW_H: f32 = 520.0;
/// 分离窗口可缩到的最小尺寸：工具条 + 表头 + 两行数据仍要能放下。
const WINDOW_MIN_W: f32 = 520.0;
const WINDOW_MIN_H: f32 = 240.0;

/// 过滤 / 排序结果的缓存键与内容。
///
/// 键是「过滤文本 + 排序」，值是与之一致的可见行号与各列数字标记；滚动与重绘
/// 都只读这份缓存，因此每帧不必重算过滤。
struct RowCache {
    filter: String,
    sort: Option<(usize, bool)>,
    /// 可见行的源要素行号（已排序）。
    rows: Vec<usize>,
    /// 各列是否按数字右对齐（下标同列号，0 = FID）。
    numeric: Vec<bool>,
}

/// 属性表面板的会话状态。
pub(super) struct AttrTable {
    /// 被查看的图层 id（图层被删除后面板自动关闭）。
    layer: u64,
    /// 过滤文本：对所有字段做不区分大小写的子串匹配。
    filter: String,
    /// 排序：(列号, 是否升序)。列号 0 = FID，1..=字段数 = 字段。
    sort: Option<(usize, bool)>,
    /// 选中的源要素行（`Layer::attrs` 的下标，与 1 基 FID 展示无关）。
    selected: Option<usize>,
    /// 过滤 + 排序结果缓存。
    cache: Option<RowCache>,
    /// 各列宽（首列 FID + 各字段）：进入面板时按内容估一次，之后允许拖动改。
    widths: Vec<f32>,
    /// true = 分离成独立系统窗口，false = 停靠在地图下方（对标 QGIS 属性表的「分离 / 停靠」）。
    detached: bool,
    /// 分离窗口的首帧标记：只有首帧设置窗口尺寸并抢焦点，之后交给系统。
    floating_first_frame: bool,
}

impl AttrTable {
    /// 为图层打开属性表（只负责状态，是否合法由调用方保证）。
    pub(super) fn new(layer: u64) -> Self {
        Self {
            layer,
            filter: String::new(),
            sort: None,
            selected: None,
            cache: None,
            widths: Vec::new(),
            detached: false,
            floating_first_frame: true,
        }
    }

    /// 过滤 / 排序结果缓存失效时重算。
    fn refresh(&mut self, layer: &Layer) {
        let up_to_date = self
            .cache
            .as_ref()
            .is_some_and(|cache| cache.filter == self.filter && cache.sort == self.sort);
        if up_to_date {
            return;
        }
        let needle = self.filter.trim().to_lowercase();
        let numeric = numeric_columns(layer);
        let mut rows: Vec<usize> = (0..layer.attrs.len())
            .filter(|&row| needle.is_empty() || row_matches(layer, row, &needle))
            .collect();
        if let Some((column, ascending)) = self.sort {
            rows.sort_by(|&a, &b| {
                let order = compare(layer, column, a, b, &numeric);
                if ascending { order } else { order.reverse() }
            });
        }
        self.cache = Some(RowCache {
            filter: self.filter.clone(),
            sort: self.sort,
            rows,
            numeric,
        });
    }

    /// 过滤后的行号（缓存未建立时为空）。
    fn rows(&self) -> &[usize] {
        self.cache
            .as_ref()
            .map_or(&[], |cache| cache.rows.as_slice())
    }

    fn row_count(&self) -> usize {
        self.rows().len()
    }

    /// 第 `index` 条可见行对应的源要素行号。
    fn row_at(&self, index: usize) -> Option<usize> {
        self.rows().get(index).copied()
    }

    /// 各列是否按数字右对齐（下标同列号，0 = FID）。
    fn numeric(&self) -> &[bool] {
        self.cache
            .as_ref()
            .map_or(&[], |cache| cache.numeric.as_slice())
    }
}

impl RustGisApp {
    /// 打开底部属性表（对标 QGIS 的 Open Attribute Table）。
    ///
    /// 同一时刻只保留一个属性表：再次打开同一个图层不会重置过滤 / 排序 / 列宽。
    /// 调用方保证 `id` 是矢量图层。
    pub(super) fn open_attr_table(&mut self, id: u64) {
        if self.attr_table.as_ref().is_some_and(|t| t.layer == id) {
            // 同一图层再次打开：保留过滤 / 排序 / 列宽 / 分离状态。
            self.layers.select(id);
            return;
        }
        // 换图层时继承「是否已分离」：分离窗口不因为换了图层又跳回底部停靠。
        let detached = self.attr_table.as_ref().is_some_and(|t| t.detached);
        let mut table = AttrTable::new(id);
        table.detached = detached;
        table.floating_first_frame = !detached; // 复用同一个窗口时不要再设一次尺寸
        self.attr_table = Some(table);
        self.layers.select(id);
    }

    /// 绘制属性表：分离时是独立系统窗口，否则停靠在底部。每帧无条件调用。
    pub(super) fn show_attr_table(&mut self, ctx: &egui::Context) {
        let Some(mut table) = self.attr_table.take() else {
            return;
        };
        // 图层可能已被删除 / 被工程替换：面板随之关闭。
        let Some(node) = self.layers.node(table.layer) else {
            return;
        };
        let Some(layer) = node.layer() else {
            return;
        };
        let lang = self.language;
        let heading = lang.tf("attr.title", &[node.name()]);

        if table.widths.len() != layer.fields.len() + 1 {
            table.widths = base_widths(layer);
        }
        table.refresh(layer);

        let mut outcome = Outcome::default();
        if table.detached {
            show_floating(ctx, &mut table, layer, &heading, &lang, &mut outcome);
        } else {
            show_panel(ctx, &mut table, layer, &heading, &lang, &mut outcome);
        }

        // 双击 / 上下文菜单「缩放至要素」：沿用既有的排队机制，下一帧地图再适配。
        if let Some(bounds) = outcome.zoom {
            let shown = match self.project_projection.as_ref() {
                Some(project) => display_aabb(bounds, &layer.projection, project),
                None => bounds,
            };
            self.zoom_extent = Some(shown);
        }
        if !outcome.close {
            self.attr_table = Some(table); // 用户没点关闭：放回框架，下一帧继续画
        }
    }
}

/// 本帧面板产生的动作（面板内部不能直接改框架，先攒起来）。
#[derive(Default)]
struct Outcome {
    close: bool,
    zoom: Option<Aabb>,
}

/// 把面板画出来。
/// 停靠模式：地图下方的可拖动面板。
fn show_panel(
    ctx: &egui::Context,
    table: &mut AttrTable,
    layer: &Layer,
    heading: &str,
    lang: &Language,
    outcome: &mut Outcome,
) {
    let panel = TopBottomPanel::bottom("attribute_table")
        .resizable(true)
        .default_height(240.0)
        .height_range(120.0..=(ctx.available_rect().height() * 0.7).max(160.0))
        .frame(
            egui::Frame::side_top_panel(&ctx.style())
                .fill(PANEL_BG)
                .inner_margin(egui::Margin::ZERO),
        );

    panel.show(ctx, |ui| {
        contents_ui(ui, table, layer, heading, lang, outcome)
    });
}

/// 分离模式：独立的系统窗口（原生标题栏 / 可拖动 / 可缩放，关闭即收起属性表）。
///
/// 对标 QGIS 属性表右上角的「分离」：窗口只在本函数被调用时存在，因此把
/// `table.detached` 改回 false 就等于把窗口关掉、内容回到地图下方的停靠面板。
fn show_floating(
    ctx: &egui::Context,
    table: &mut AttrTable,
    layer: &Layer,
    heading: &str,
    lang: &Language,
    outcome: &mut Outcome,
) {
    let mut builder = ViewportBuilder::default()
        .with_title(heading)
        .with_min_inner_size([WINDOW_MIN_W, WINDOW_MIN_H])
        .with_resizable(true)
        .with_close_button(true);
    if table.floating_first_frame {
        table.floating_first_frame = false;
        builder = builder
            .with_inner_size([WINDOW_W, WINDOW_H])
            .with_active(true)
            .with_app_id("rustgis-attribute-table");
    }

    let mut close_requested = false;
    ctx.show_viewport_immediate(window_id(), builder, |ctx, _class| {
        // 系统标题栏的 ✕ 等价于关闭属性表（与面板里的关闭按钮一致）。
        if ctx.input(|i| i.viewport().close_requested()) {
            close_requested = true;
        }
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(PANEL_BG)
                    .inner_margin(egui::Margin::ZERO),
            )
            .show(ctx, |ui| {
                contents_ui(ui, table, layer, heading, lang, outcome)
            });
    });
    if close_requested {
        outcome.close = true;
    }
}

/// 面板 / 窗口共用的内容：工具条 + 深色表头 + 数据行 + 状态条。
fn contents_ui(
    ui: &mut Ui,
    table: &mut AttrTable,
    layer: &Layer,
    heading: &str,
    lang: &Language,
    outcome: &mut Outcome,
) {
    ui.spacing_mut().item_spacing = Vec2::ZERO;
    toolbar_ui(ui, table, heading, lang, outcome);
    ui.painter().hline(
        ui.max_rect().x_range(),
        ui.cursor().top(),
        Stroke::new(1.0_f32, style::BORDER),
    );

    let grid_h = (ui.available_height() - STATUS_H).max(0.0);
    let width = ui.available_width();
    ui.allocate_ui_with_layout(
        Vec2::new(width, grid_h),
        Layout::top_down(Align::Min),
        |ui| {
            grid_ui(ui, table, layer, lang, &mut outcome.zoom);
        },
    );
    status_ui(ui, table, layer, lang);
}

// ─────────────────────── 工具栏 ───────────────────────

/// 顶部工具条：图标 + 标题 + 只读标记 + 过滤框 + 分离 / 关闭按钮。
fn toolbar_ui(
    ui: &mut Ui,
    table: &mut AttrTable,
    heading: &str,
    lang: &Language,
    outcome: &mut Outcome,
) {
    ui.allocate_ui_with_layout(
        Vec2::new(ui.available_width(), 26.0),
        Layout::left_to_right(Align::Center),
        |ui| {
            ui.add_space(8.0);
            let (glyph, _) = ui.allocate_exact_size(Vec2::splat(14.0), Sense::hover());
            draw_table_glyph(ui.painter(), glyph);
            ui.add_space(3.0);
            ui.label(RichText::new(heading).strong().size(12.0));
            ui.add_space(6.0);
            read_only_badge(ui, lang.t("attr.readOnly"));
            ui.add_space(10.0);

            let filter = ui.add(
                TextEdit::singleline(&mut table.filter)
                    .hint_text(lang.t("attr.filterHint"))
                    .desired_width(200.0)
                    .margin(egui::Margin::symmetric(6, 2))
                    .font(FontId::proportional(11.5)),
            );
            if filter.changed() {
                table.selected = None;
            }
            if !table.filter.is_empty() {
                ui.add_space(4.0);
                let clear =
                    egui::Button::new(RichText::new(lang.t("attr.clear")).size(10.5)).frame(false);
                if ui.add(clear).clicked() {
                    table.filter.clear();
                }
            }

            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.add_space(4.0);
                if icon_button(ui, lang.t("attr.close"), true, draw_close_glyph).clicked() {
                    outcome.close = true;
                }
                // 分离 / 停靠：图标与提示语随当前形态切换。
                let detach = if table.detached {
                    icon_button(ui, lang.t("attr.dock"), true, draw_dock_glyph)
                } else {
                    icon_button(ui, lang.t("attr.detach"), true, draw_detach_glyph)
                };
                if detach.clicked() {
                    table.detached = !table.detached;
                    if table.detached {
                        table.floating_first_frame = true;
                    }
                }
            });
        },
    );
}

/// 「分离为独立窗口」图标：窗体轮廓 + 指向右上角的箭头。
fn draw_detach_glyph(painter: &egui::Painter, rect: Rect, color: Color32) {
    let stroke = Stroke::new(1.0_f32, color);
    let body = Rect::from_min_max(
        rect.min + Vec2::new(1.0, 7.0),
        rect.max - Vec2::new(6.0, 1.0),
    );
    painter.rect_stroke(body, 1.0, stroke, egui::StrokeKind::Inside);
    // 角上的箭头：斜线 + 两条组成箭头的短线。
    let tip = Pos2::new(rect.right() - 2.0, rect.top() + 2.0);
    painter.line_segment(
        [Pos2::new(body.right() - 3.0, body.top() + 3.0), tip],
        stroke,
    );
    painter.line_segment([tip, Pos2::new(tip.x - 5.0, tip.y)], stroke);
    painter.line_segment([tip, Pos2::new(tip.x, tip.y + 5.0)], stroke);
}

/// 「停靠回底部」图标：窗体轮廓 + 实心底部横条。
fn draw_dock_glyph(painter: &egui::Painter, rect: Rect, color: Color32) {
    let stroke = Stroke::new(1.0_f32, color);
    let body = rect.shrink(3.0);
    painter.rect_stroke(body, 1.0, stroke, egui::StrokeKind::Inside);
    let band = Rect::from_min_max(
        Pos2::new(body.left() + 1.0, body.bottom() - 4.5),
        Pos2::new(body.right() - 1.0, body.bottom() - 1.0),
    );
    painter.rect_filled(band, 0.5, color);
}

/// 关闭图标：两笔斜线（比 ✕ 字形更稳，不依赖字体里有没有该字符）。
fn draw_close_glyph(painter: &egui::Painter, rect: Rect, color: Color32) {
    let stroke = Stroke::new(1.2_f32, color);
    let body = rect.shrink(6.0);
    painter.line_segment([body.left_top(), body.right_bottom()], stroke);
    painter.line_segment([body.right_top(), body.left_bottom()], stroke);
}

/// 「只读」小徽标：浅灰底 + 细边框 + 描边字。
fn read_only_badge(ui: &mut Ui, text: &str) {
    let color = Color32::from_rgb(96, 104, 112);
    let font = FontId::proportional(9.0);
    let galley = ui.painter().layout_no_wrap(text.to_string(), font, color);
    let size = Vec2::new(galley.size().x + 10.0, galley.size().y + 4.0);
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 2.0, Color32::from_rgb(238, 241, 244));
    painter.rect_stroke(
        rect,
        2.0,
        Stroke::new(1.0_f32, Color32::from_rgb(196, 203, 210)),
        egui::StrokeKind::Inside,
    );
    painter.galley(
        Pos2::new(rect.left() + 5.0, rect.center().y - galley.size().y * 0.5),
        galley,
        color,
    );
}

/// 属性表图标：表头 + 两列网格的线性图标。
fn draw_table_glyph(painter: &egui::Painter, rect: Rect) {
    let stroke = Stroke::new(1.0_f32, Color32::from_rgb(70, 82, 94));
    painter.rect_stroke(rect, 1.0, stroke, egui::StrokeKind::Inside);
    let header = rect.top() + 4.0;
    painter.hline(rect.x_range(), header, stroke);
    for i in 1..3 {
        let x = rect.left() + rect.width() / 3.0 * i as f32;
        painter.vline(x, egui::Rangef::new(header, rect.bottom()), stroke);
    }
}

// ─────────────────────── 数据区 ───────────────────────

/// 表头 + 虚拟滚动的数据行。
fn grid_ui(
    ui: &mut Ui,
    table: &mut AttrTable,
    layer: &Layer,
    lang: &Language,
    zoom: &mut Option<Aabb>,
) {
    table.refresh(layer);
    let row_count = table.row_count();
    let numeric: Vec<bool> = table.numeric().to_vec();
    let widths = display_widths(&table.widths, ui.available_width());
    let total_w: f32 = widths.iter().sum();
    let stretched = table.widths.iter().sum::<f32>() < ui.available_width() - 0.5;

    egui::ScrollArea::horizontal()
        .id_salt(("attr_h", table.layer))
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.set_min_width(total_w);
            header_ui(ui, table, layer, &widths, total_w, stretched, lang);
            if row_count == 0 {
                let message = if layer.attrs.is_empty() {
                    lang.t("attr.empty")
                } else {
                    lang.t("attr.noMatch")
                };
                let (rect, _) = ui.allocate_exact_size(Vec2::new(total_w, 48.0), Sense::hover());
                ui.painter().text(
                    rect.center(),
                    Align2::CENTER_CENTER,
                    message,
                    FontId::proportional(12.0),
                    style::TEXT_WEAK,
                );
                return;
            }
            let height = ui.available_height();
            egui::ScrollArea::vertical()
                .id_salt(("attr_v", table.layer))
                .auto_shrink([false, false])
                .max_height(height)
                .show_rows(ui, ROW_H, row_count, |ui, range| {
                    for index in range {
                        if let Some(row) = table.row_at(index) {
                            draw_row(
                                ui,
                                table,
                                layer,
                                row,
                                index % 2 == 1,
                                &widths,
                                &numeric,
                                lang,
                                zoom,
                            );
                        }
                    }
                });
        });
}

/// 深色表头：列名 + 排序箭头 + 列宽拖动柄。
fn header_ui(
    ui: &mut Ui,
    table: &mut AttrTable,
    layer: &Layer,
    widths: &[f32],
    total_w: f32,
    stretched: bool,
    lang: &Language,
) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(total_w, HEADER_H), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 0.0, HEADER_BG);

    let font = FontId::proportional(CELL_FONT_SIZE);
    let mut x = rect.left();
    for (column, width) in widths.iter().enumerate() {
        let cell = Rect::from_min_size(Pos2::new(x, rect.top()), Vec2::new(*width, HEADER_H));
        let label = if column == 0 {
            lang.t("attr.fid").to_string()
        } else {
            layer.fields.get(column - 1).cloned().unwrap_or_default()
        };

        let clip = painter.with_clip_rect(Rect::from_min_max(
            cell.min,
            Pos2::new(cell.right() - 4.0, cell.bottom()),
        ));
        clip.text(
            Pos2::new(cell.left() + CELL_PAD, cell.center().y),
            Align2::LEFT_CENTER,
            &label,
            font.clone(),
            HEADER_TEXT,
        );
        if let Some((sorted, ascending)) = table.sort
            && sorted == column
        {
            clip.text(
                Pos2::new(
                    cell.left() + CELL_PAD + text_width(&label, CELL_FONT_SIZE),
                    cell.center().y,
                ),
                Align2::LEFT_CENTER,
                if ascending { "▲" } else { "▼" },
                FontId::proportional(8.0),
                SORT_ARROW,
            );
        }
        painter.vline(
            cell.right(),
            rect.y_range(),
            Stroke::new(1.0_f32, HEADER_SEP),
        );

        // 点击列头排序（列宽拖动柄占最右 5px，不参与排序）。
        let body = Rect::from_min_max(cell.min, Pos2::new(cell.right() - 3.0, cell.bottom()));
        let sort_resp = ui.interact(body, ui.id().with(("attr.sort", column)), Sense::click());
        if sort_resp.clicked() {
            table.sort = match table.sort {
                Some((sorted, ascending)) if sorted == column => Some((column, !ascending)),
                _ => Some((column, true)),
            };
        }

        // 右边界拖动改列宽；末列在「填满」模式下由面板宽度分摊，不响应拖动。
        if column + 1 < widths.len() || !stretched {
            let handle = Rect::from_min_max(
                Pos2::new(cell.right() - 3.0, cell.top()),
                Pos2::new(cell.right() + 2.0, cell.bottom()),
            );
            let resize = ui.interact(
                handle,
                ui.id().with(("attr.resize", column)),
                Sense::click_and_drag(),
            );
            if resize.hovered() || resize.dragged() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
            }
            // 「填满面板」模式下拖动先把展示宽度落成基准宽度，避免拖动时被反复均摊而发飘。
            if resize.drag_started() {
                table.widths = widths.to_vec();
            }
            if resize.dragged()
                && let Some(current) = table.widths.get_mut(column)
            {
                *current = (*current + resize.drag_delta().x).clamp(MIN_COL_W, MAX_COL_W);
            }
        }
        x += *width;
    }
}

/// 一条数据行：斑马纹 / 悬停 / 选中底、单元格文字与网格、点击与双击行为。
#[allow(clippy::too_many_arguments)]
fn draw_row(
    ui: &mut Ui,
    table: &mut AttrTable,
    layer: &Layer,
    row: usize,
    zebra: bool,
    widths: &[f32],
    numeric: &[bool],
    lang: &Language,
    zoom: &mut Option<Aabb>,
) {
    let total_w: f32 = widths.iter().sum();
    let (rect, response) = ui.allocate_exact_size(Vec2::new(total_w, ROW_H), Sense::click());
    let painter = ui.painter();
    let selected = table.selected == Some(row);

    if selected {
        painter.rect_filled(rect, 0.0, ROW_SELECTED_BG);
    } else if response.hovered() {
        painter.rect_filled(rect, 0.0, ROW_HOVER);
    } else if zebra {
        painter.rect_filled(rect, 0.0, ZEBRA);
    }

    let mut x = rect.left();
    for width in widths {
        x += *width;
        painter.vline(x, rect.y_range(), Stroke::new(1.0_f32, GRID));
    }
    painter.hline(
        rect.x_range(),
        rect.bottom() - 0.5,
        Stroke::new(1.0_f32, GRID),
    );
    if selected {
        painter.rect_stroke(
            rect.shrink(0.5),
            0.0,
            Stroke::new(1.0_f32, ROW_SELECTED_EDGE),
            egui::StrokeKind::Inside,
        );
    }

    let font = FontId::proportional(CELL_FONT_SIZE);
    let mut x = rect.left();
    for (column, width) in widths.iter().enumerate() {
        let cell = Rect::from_min_size(Pos2::new(x, rect.top()), Vec2::new(*width, ROW_H));
        let clip = painter.with_clip_rect(cell);
        if column == 0 {
            clip.text(
                Pos2::new(cell.right() - CELL_PAD, cell.center().y),
                Align2::RIGHT_CENTER,
                (row + 1).to_string(),
                font.clone(),
                style::TEXT_WEAK,
            );
        } else {
            let text = field_value(layer, column - 1, row);
            if numeric.get(column).copied().unwrap_or(false) {
                clip.text(
                    Pos2::new(cell.right() - CELL_PAD, cell.center().y),
                    Align2::RIGHT_CENTER,
                    text,
                    font.clone(),
                    CELL_TEXT,
                );
            } else {
                clip.text(
                    Pos2::new(cell.left() + CELL_PAD, cell.center().y),
                    Align2::LEFT_CENTER,
                    text,
                    font.clone(),
                    CELL_TEXT,
                );
            }
        }
        x += *width;
    }

    if response.clicked() {
        table.selected = Some(row);
    }
    if response.double_clicked()
        && let Some(bounds) = layer.feature_bounds(row)
    {
        *zoom = Some(bounds);
    }
    response.context_menu(|ui| {
        if ui.button(lang.t("attr.zoomToFeature")).clicked() {
            if let Some(bounds) = layer.feature_bounds(row) {
                *zoom = Some(bounds);
            }
            ui.close_menu();
        }
        if ui.button(lang.t("attr.copyRow")).clicked() {
            ui.ctx().copy_text(row_text(layer, row));
            ui.close_menu();
        }
    });
}

/// 底部状态条：记录数 / 字段数 + 选中 / 提示。
fn status_ui(ui: &mut Ui, table: &AttrTable, layer: &Layer, lang: &Language) {
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, STATUS_H), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 0.0, PANEL_BG);
    painter.hline(
        rect.x_range(),
        rect.top() + 0.5,
        Stroke::new(1.0_f32, style::BORDER),
    );

    let footer = lang.tf(
        "attr.footer",
        &[
            &table.row_count().to_string(),
            &layer.attrs.len().to_string(),
            &layer.fields.len().to_string(),
        ],
    );
    painter.text(
        Pos2::new(rect.left() + 8.0, rect.center().y),
        Align2::LEFT_CENTER,
        footer,
        FontId::monospace(10.5),
        style::TEXT_WEAK,
    );

    let right = match table.selected {
        Some(row) => lang.tf("attr.selected", &[&(row + 1).to_string()]),
        None => lang.t("attr.hint").to_string(),
    };
    painter.text(
        Pos2::new(rect.right() - 8.0, rect.center().y),
        Align2::RIGHT_CENTER,
        right,
        FontId::proportional(10.5),
        style::TEXT_WEAK,
    );
}

// ─────────────────────── 列宽 / 取数 ───────────────────────

/// 进面板时的初始列宽：FID 固定，其余按「字段名 + 前 20 行最宽值」估。
fn base_widths(layer: &Layer) -> Vec<f32> {
    let mut widths = Vec::with_capacity(layer.fields.len() + 1);
    widths.push(FID_W);
    for (column, name) in layer.fields.iter().enumerate() {
        let mut chars = name.chars().count();
        for row in layer.attrs.iter().take(20) {
            if let Some(value) = row.get(column) {
                chars = chars.max(value.chars().count());
            }
        }
        widths.push((chars as f32 * CHAR_W + CELL_PAD * 2.0).clamp(MIN_COL_W, MAX_COL_W));
    }
    widths
}

/// 实际绘制的列宽：总宽不足面板宽度时，把差额均摊给各列（末列随之拉满）。
fn display_widths(widths: &[f32], available: f32) -> Vec<f32> {
    let total: f32 = widths.iter().sum();
    if total >= available || widths.is_empty() {
        return widths.to_vec();
    }
    let extra = (available - total) / widths.len() as f32;
    widths.iter().map(|width| width + extra).collect()
}

/// 逐列判断是否「全为数字」，供右对齐与数值排序使用。
fn numeric_columns(layer: &Layer) -> Vec<bool> {
    (0..=layer.fields.len())
        .map(|column| {
            if column == 0 || layer.attrs.is_empty() {
                return false; // FID 按源顺序
            }
            (0..layer.attrs.len()).all(|row| {
                let value = field_value(layer, column - 1, row).trim();
                !value.is_empty() && value.parse::<f64>().is_ok()
            })
        })
        .collect()
}

/// 第 `row` 行第 `field` 个字段的值（越界 / 缺失一律空串）。
fn field_value(layer: &Layer, field: usize, row: usize) -> &str {
    layer
        .attrs
        .get(row)
        .and_then(|values| values.get(field))
        .map_or("", String::as_str)
}

/// 过滤匹配：任一字段（或 FID 文本）包含 `needle`（已小写）。
fn row_matches(layer: &Layer, row: usize, needle: &str) -> bool {
    if (row + 1).to_string().contains(needle) {
        return true;
    }
    layer.attrs.get(row).is_some_and(|values| {
        values
            .iter()
            .any(|value| value.to_lowercase().contains(needle))
    })
}

/// 两行在第 `column` 列的比较（数字列按数值，其余按不区分大小写的字典序）。
fn compare(
    layer: &Layer,
    column: usize,
    a: usize,
    b: usize,
    numeric: &[bool],
) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    if column == 0 {
        return a.cmp(&b);
    }
    let left = field_value(layer, column - 1, a);
    let right = field_value(layer, column - 1, b);
    let order = if numeric.get(column).copied().unwrap_or(false) {
        let parse = |text: &str| text.trim().parse::<f64>().unwrap_or(f64::NAN);
        parse(left)
            .partial_cmp(&parse(right))
            .unwrap_or(Ordering::Equal)
    } else {
        left.to_lowercase().cmp(&right.to_lowercase())
    };
    order.then_with(|| a.cmp(&b))
}

/// 整行文本（制表符分隔）：FID + 各字段值，便于粘到表格软件里。
fn row_text(layer: &Layer, row: usize) -> String {
    let mut out = (row + 1).to_string();
    if let Some(values) = layer.attrs.get(row) {
        for value in values {
            out.push('\t');
            out.push_str(value);
        }
    }
    out
}

/// 估算文本宽度（按字符数），只用于列宽与排序箭头定位。
fn text_width(text: &str, size: f32) -> f32 {
    text.chars().count() as f32 * size * 0.62
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geo::PolygonData;

    /// 边长 1 的正方形（首尾闭合）。
    fn square(x: f64, y: f64) -> PolygonData {
        PolygonData {
            outer: vec![
                [x, y],
                [x + 1.0, y],
                [x + 1.0, y + 1.0],
                [x, y + 1.0],
                [x, y],
            ],
            holes: Vec::new(),
        }
    }

    /// 4 个正方形要素 + 两个字段（城市名、人口）。
    fn test_layer() -> Layer {
        let mut layer = Layer::from_geometry(
            "cities".to_string(),
            Vec::new(),
            Vec::new(),
            vec![
                square(0.0, 0.0),
                square(2.0, 0.0),
                square(4.0, 0.0),
                square(6.0, 0.0),
            ],
        );
        layer.fields = vec!["name".to_string(), "pop".to_string()];
        layer.attrs = vec![
            vec!["BeiJing".to_string(), "900".to_string()],
            vec!["ShangHai".to_string(), "2400".to_string()],
            vec!["BeiJing2".to_string(), "1200".to_string()],
            vec!["GuangZhou".to_string(), "700".to_string()],
        ];
        layer
    }

    #[test]
    fn rows_are_filtered_and_sorted() {
        let layer = test_layer();
        let mut table = AttrTable::new(1);

        table.refresh(&layer);
        assert_eq!(table.row_count(), 4);
        assert_eq!(table.row_at(2), Some(2));

        // 不区分大小写的子串过滤：命中 BeiJing 与 BeiJing2。
        table.filter = "beiJING".to_string();
        table.refresh(&layer);
        assert_eq!(table.rows(), &[0, 2]);

        // 数字列降序：2400 / 1200 / 900 / 700。
        table.filter.clear();
        table.sort = Some((2, false));
        table.refresh(&layer);
        assert_eq!(table.rows(), &[1, 2, 0, 3]);

        // 文本列升序。
        table.sort = Some((1, true));
        table.refresh(&layer);
        assert_eq!(table.rows(), &[0, 2, 3, 1]);

        // 过滤把选中行隐藏后也不清空选中（只是不再高亮）。
        table.selected = Some(1);
        table.filter = "guang".to_string();
        table.refresh(&layer);
        assert_eq!(table.rows(), &[3]);
        assert_eq!(table.selected, Some(1));
    }

    #[test]
    fn numeric_columns_drive_right_alignment() {
        let layer = test_layer();
        let numeric = numeric_columns(&layer);
        assert_eq!(numeric, vec![false, false, true]);
        assert_eq!(field_value(&layer, 1, 1), "2400");
        assert_eq!(field_value(&layer, 9, 9), "");
        assert_eq!(row_text(&layer, 0), "1\tBeiJing\t900");
    }

    #[test]
    fn feature_rows_map_to_their_bounds() {
        let layer = test_layer();
        let bounds = layer.feature_bounds(1).expect("第 2 个要素有几何");
        assert_eq!(bounds.min, [2.0, 0.0]);
        assert_eq!(bounds.max, [3.0, 1.0]);
    }

    #[test]
    fn panel_renders_headless() {
        let ctx = egui::Context::default();
        let mut app = RustGisApp::new(&ctx);
        let id = app.layers.add(test_layer());
        app.attr_table = Some(AttrTable::new(id));
        let input = || egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1280.0, 800.0))),
            ..Default::default()
        };

        let _ = ctx.run(input(), |ctx| app.show_attr_table(ctx));
        assert_eq!(
            app.attr_table.as_ref().expect("面板保持打开").row_count(),
            4
        );

        // 下一帧带过滤条件：行数收窄，面板仍然可绘制。
        app.attr_table.as_mut().unwrap().filter = "bei".to_string();
        let _ = ctx.run(input(), |ctx| app.show_attr_table(ctx));
        assert_eq!(app.attr_table.as_ref().unwrap().row_count(), 2);

        // 关掉面板后不再占用底部空间。
        app.attr_table = None;
        let _ = ctx.run(input(), |ctx| app.show_attr_table(ctx));
        assert!(app.attr_table.is_none());
    }

    #[test]
    fn detached_table_renders_in_its_own_viewport() {
        let ctx = egui::Context::default();
        let mut app = RustGisApp::new(&ctx);
        let id = app.layers.add(test_layer());
        app.attr_table = Some(AttrTable::new(id));
        let input = || egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1280.0, 800.0))),
            ..Default::default()
        };

        // 分离：属性表离开底部面板，改为独立窗口绘制。
        app.attr_table.as_mut().unwrap().detached = true;
        let _ = ctx.run(input(), |ctx| app.show_attr_table(ctx));
        let table = app.attr_table.as_ref().expect("分离窗口里的属性表保持打开");
        assert!(table.detached);
        assert_eq!(table.row_count(), 4);
        // 首帧标记只消耗一次，之后窗口尺寸交给系统（用户可自由缩放）。
        assert!(!table.floating_first_frame);

        // 过滤与停靠互不影响：分离状态下改过滤仍然生效。
        app.attr_table.as_mut().unwrap().filter = "bei".to_string();
        let _ = ctx.run(input(), |ctx| app.show_attr_table(ctx));
        assert_eq!(app.attr_table.as_ref().unwrap().row_count(), 2);

        // 停靠回去：下一帧回到地图下方的面板，内容与过滤条件都保留。
        app.attr_table.as_mut().unwrap().detached = false;
        let _ = ctx.run(input(), |ctx| app.show_attr_table(ctx));
        let table = app.attr_table.as_ref().expect("停靠后属性表仍打开");
        assert!(!table.detached);
        assert_eq!(table.row_count(), 2);
    }

    #[test]
    fn opening_another_layer_keeps_the_detached_window() {
        let ctx = egui::Context::default();
        let mut app = RustGisApp::new(&ctx);
        let first = app.layers.add(test_layer());
        let second = app.layers.add(test_layer());
        app.open_attr_table(first);
        app.attr_table.as_mut().unwrap().detached = true;

        // 换图层：沿用同一个分离窗口，不能又跳回底部停靠。
        app.open_attr_table(second);
        let table = app.attr_table.as_ref().expect("换图层后属性表仍打开");
        assert_eq!(table.layer, second);
        assert!(table.detached);
        assert!(!table.floating_first_frame);
    }
}
