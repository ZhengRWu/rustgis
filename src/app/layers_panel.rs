//! 左侧「内容列表」（TOC）面板：图层树 + 拖拽重排 + 右键菜单。
//!
//! 对标 ArcGIS 的 Table Of Contents。布局按截图组织：顶部是一排图标工具条，
//! 下面是「图层」根节点，再往下是逐级缩进的图层树；每一行都由
//! 「+/- 折叠框 → 勾选框 → 图例符号 → 名称」四段组成。
//!
//! 行主体（折叠框 / 勾选框以外的区域）支持**多选**，与文件管理器一致：
//!
//! | 操作 | 效果 |
//! | --- | --- |
//! | 左键单击 | 只选这一项 |
//! | Ctrl + 左键单击 | 逐个加选 / 取消，可挑不连续的多项 |
//! | Shift + 左键单击 | 选中上次锚点到这一项之间的连续一段 |
//! | 右键「移除」或按 R | 移除整个选择集（右键在已选中的行上）或这一行 |
//!
//! 选择状态存在 [`crate::geo::LayerManager`] 里：集合末位是「主选择项」，
//! 属性表、查找这类单选语义读它。
//!
//! 行布局是**像素级手算**的（缩进、列宽、命中区域），[`mod tests`] 里用无头
//! `Context` 跑了完整的布局与拖拽时序回归，改动本文件后必须先过那些测试。
//! 基础图标（加号 / 文件夹 / 折叠框 / 勾选框等）在 [`super::widgets`]。

use egui::{
    Align2, Color32, FontId, PointerButton, Pos2, Rect, RichText, Sense, Stroke, StrokeKind, Ui,
    Vec2,
};

use crate::geo;
use crate::style;

use super::RustGisApp;
use super::widgets::{
    draw_box_sign, draw_checkbox, draw_expander, draw_group_icon, draw_layers_glyph, draw_plus,
    icon_button,
};

// ─────────────────────── 内容列表（TOC）布局常量 ───────────────────────

/// 单行高度。
const ROW_H: f32 = 22.0;
/// 每深一级的缩进像素。
const INDENT: f32 = 14.0;
/// 折叠框（+ / −）边长。
const EXPANDER_SIZE: f32 = 10.0;
/// 勾选框边长。
const CHECKBOX_SIZE: f32 = 13.0;
/// 图例符号 / 组图标的绘制区边长。
const GLYPH_SIZE: f32 = 15.0;

/// 内容列表单行所需的展示数据（先行克隆，避免与 `ui` 的借用冲突）。
struct Row {
    id: u64,
    name: String,
    /// 缩进深度（顶层节点为 0）。
    depth: usize,
    is_group: bool,
    expanded: bool,
    has_children: bool,
    /// 自身勾选状态。
    visible: bool,
    /// 祖先是否全部勾选；`false` 时整行弱化显示。
    ancestors_visible: bool,
    /// 父组 id 与在父组中的下标（拖拽落点用）。
    parent: Option<u64>,
    index: usize,
    /// 叶子图层的图例符号类型（组为 `None`）。
    kind: Option<geo::LegendKind>,
    fill: Color32,
    outline: Color32,
    point_color: Color32,
    point_radius: f32,
}

/// 一行的拖拽落点信息（行矩形 + 归属父组 + 下标）。
struct RowMeta {
    id: u64,
    rect: Rect,
    parent: Option<u64>,
    index: usize,
    is_group: bool,
}

/// 绘制图例符号（白底画布配色）：面 = 半透明填充块 + 边框；线 = 斜线段；
/// 点 = 圆点；组 = 文件夹图标。
fn draw_legend_symbol(painter: &egui::Painter, rect: Rect, row: &Row) {
    match row.kind {
        Some(geo::LegendKind::Polygon) => {
            painter.rect_filled(rect, 0.0, row.fill);
            painter.rect_stroke(
                rect,
                0.0,
                Stroke::new(1.0_f32, row.outline),
                StrokeKind::Middle,
            );
        }
        Some(geo::LegendKind::Line) => {
            let p1 = Pos2::new(rect.min.x + 1.0, rect.max.y - 2.0);
            let p2 = Pos2::new(rect.max.x - 1.0, rect.min.y + 2.0);
            painter.line_segment([p1, p2], Stroke::new(2.0_f32, row.outline));
        }
        Some(geo::LegendKind::Point) => {
            painter.circle_filled(rect.center(), row.point_radius, row.point_color);
        }
        Some(geo::LegendKind::Raster) => {
            // 栅格用「彩色小方格」示意：左红右绿，中间一小片蓝，贴近 RGB 合成观感。
            let third = rect.width() / 3.0;
            let left = Rect::from_min_size(rect.min, Vec2::new(third, rect.height()));
            let mid = Rect::from_min_size(
                Pos2::new(rect.min.x + third, rect.min.y),
                Vec2::new(third, rect.height()),
            );
            let right = Rect::from_min_size(
                Pos2::new(rect.min.x + third * 2.0, rect.min.y),
                Vec2::new(third, rect.height()),
            );
            painter.rect_filled(left, 0.0, Color32::from_rgb(198, 92, 74));
            painter.rect_filled(mid, 0.0, Color32::from_rgb(96, 158, 92));
            painter.rect_filled(right, 0.0, Color32::from_rgb(84, 110, 168));
            painter.rect_stroke(
                rect,
                0.0,
                Stroke::new(1.0_f32, style::GROUP_ICON),
                StrokeKind::Inside,
            );
        }
        None => draw_group_icon(painter, rect, style::GROUP_ICON),
    }
}

