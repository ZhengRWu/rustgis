//! 左侧「内容列表」（TOC）面板：图层树 + 拖拽重排 + 右键菜单。
//!
//! 对标 ArcGIS 的 Table Of Contents。布局按截图组织：顶部是一排图标工具条，
//! 下面是「图层」根节点，再往下是逐级缩进的图层树；每一行都由
//! 「+/- 折叠框 → 勾选框 → 图例符号 → 名称」四段组成。
//!
//! 行布局是**像素级手算**的（缩进、列宽、命中区域），[`mod tests`] 里用无头
//! `Context` 跑了完整的布局与拖拽时序回归，改动本文件后必须先过那些测试。
//! 基础图标（加号 / 文件夹 / 折叠框 / 勾选框等）在 [`super::widgets`]。

use egui::{Align2, Color32, FontId, PointerButton, Pos2, Rect, RichText, Sense, Stroke, StrokeKind, Ui, Vec2};

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
            [
                Pos2::new(r.min.x + 10.0, y),
                Pos2::new(r.max.x - 10.0, y),
            ],
            Stroke::new(2.0_f32, style::ACCENT),
        );
    };
    // 「插在该位置」对应的锚行：兄弟位置就是那一行本身，「组最上层」是组的首个子行。
    let anchor = metas.iter().find(|m| m.parent == parent && m.index == index);
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
                let (rect, _) = ui.allocate_exact_size(
                    Vec2::new(ui.available_width(), ROW_H),
                    Sense::hover(),
                );
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
                        ui.label(RichText::new(self.t("layers.empty")).weak().small());
                    });
                } else {
                    for row in rows.iter() {
                        let (still_dragging, just_stopped) =
                            self.layer_row(ui, row, &mut metas);
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
                if current != Some((parent, index))
                    && self.layers.move_node(drag_id, parent, index)
                {
                    self.dirty = true;
                }
            }
            self.layer_drag = None;
        }

        ui.separator();
        ui.label(RichText::new(self.t("layers.dragHint")).weak().small());
        ui.label(RichText::new(self.t("map.hint")).weak().small());
    }

    /// 绘制内容列表的一行，并处理它的点击 / 拖拽 / 右键菜单。
    ///
    /// 拆成独立方法有两个好处：`layers_panel` 不至于嵌套五六层；
    /// 行内各子元素的命中测试（折叠框 / 勾选框 / 其余区域）集中在一处。
    ///
    /// 返回 `(拖拽仍在继续, 拖拽刚在本行结束)`——被拖行的 Response 才能报告这两件事，
    /// 而落点结算需要整棵树的 `metas`，所以只在这里取信号、由 `layers_panel` 统一结算。
    fn layer_row(
        &mut self,
        ui: &mut Ui,
        row: &Row,
        metas: &mut Vec<RowMeta>,
    ) -> (bool, bool) {
        let selected = self.layers.selected_id() == Some(row.id);
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

        // 名称：自身或祖先未勾选的节点用弱文字色提示"当前不绘制"。
        let text_color = if row.visible && row.ancestors_visible {
            style::TEXT
        } else {
            style::TEXT_WEAK
        };
        // 与顶部菜单栏同字体（组与图层不再区分字号）。
        let font = menu_bar_font(ui.style());
        ui.painter()
            .text(Pos2::new(text_x, cy), Align2::LEFT_CENTER, &row.name, font, text_color);

        metas.push(RowMeta {
            id: row.id,
            rect: row_rect,
            parent: row.parent,
            index: row.index,
            is_group: row.is_group,
        });

        // ── 左键：折叠框切展开、勾选框切绘制、其余区域选中 ──
        if resp.clicked() {
            let hit = |r: Rect| pointer.is_some_and(|p| r.contains(p));
            if row.has_children && hit(expander_rect) {
                self.layers.toggle_expanded(row.id);
            } else if hit(checkbox_rect) {
                self.layers.set_visible(row.id, !row.visible);
                self.dirty = true;
            } else {
                self.layers.select(row.id);
            }
        }
        if resp.dragged() {
            self.layers.select(row.id);
            self.layer_drag = Some(row.id);
        }
        // 注意：`layer_drag` 不在这里清——清与结算都放在 `layers_panel` 的树循环之后，
        // 那里才拿得到完整的 `metas` 来算松手落点。
        let drag_continued = dragged && resp.dragged();
        let drag_stopped_here = dragged && resp.drag_stopped();

        // ── 右键菜单：显示 / 隐藏、新建子组、移出组、删除 ──
        let visible = row.visible;
        let is_group = row.is_group;
        let inside_group = row.parent.is_some();
        let id = row.id;
        resp.context_menu(|ui| {
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
            if is_group && ui.button(RichText::new(self.t("layers.newGroupHere"))).clicked() {
                let name = self.layers.unique_name(self.t("layers.newGroupName"));
                self.layers.select(id); // 让新组落进当前组
                self.layers.add_group(name);
                self.dirty = true;
                ui.close_menu();
            }
            if inside_group && ui.button(RichText::new(self.t("layers.moveToTop"))).clicked() {
                self.layers.move_node(id, None, 0);
                self.dirty = true;
                ui.close_menu();
            }
            ui.separator();
            if ui
                .button(RichText::new(self.t("layers.delete")).color(style::DANGER))
                .clicked()
            {
                self.layers.remove(id);
                self.layer_drag = None;
                self.dirty = true;
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
        assert_eq!(drop_target(root, &metas, row(ROW_H).center()), Some((Some(7), 0)));
        assert_eq!(
            drop_target(root, &metas, row(ROW_H * 2.0).center()),
            Some((Some(7), 3))
        );
        assert_eq!(drop_target(root, &metas, Pos2::new(0.0, 500.0)), None);
    }

    /// 跑一帧内容列表（带指针事件）。命中测试基于上一遍注册的 widgets，
    /// 所以真正的交互前要先跑至少一帧布局。
    fn frame(ctx: &egui::Context, app: &mut RustGisApp, events: Vec<egui::Event>) {
        let mut input = headless_input();
        input.events = events;
        let _ = ctx.run(input, |ctx| {
            egui::SidePanel::left("layers_panel").show(ctx, |ui| app.layers_panel(ui));
        });
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
        frame(&ctx, &mut app, vec![egui::Event::PointerMoved(y_b), press_event(y_b)]);
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
}