/// 拖拽落点：返回 (父组 id，下标)。落在根节点行上 = 移到顶层最前。
fn drop_target(root_rect: Rect, metas: &[RowMeta], pos: Pos2) -> Option<(Option<u64>, usize)> {
    if root_rect.contains(pos) {
        return Some((None, 0));
    }
    let meta = metas.iter().find(|m| m.rect.contains(pos))?;
    // 落在组上 → 放进该组最上层；落在图层上 → 插到该图层所在位置。
    if meta.is_group {
        Some((Some(meta.id), 0))
    } else {
        Some((meta.parent, meta.index))
    }
}

/// 落点是否合法：把节点放进自身（或自身后代）会成环，`move_node` 会拒绝，
/// 此时指示器也不该画（否则用户看见指示线、松手却什么都没发生）。
fn drop_target_valid(metas: &[RowMeta], drag_id: u64, parent: Option<u64>) -> bool {
    let Some(pid) = parent else {
        return true; // 顶层永远合法
    };
    if pid == drag_id {
        return false;
    }
    // 沿 pid 的父链向上走；能走到 drag_id 说明 pid 是被拖节点的后代。
    // （父链只会经过可见行，而只有展开的组才有可见子行，故链不会断在半空。）
    let mut node = metas.iter().find(|m| m.id == pid).and_then(|m| m.parent);
    while let Some(p) = node {
        if p == drag_id {
            return false;
        }
        node = metas.iter().find(|m| m.id == p).and_then(|m| m.parent);
    }
    true
}

/// 拖拽中的落点指示（真正的移动在鼠标松开时才发生，见 `layers_panel`）：
/// - 落点处有一行（兄弟位置，或展开组的首个子行）→ 在该行顶边画横线；
/// - 落进折叠 / 空的组 → 高亮组行本身；
/// - 顶层且尚无任何行 → 在根节点行下边画横线。
fn draw_drop_indicator(
    painter: &egui::Painter,
    metas: &[RowMeta],
    root_rect: Rect,
    parent: Option<u64>,
    index: usize,
) {
    let line = |r: Rect, y: f32| {
        painter.line_segment(
            [Pos2::new(r.min.x + 10.0, y), Pos2::new(r.max.x - 10.0, y)],
            Stroke::new(2.0_f32, style::ACCENT),
        );
    };
    // 「插在该位置」对应的锚行：兄弟位置就是那一行本身，「组最上层」是组的首个子行。
    let anchor = metas
        .iter()
        .find(|m| m.parent == parent && m.index == index);
    match (anchor, parent) {
        (Some(row), _) => line(row.rect, row.rect.min.y + 1.0),
        (None, Some(pid)) => {
            if let Some(group) = metas.iter().find(|m| m.id == pid) {
                painter.rect_filled(group.rect, 0.0, style::ACCENT_HOVER_BG);
                painter.rect_stroke(
                    group.rect.shrink(0.5),
                    2.0,
                    Stroke::new(1.0_f32, style::ACCENT_HOVER_STROKE),
                    StrokeKind::Inside,
                );
            }
        }
        (None, None) => line(root_rect, root_rect.max.y - 1.0),
    }
}

/// 顶部菜单栏实际使用的字体（族 + 字号）：`menu_button` 默认走 `TextStyle::Button`，
/// 这里直接取该样式对应的 `FontId`。
///
/// 内容列表根节点与图层名称统一用它，保证与菜单栏字体一致；
/// 日后全局样式调整字号时两边自动同步，不必再手工对齐硬编码值。
fn menu_bar_font(style: &egui::Style) -> FontId {
    style
        .text_styles
        .get(&egui::TextStyle::Button)
        .cloned()
        .unwrap_or_default()
}

impl RustGisApp {
    /// 左侧「内容列表」面板（对标 ArcGIS 的 Table Of Contents）。
    ///
    /// 布局按截图组织：顶部是一排图标工具条，下面是「图层」根节点，再往下是
    /// 逐级缩进的图层树；每一行都由「+/- 折叠框 → 勾选框 → 图例符号 → 名称」
    /// 四段组成，勾选框控制绘制、折叠框控制符号展开。所有图标都是矢量绘制。
    pub(super) fn layers_panel(&mut self, ui: &mut Ui) {
        // ── 标题 + 工具条（右起：折叠全部 / 展开全部 / 新建组 / 添加图层） ──
        ui.horizontal(|ui| {
            // 标题与菜单栏同字体（原 `heading` 走 18px 的 Heading 样式，明显偏大）。
            let font = menu_bar_font(ui.style());
            ui.label(RichText::new(self.t("layers.panel")).font(font));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // 导入进行中：工具条置灰并显示当前阶段提示。
                if let Some(key) = self.import_hint() {
                    ui.weak(self.t(key));
                } else if let Some(key) = self.raster_import_hint() {
                    ui.weak(self.t(key));
                }
                let idle = self.project_idle();

                if icon_button(ui, self.t("layers.collapseAllTip"), idle, |p, r, c| {
                    draw_box_sign(p, r, false, c)
                })
                .clicked()
                {
                    self.layers.set_all_expanded(false);
                }
                if icon_button(ui, self.t("layers.expandAllTip"), idle, |p, r, c| {
                    draw_box_sign(p, r, true, c)
                })
                .clicked()
                {
                    self.layers.set_all_expanded(true);
                }
                if icon_button(ui, self.t("layers.newGroupTip"), idle, |p, r, c| {
                    draw_group_icon(p, r, c)
                })
                .clicked()
                {
                    let name = self.layers.unique_name(self.t("layers.newGroupName"));
                    self.layers.add_group(name);
                    self.dirty = true;
                }
                if icon_button(ui, self.t("layers.addTip"), idle, draw_plus).clicked() {
                    self.pick_and_add_layer();
                }
            });
        });
        ui.separator();

        // 本帧开始时是否已有拖拽在飞：用于区分「拖拽刚开始这一帧」（layer_drag
        // 本帧才置上）与「拖拽已结束这一帧」（该帧松开 / Esc / 指针滑出窗口）。
        let drag_active_at_frame_start = self.layer_drag;

        let rows: Vec<Row> = self
            .layers
            .panel_rows()
            .into_iter()
            .map(|r| {
                let style = r.node.layer().map(|l| l.style).unwrap_or_default();
                Row {
                    id: r.node.id(),
                    name: r.node.name().to_string(),
                    depth: r.depth,
                    is_group: r.node.is_group(),
                    expanded: r.node.expanded(),
                    has_children: !r.node.children().is_empty(),
                    visible: r.node.visible(),
                    ancestors_visible: r.ancestors_visible,
                    parent: r.parent,
                    index: r.index,
                    kind: r.node.legend_kind(),
                    fill: style.fill,
                    outline: style.outline,
                    point_color: style.point_color,
                    point_radius: style.point_radius,
                }
            })
            .collect();

        // 底部留出两行提示的高度，其余全部给可滚动的树。
        let list_height = (ui.available_height() - 40.0).max(60.0);
        let mut metas: Vec<RowMeta> = Vec::with_capacity(rows.len());
        let mut root_rect = Rect::NOTHING;
        // 由 `layer_row` 在行循环里回填：被拖行本帧是否仍在被拖 / 拖拽是否在本行结束。
        let mut drag_continued = false;
        let mut drag_stopped_here = false;

        egui::ScrollArea::vertical()
            .max_height(list_height)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                // ── 根节点「图层」：对应截图里的数据框（不参与勾选 / 拖拽） ──
                let (rect, _) =
                    ui.allocate_exact_size(Vec2::new(ui.available_width(), ROW_H), Sense::hover());
                root_rect = rect;
                ui.painter().rect_filled(rect, 0.0, style::SOFT_BG);
                let cy = rect.center().y;
                let icon_rect = Rect::from_center_size(
                    Pos2::new(rect.min.x + 6.0 + GLYPH_SIZE * 0.5, cy),
                    Vec2::splat(GLYPH_SIZE),
                );
                draw_layers_glyph(ui.painter(), icon_rect, style::LAYER_OUTLINE);
                ui.painter().text(
                    Pos2::new(icon_rect.max.x + 6.0, cy),
                    Align2::LEFT_CENTER,
                    self.t("layers.root"),
                    menu_bar_font(ui.style()),
                    style::TEXT,
                );

                if rows.is_empty() {
                    ui.add_space(10.0);
                    ui.horizontal_wrapped(|ui| {
                        ui.add_space(10.0);
                        ui.label(RichText::new(self.t("layers.empty")).small());
                    });
                } else {
                    for row in rows.iter() {
                        let (still_dragging, just_stopped) = self.layer_row(ui, row, &mut metas);
                        drag_continued |= still_dragging;
                        drag_stopped_here |= just_stopped;
                    }

                    // 拖拽中：只按指针位置画落点指示，不真正移动节点。
                    if let Some(drag_id) = self.layer_drag
                        && let Some(pos) = ui.input(|i| i.pointer.latest_pos())
                        && let Some((parent, index)) = drop_target(root_rect, &metas, pos)
                    {
                        let current = metas
                            .iter()
                            .find(|m| m.id == drag_id)
                            .map(|m| (m.parent, m.index));
                        if current != Some((parent, index))
                            && drop_target_valid(&metas, drag_id, parent)
                        {
                            draw_drop_indicator(ui.painter(), &metas, root_rect, parent, index);
                        }
                    }
                }
            });

        // 拖拽结束，结算落点：图层只在鼠标松开的那一刻真正移动。
        // 松手位置在列表里 → 移到指示的位置；Esc 取消或松手落在列表外 → 保持原位。
        if let Some(drag_id) = drag_active_at_frame_start
            && !drag_continued
        {
            if drag_stopped_here
                && ui.input(|i| i.pointer.button_released(PointerButton::Primary))
                && let Some(pos) = ui.input(|i| i.pointer.latest_pos())
                && let Some((parent, index)) = drop_target(root_rect, &metas, pos)
            {
                let current = metas
                    .iter()
                    .find(|m| m.id == drag_id)
                    .map(|m| (m.parent, m.index));
                if current != Some((parent, index)) && self.layers.move_node(drag_id, parent, index)
                {
                    self.dirty = true;
                }
            }
            self.layer_drag = None;
        }
    }

    /// 移除一批节点（多选时一次删掉整个选择集）。
    ///
    /// 栅格图层可能正在跑统计 / 建金字塔的后台任务，必须**先取消任务再摘节点**，
    /// 否则后台线程会一直握着已删除图层的句柄；拖拽中的行被删掉时也要清拖拽状态，
    /// 否则 `layers_panel` 会拿着一个不存在的 id 去结算落点。
    fn remove_nodes(&mut self, ids: &[u64]) {
        if ids.is_empty() {
            return;
        }
        for id in ids {
            self.cancel_raster_job(*id);
        }
        for id in ids {
            self.layers.remove(*id);
        }
        if self.layer_drag.is_some_and(|d| ids.contains(&d)) {
            self.layer_drag = None;
        }
        self.dirty = true;
    }

    /// 缩放至节点范围（右键「缩放至图层 / 缩放至组」，对标 QGIS 的 Zoom to Layer）。
    ///
    /// 这里只排队、不立刻改视图：右键菜单位于内容列表面板内，拿不到地图控件的
    /// 精确像素矩形，下一帧由 `map_view::map_panel` 用实际矩形调用 `MapView::fit`。
    fn zoom_to_node(&mut self, id: u64) {
        // 必须在**工程 CRS** 下取范围：源 CRS 是 UTM 的影像，其米制包围盒
        // 放到经纬度工程里直接适配，跨度会变成几十万「度」。
        let project = self.project_projection.clone();
        let bounds = match &project {
            Some(project) => self.layers.node(id).map(|n| n.bounds_in(project)),
            None => self.layers.node(id).map(geo::LayerNode::bounds),
        };
        let Some(bounds) = bounds else {
            return;
        };
        if bounds.is_empty() {
            return;
        }
        self.zoom_extent = Some(bounds);
        self.dirty = true;
    }

    /// 绘制内容列表的一行，并处理它的点击 / 拖拽 / 右键菜单。
    ///
    /// 拆成独立方法有两个好处：`layers_panel` 不至于嵌套五六层；
    /// 行内各子元素的命中测试（折叠框 / 勾选框 / 其余区域）集中在一处。
    ///
    /// 返回 `(拖拽仍在继续, 拖拽刚在本行结束)`——被拖行的 Response 才能报告这两件事，
    /// 而落点结算需要整棵树的 `metas`，所以只在这里取信号、由 `layers_panel` 统一结算。
    fn layer_row(&mut self, ui: &mut Ui, row: &Row, metas: &mut Vec<RowMeta>) -> (bool, bool) {
        let selected = self.layers.is_selected(row.id);
        let dragged = self.layer_drag == Some(row.id);
        let (row_rect, resp) = ui.allocate_exact_size(
            Vec2::new(ui.available_width(), ROW_H),
            Sense::drag() | Sense::click(),
        );
        let cy = row_rect.center().y;

        // 行背景优先级：拖拽中（"拾起"感）> 选中 > 悬停。
        let painter = ui.painter();
        if dragged {
            painter.rect_filled(row_rect, 0.0, style::SELECTION);
            painter.rect_stroke(
                row_rect.shrink(1.0),
                3.0,
                Stroke::new(1.5_f32, style::ACCENT),
                StrokeKind::Inside,
            );
        } else if selected {
            painter.rect_filled(row_rect, 0.0, style::SELECTION);
        } else if resp.hovered() {
            painter.rect_filled(row_rect, 0.0, style::ACCENT_HOVER_BG);
        }

        // 四段式列布局：折叠框 → 勾选框 → 图例/组图标 → 名称。
        let base_x = row_rect.min.x + 4.0 + row.depth as f32 * INDENT;
        let expander_rect = Rect::from_center_size(
            Pos2::new(base_x + EXPANDER_SIZE * 0.5, cy),
            Vec2::splat(EXPANDER_SIZE),
        );
        let checkbox_rect = Rect::from_center_size(
            Pos2::new(base_x + EXPANDER_SIZE + 3.0 + CHECKBOX_SIZE * 0.5, cy),
            Vec2::splat(CHECKBOX_SIZE),
        );
        let glyph_x = checkbox_rect.max.x + 5.0;
        let glyph_rect = Rect::from_center_size(
            Pos2::new(glyph_x + GLYPH_SIZE * 0.5, cy),
            Vec2::splat(GLYPH_SIZE),
        );
        let text_x = glyph_x + GLYPH_SIZE + 6.0;

        let pointer = ui.input(|i| i.pointer.interact_pos());
        let hover_expander = row.has_children && pointer.is_some_and(|p| expander_rect.contains(p));

        if row.has_children {
            draw_expander(ui.painter(), expander_rect, row.expanded, hover_expander);
        }
        draw_checkbox(
            ui.painter(),
            checkbox_rect,
            row.visible,
            !row.ancestors_visible,
        );
        draw_legend_symbol(ui.painter(), glyph_rect, row);

        // 名称：全应用文字统一纯黑；是否绘制由勾选框状态表达。
        let text_color = style::TEXT;
        // 与顶部菜单栏同字体（组与图层不再区分字号）。
        let font = menu_bar_font(ui.style());
        ui.painter().text(
            Pos2::new(text_x, cy),
            Align2::LEFT_CENTER,
            &row.name,
            font,
            text_color,
        );

        metas.push(RowMeta {
            id: row.id,
            rect: row_rect,
            parent: row.parent,
            index: row.index,
            is_group: row.is_group,
        });

        // ── 左键：折叠框切展开、勾选框切绘制、其余区域选中 ──
        //
        // 行主体（折叠框 / 勾选框以外）支持多选，与文件管理器的习惯一致：
        //   · 普通点击   → 只选这一项；
        //   · Ctrl + 点击 → 逐个加选 / 取消（可挑不连续的多项）；
        //   · Shift+ 点击 → 选中上次锚点到这一项之间的连续一段。
        if resp.clicked() {
            let hit = |r: Rect| pointer.is_some_and(|p| r.contains(p));
            if row.has_children && hit(expander_rect) {
                self.layers.toggle_expanded(row.id);
            } else if hit(checkbox_rect) {
                self.layers.set_visible(row.id, !row.visible);
                self.dirty = true;
            } else {
                // 修饰键在**按下那一刻**的状态：点击是按下即判定的，
                // 松手时用户可能已经提前松开了 Ctrl / Shift。
                let modifiers = ui.input(|i| i.modifiers);
                if modifiers.shift {
                    self.layers.select_range(row.id);
                } else if modifiers.ctrl || modifiers.mac_cmd {
                    self.layers.toggle_select(row.id);
                } else {
                    self.layers.select(row.id);
                }
            }
        }
        if resp.dragged() {
            // 拖已选中的行时不重排选择（多选拖动的起点），拖未选中的行才收敛到它。
            if !self.layers.is_selected(row.id) {
                self.layers.select(row.id);
            }
            self.layer_drag = Some(row.id);
        }
        // 双击栅格图层：打开「图层属性」对话框（对标 ArcGIS 双击图层）。
        if resp.double_clicked() && !row.is_group {
            self.layers.select(row.id);
            self.open_raster_props(row.id);
        }
        // 注意：`layer_drag` 不在这里清——清与结算都放在 `layers_panel` 的树循环之后，
        // 那里才拿得到完整的 `metas` 来算松手落点。
        let drag_continued = dragged && resp.dragged();
        let drag_stopped_here = dragged && resp.drag_stopped();

        // ── 右键菜单：缩放至图层 / 组、显示 / 隐藏、新建子组、移出组、删除 ──
        let visible = row.visible;
        let is_group = row.is_group;
        let inside_group = row.parent.is_some();
        let id = row.id;
        // 与 QGIS 一致（`setEnabled(layer->isValid())`）：空组、无几何或无地理参考的
        // 图层没有有效范围，菜单项禁用而不是缩放到默认的全图范围。
        let node_bounds = {
            // 与缩放动作保持同一口径：工程坐标系已设置时按显示范围判断有效性。
            let project = self.project_projection.clone();
            match &project {
                Some(project) => self.layers.node(id).map(|n| n.bounds_in(project)),
                None => self.layers.node(id).map(geo::LayerNode::bounds),
            }
            .filter(|b| !b.is_empty())
        };
        // 只有矢量图层有属性行（QGIS 对栅格另开「Open Raster Attribute Table」，此处不提供）。
        let is_vector = self
            .layers
            .node(id)
            .and_then(geo::LayerNode::layer)
            .is_some();
        // 右键落在未选中的行上：先把选择收敛到这一行（与文件管理器一致），
        // 菜单里的「移除」因此总是作用在用户看得见的那一批行上。
        if resp.secondary_clicked() {
            let hit = |r: Rect| pointer.is_some_and(|p| r.contains(p));
            if !((row.has_children && hit(expander_rect)) || hit(checkbox_rect))
                && !self.layers.is_selected(row.id)
            {
                self.layers.select(row.id);
            }
        }
        resp.context_menu(|ui| {
            let zoom_key = if is_group {
                "layers.zoomToGroup"
            } else {
                "layers.zoomToLayer"
            };
            if ui
                .add_enabled(
                    node_bounds.is_some(),
                    egui::Button::new(RichText::new(self.t(zoom_key))),
                )
                .clicked()
            {
                self.zoom_to_node(id);
                ui.close_menu();
            }
            if is_vector
                && ui
                    .button(RichText::new(self.t("layers.openAttrTable")))
                    .clicked()
            {
                self.open_attr_table(id);
                ui.close_menu();
            }
            ui.separator();
            if !is_group
                && ui
                    .button(RichText::new(self.t("layers.properties")))
                    .clicked()
            {
                self.open_raster_props(id);
                ui.close_menu();
            }
            if ui
                .button(RichText::new(self.t(if visible {
                    "layers.hide"
                } else {
                    "layers.show"
                })))
                .clicked()
            {
                self.layers.set_visible(id, !visible);
                self.dirty = true;
                ui.close_menu();
            }
            if is_group
                && ui
                    .button(RichText::new(self.t("layers.newGroupHere")))
                    .clicked()
            {
                let name = self.layers.unique_name(self.t("layers.newGroupName"));
                self.layers.select(id); // 让新组落进当前组
                self.layers.add_group(name);
                self.dirty = true;
                ui.close_menu();
            }
            if inside_group
                && ui
                    .button(RichText::new(self.t("layers.moveToTop")))
                    .clicked()
            {
                self.layers.move_node(id, None, 0);
                self.dirty = true;
                ui.close_menu();
            }
            ui.separator();
            // 「移除」的作用范围：右键落在已选中的行上 → 移除整个选择集（多选批量删）；
            // 否则只移除这一行。菜单文案随之显示「移除 N 项」。
            let targets: Vec<u64> = if self.layers.is_selected(id) {
                self.layers.selection_ids().to_vec()
            } else {
                vec![id]
            };
            let remove_text = if targets.len() > 1 {
                self.tf("layers.removeMany", &[&targets.len().to_string()])
            } else {
                self.t("layers.remove").to_string()
            };
            // 快捷键 R：菜单打开期间按 R 等同于点这一项（文案右侧显示 "R" 提示）。
            let remove_pressed = ui
                .add(
                    egui::Button::new(RichText::new(remove_text).color(style::DANGER))
                        .shortcut_text("R"),
                )
                .clicked()
                || ui.input(|i| i.key_pressed(egui::Key::R));
            if remove_pressed {
                self.remove_nodes(&targets);
                ui.close_menu();
            }
        });

        (drag_continued, drag_stopped_here)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一个只含小方块的多边形图层（内容列表测试不关心几何细节）。
    fn sample_layer(name: &str) -> geo::Layer {
        geo::Layer::from_geometry(
            name.to_string(),
            Vec::new(),
            Vec::new(),
            vec![geo::PolygonData {
                outer: vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0], [0.0, 0.0]],
                holes: Vec::new(),
            }],
        )
    }

    fn headless_input() -> egui::RawInput {
        egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1280.0, 800.0))),
            ..Default::default()
        }
    }

    /// 无头跑几帧完整的内容列表：工具条、根节点「图层」、缩进的图层树、
    /// 勾选框与矢量图标都真实走一遍布局、绘制与命中测试。
    ///
    /// 面板的行布局全是按像素手算的（缩进、列宽、命中区域），这段代码最容易
    /// 在改动后出界或 panic；用 `Context::default()` 就可以在没有窗口的情况下跑起来。
    #[test]
    fn layers_panel_lays_out_headless() {
        let ctx = egui::Context::default();
        let mut app = RustGisApp::new(&ctx);

        let group = app.layers.add_group("组".to_string());
        app.layers.add(sample_layer("a"));
        app.layers.select(u64::MAX); // 选中项已失效 ⇒ 新图层退回顶层
        let top = app.layers.add(sample_layer("top"));
        app.layers.move_node(top, Some(group), 0);

        for expanded in [true, false, true] {
            app.layers.set_all_expanded(expanded);
            let _ = ctx.run(headless_input(), |ctx| {
                egui::SidePanel::left("layers_panel").show(ctx, |ui| app.layers_panel(ui));
            });
        }

        let names: Vec<&str> = app
            .layers
            .panel_rows()
            .iter()
            .map(|r| r.node.name())
            .collect();
        assert_eq!(names, vec!["组", "top", "a"], "渲染不应改动图层树");
    }

    /// 拖拽落点：根节点行 → 顶层最前；组行 → 放进该组；图层行 → 停在它的位置。
    #[test]
    fn drop_target_resolves_by_row_kind() {
        let row = |y: f32| Rect::from_min_size(Pos2::new(0.0, y), Vec2::new(200.0, ROW_H));
        let root = row(0.0);
        let metas = vec![
            RowMeta {
                id: 7,
                rect: row(ROW_H),
                parent: None,
                index: 0,
                is_group: true,
            },
            RowMeta {
                id: 8,
                rect: row(ROW_H * 2.0),
                parent: Some(7),
                index: 3,
                is_group: false,
            },
        ];
        assert_eq!(drop_target(root, &metas, root.center()), Some((None, 0)));
        assert_eq!(
            drop_target(root, &metas, row(ROW_H).center()),
            Some((Some(7), 0))
        );
        assert_eq!(
            drop_target(root, &metas, row(ROW_H * 2.0).center()),
            Some((Some(7), 3))
        );
        assert_eq!(drop_target(root, &metas, Pos2::new(0.0, 500.0)), None);
    }

    /// 跑一帧内容列表（带指针事件）。命中测试基于上一遍注册的 widgets，
    /// 所以真正的交互前要先跑至少一帧布局。
    fn frame(ctx: &egui::Context, app: &mut RustGisApp, events: Vec<egui::Event>) {
        frame_with(ctx, app, egui::Modifiers::default(), events);
    }

    /// 跑一帧内容列表，并指定这一帧按住的修饰键（Ctrl / Shift 多选靠它区分）。
    fn frame_with(
        ctx: &egui::Context,
        app: &mut RustGisApp,
        modifiers: egui::Modifiers,
        events: Vec<egui::Event>,
    ) {
        let mut input = headless_input();
        // `InputState::modifiers` 每帧由 `RawInput` 覆盖，按住状态必须逐帧重发。
        input.modifiers = modifiers;
        input.events = events;
        let _ = ctx.run(input, |ctx| {
            egui::SidePanel::left("layers_panel").show(ctx, |ui| app.layers_panel(ui));
        });
    }

    /// 在 `pos` 处按一次左键（移动 + 按下 + 松开），返回这一帧的事件序列。
    fn click_events(pos: Pos2) -> Vec<egui::Event> {
        vec![
            egui::Event::PointerMoved(pos),
            press_event(pos),
            release_event(pos),
        ]
    }

    /// 探测前 `count` 行的屏幕位置：从上往下逐点点击，按点中后 `selected_id()`
    /// 落在第几行记下坐标。
    ///
    /// 面板布局是像素级手算的，探测比硬编码 y 值稳健；返回的向量按下标对应
    /// `panel_rows()` 的顺序（即自上而下）。
    fn probe_row_positions(ctx: &egui::Context, app: &mut RustGisApp, count: usize) -> Vec<Pos2> {
        let ids: Vec<u64> = app.layers.panel_rows().iter().map(|r| r.node.id()).collect();
        app.layers.select(u64::MAX); // 先让选中失效，避免「没点中」被旧值误报
        let mut found: Vec<Option<Pos2>> = vec![None; count];
        for y in (30..=200).step_by(2) {
            let pos = Pos2::new(60.0, y as f32);
            frame(ctx, app, click_events(pos));
            if let Some(hit) = app.layers.selected_id()
                && let Some(i) = ids.iter().position(|id| *id == hit)
                && i < count
                && found[i].is_none()
            {
                found[i] = Some(pos);
            }
            if found.iter().all(Option::is_some) {
                break;
            }
        }
        found
            .into_iter()
            .map(|f| f.expect("内容列表的行应能被点选到"))
            .collect()
    }

    fn press_event(pos: Pos2) -> egui::Event {
        egui::Event::PointerButton {
            pos,
            button: PointerButton::Primary,
            pressed: true,
            modifiers: egui::Modifiers::default(),
        }
    }

    fn release_event(pos: Pos2) -> egui::Event {
        egui::Event::PointerButton {
            pos,
            button: PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::default(),
        }
    }

    /// 内容列表自上而下的名称顺序。
    fn toc_names(app: &RustGisApp) -> Vec<&str> {
        app.layers
            .panel_rows()
            .iter()
            .map(|r| r.node.name())
            .collect()
    }

    /// 拖拽重排的时序：拖动过程中只出现落点指示器、图层树纹丝不动，
    /// 只有鼠标松开的那一刻才真正移动图层。
    ///
    /// 行的屏幕位置靠「逐行点选探测」得到——面板布局是像素级手算的，
    /// 探测比硬编码 y 值稳健。
    #[test]
    fn layer_drag_moves_only_on_release() {
        let ctx = egui::Context::default();
        let mut app = RustGisApp::new(&ctx);

        let a = app.layers.add(sample_layer("a"));
        let b = app.layers.add(sample_layer("b"));
        app.layers.move_node(a, None, 0); // 固定初始顺序 [a, b]

        // 先跑一帧布局，让行注册进上一遍的 widgets。
        frame(&ctx, &mut app, Vec::new());

        // 探测两行的真实屏幕位置（x 取 60：避开折叠框 / 勾选框 / 工具条按钮）。
        // 先把选中项设为失效 id，否则「没点中任何东西」也会带着旧选中值误报。
        app.layers.select(u64::MAX);
        let mut y_a: Option<Pos2> = None;
        let mut y_b: Option<Pos2> = None;
        for y in (30..=170).step_by(2) {
            let pos = Pos2::new(60.0, y as f32);
            frame(
                &ctx,
                &mut app,
                vec![
                    egui::Event::PointerMoved(pos),
                    press_event(pos),
                    release_event(pos),
                ],
            );
            match app.layers.selected_id() {
                Some(id) if id == a && y_a.is_none() => y_a = Some(pos),
                Some(id) if id == b && y_b.is_none() => y_b = Some(pos),
                _ => {}
            }
            if y_a.is_some() && y_b.is_some() {
                break;
            }
        }
        let y_a = y_a.expect("「a」行应能被点选到");
        let y_b = y_b.expect("「b」行应能被点选到");

        // 1) 在 b 行按下：还没移动，不算拖拽。
        frame(
            &ctx,
            &mut app,
            vec![egui::Event::PointerMoved(y_b), press_event(y_b)],
        );
        assert_eq!(app.layer_drag, None, "刚按下、未移动时不应进入拖拽");

        // 2) 移到 a 行（位移超过点击阈值）⇒ 进入拖拽；但树必须纹丝不动。
        frame(&ctx, &mut app, vec![egui::Event::PointerMoved(y_a)]);
        assert_eq!(app.layer_drag, Some(b), "移动超过阈值后应进入拖拽");
        assert_eq!(toc_names(&app), vec!["a", "b"], "拖动过程中树不得移动");

        // 3) 按住指针停在 a 行上多跑一帧：仍然只是指示器，不是移动。
        frame(&ctx, &mut app, Vec::new());
        assert_eq!(app.layer_drag, Some(b));
        assert_eq!(toc_names(&app), vec!["a", "b"], "鼠标未松开前图层不得移动");

        // 4) 在 a 行位置松开 ⇒ 图层此刻才真正移动（b 插到 a 之前）。
        frame(&ctx, &mut app, vec![release_event(y_a)]);
        assert_eq!(app.layer_drag, None, "松开后拖拽状态应被清理");
        assert_eq!(toc_names(&app), vec!["b", "a"], "松开后 b 应移到 a 之前");
    }

    /// 三行的屏幕位置（自上而下）与对应 id。
    fn three_rows(ctx: &egui::Context, app: &mut RustGisApp) -> (Vec<Pos2>, Vec<u64>) {
        app.layers.add(sample_layer("a"));
        app.layers.add(sample_layer("b"));
        app.layers.add(sample_layer("c"));
        frame(ctx, app, Vec::new()); // 先跑一帧布局，让行注册进 widgets
        let ids: Vec<u64> = app.layers.panel_rows().iter().map(|r| r.node.id()).collect();
        let pos = probe_row_positions(ctx, app, ids.len());
        (pos, ids)
    }

    /// Ctrl + 左键：逐个加选 / 取消，可挑不连续的多项。
    #[test]
    fn ctrl_click_toggles_rows_in_panel() {
        let ctx = egui::Context::default();
        let mut app = RustGisApp::new(&ctx);
        let (pos, ids) = three_rows(&ctx, &mut app);
        let (first, last) = (ids[0], ids[2]);

        app.layers.select(first);
        frame_with(&ctx, &mut app, egui::Modifiers::CTRL, click_events(pos[2]));
        assert_eq!(app.layers.selection_ids(), &[first, last][..]);

        // 再点已选中的那一行 ⇒ 取消
        frame_with(&ctx, &mut app, egui::Modifiers::CTRL, click_events(pos[2]));
        assert_eq!(app.layers.selection_ids(), &[first][..]);

        // 点中间那行 ⇒ 变成「首 + 中」两个不相邻的项
        frame_with(&ctx, &mut app, egui::Modifiers::CTRL, click_events(pos[1]));
        assert_eq!(app.layers.selection_ids(), &[first, ids[1]][..]);
    }

    /// Shift + 左键：选中锚点到点击行之间的连续一段。
    #[test]
    fn shift_click_selects_range_in_panel() {
        let ctx = egui::Context::default();
        let mut app = RustGisApp::new(&ctx);
        let (pos, ids) = three_rows(&ctx, &mut app);

        app.layers.select(ids[0]); // 锚点 = 第一行
        frame_with(&ctx, &mut app, egui::Modifiers::SHIFT, click_events(pos[2]));
        assert_eq!(app.layers.selection_ids(), &ids[..], "首行到末行整段选中");

        // 锚点不随这次点击移动：再 Shift 点中间那行 ⇒ 收敛回首 + 中
        frame_with(&ctx, &mut app, egui::Modifiers::SHIFT, click_events(pos[1]));
        assert_eq!(app.layers.selection_ids(), &ids[0..2]);
    }

    /// 右键菜单打开期间按 R ⇒ 移除选中项（「移除」的快捷键）。
    #[test]
    fn r_shortcut_removes_selected_in_context_menu() {
        let ctx = egui::Context::default();
        let mut app = RustGisApp::new(&ctx);
        let (pos, ids) = three_rows(&ctx, &mut app);
        let target = ids[0];

        // 右键点第一行：菜单打开，且选择收敛到这一行
        let secondary = |pressed: bool| egui::Event::PointerButton {
            pos: pos[0],
            button: PointerButton::Secondary,
            pressed,
            modifiers: egui::Modifiers::default(),
        };
        frame(
            &ctx,
            &mut app,
            vec![
                egui::Event::PointerMoved(pos[0]),
                secondary(true),
                secondary(false),
            ],
        );
        assert_eq!(
            app.layers.selected_id(),
            Some(target),
            "右键未选中的行应把选择收敛到它"
        );

        // 菜单仍在打开的下一帧：按 R
        frame(
            &ctx,
            &mut app,
            vec![egui::Event::Key {
                key: egui::Key::R,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::default(),
            }],
        );
        assert!(
            app.layers.node(target).is_none(),
            "菜单里按 R 应移除选中图层"
        );
        assert_eq!(app.layers.panel_rows().len(), 2);
        assert!(app.dirty, "移除应把工程标脏");
    }

    /// 多选后右键「移除」一次删掉整个选择集。
    #[test]
    fn remove_applies_to_the_whole_selection() {
        let ctx = egui::Context::default();
        let mut app = RustGisApp::new(&ctx);
        let (_, ids) = three_rows(&ctx, &mut app);

        app.layers.select(ids[0]);
        app.layers.toggle_select(ids[2]);
        assert_eq!(app.layers.selection_ids(), &[ids[0], ids[2]][..]);

        app.remove_nodes(&[ids[0], ids[2]]);
        let left: Vec<u64> = app.layers.panel_rows().iter().map(|r| r.node.id()).collect();
        assert_eq!(left, vec![ids[1]], "只留下没被选中的那一行");
        assert!(
            app.layers.selection_ids().is_empty(),
            "被选中的两行都删掉了，选择集应清空"
        );
    }

    /// 无头跑一帧中央地图面板（视图适配逻辑所在的位置）。
    fn map_frame(ctx: &egui::Context, app: &mut RustGisApp) {
        let _ = ctx.run(headless_input(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| app.map_panel(ui));
        });
    }

    /// 在 `(x, y)` 处造一个 1×1 的多边形图层。
    fn square_layer(name: &str, x: f64, y: f64) -> geo::Layer {
        geo::Layer::from_geometry(
            name.to_string(),
            Vec::new(),
            Vec::new(),
            vec![geo::PolygonData {
                outer: vec![
                    [x, y],
                    [x + 1.0, y],
                    [x + 1.0, y + 1.0],
                    [x, y + 1.0],
                    [x, y],
                ],
                holes: Vec::new(),
            }],
        )
    }

    /// 右键「缩放至图层」：排队的是该节点（组为子树并集）的范围，
    /// 空组没有范围时不排队、也不把工程标脏。
    #[test]
    fn zoom_to_node_queues_node_bounds() {
        let ctx = egui::Context::default();
        let mut app = RustGisApp::new(&ctx);

        let group = app.layers.add_group("组".to_string());
        let a = app.layers.add(sample_layer("a"));
        app.layers.move_node(a, Some(group), 0);

        app.zoom_to_node(group);
        let queued = app.zoom_extent.expect("组应有待缩放的子树并集范围");
        assert_eq!(queued, app.layers.node(a).unwrap().bounds());
        assert!(app.dirty, "视图操作应把工程标脏");

        // 空组：没有有效范围，菜单项在界面上是禁用的，这里再兜一层。
        app.zoom_extent = None;
        app.dirty = false;
        let empty = app.layers.add_group("空组".to_string());
        app.zoom_to_node(empty);
        assert_eq!(app.zoom_extent, None);
        assert!(!app.dirty);
    }

    /// 排队范围在下一帧被地图面板消费：视图中心落在该图层范围中心，
    /// 而不是全部图层的总范围中心。
    #[test]
    fn zoom_extent_is_applied_next_map_frame() {
        let ctx = egui::Context::default();
        let mut app = RustGisApp::new(&ctx);
        let near = app.layers.add(sample_layer("near")); // 范围 [0,0]-[1,1]
        app.layers.add(square_layer("far", 100.0, 100.0));

        map_frame(&ctx, &mut app); // 首帧自动适配全部数据
        let total = app.map.view.expect("首帧后应有视图");
        assert!(total.center[0] > 50.0, "首帧应对齐全图总范围");

        app.zoom_to_node(near);
        assert!(app.zoom_extent.is_some());
        map_frame(&ctx, &mut app);

        assert_eq!(app.zoom_extent, None, "适配后应清空排队范围");
        let view = app.map.view.expect("应有视图");
        assert!((view.center[0] - 0.5).abs() < 1e-9, "{:?}", view.center);
        assert!((view.center[1] - 0.5).abs() < 1e-9, "{:?}", view.center);
        assert!(view.scale > 0.0);
    }
}
