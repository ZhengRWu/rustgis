//! 「查找」：按名称 / 属性值在矢量图层里检索要素，双击结果定位到要素。
//!
//! 对标 ArcGIS 的 Find 对话框：输入关键词 → 列出命中的要素（图层 / FID / 字段 / 值）
//! → 双击某行把地图缩放到该要素，并在画面上闪一圈橙色高亮框。检索是只读操作，
//! 不修改图层数据、不把工程标记为「已修改」。
//!
//! 检索范围：所有矢量图层的 FID 与全部字段值（栅格图层没有属性表，不参与）。
//! 一个要素命中多个字段时只列出第一次命中的那一列，避免结果列表被同一要素刷屏。

use egui::{Context, Id, Key, TextEdit};

use super::{RustGisApp, Status};
use crate::geo::proj::display_aabb;
use crate::geo::{Aabb, Layer, LayerManager, LayerNode};
use crate::style;

/// 一次查找最多收集的结果数：超出只提示"结果过多"，不再继续扫描渲染
/// （几万行的属性表逐行建控件会把界面拖死）。
const MAX_RESULTS: usize = 500;
/// 定位高亮的显示时长（秒）。
const HIGHLIGHT_SECONDS: f64 = 4.0;
/// 定位高亮颜色（橙色，与「转到 XY」的红色十字环区分开）。
pub(super) const HIGHLIGHT_COLOR: egui::Color32 = egui::Color32::from_rgb(230, 126, 34);
/// 结果列表里「值」列的展示上限（字符数），超出用省略号。
const VALUE_CHARS: usize = 64;

/// 命中项来自哪一列。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum FindField {
    /// 要素 ID（FID 文本被命中）。
    Fid,
    /// 属性字段值（携带字段名）。
    Attr(String),
}

/// 一条查找结果。
#[derive(Debug, Clone)]
pub(super) struct FindHit {
    /// 命中要素所属图层 id。
    pub(super) layer: u64,
    /// 命中要素所属图层名（图层可能被重命名 / 删除，列表里保留命中时的名字）。
    pub(super) layer_name: String,
    /// 要素行号（0 基；界面显示 FID = `row + 1`）。
    pub(super) row: usize,
    /// 命中的列。
    pub(super) field: FindField,
    /// 命中的值（列表里展示的文本）。
    pub(super) value: String,
}

/// 「查找」窗口状态（跨帧保存输入内容与结果列表）。
#[derive(Default)]
pub(super) struct Find {
    pub open: bool,
    query: String,
    /// 匹配时区分大小写。
    case_sensitive: bool,
    /// 检索隐藏图层（默认只查当前可见的）。
    include_hidden: bool,
    /// 只检索内容列表里当前选中的图层（或组）。
    only_selected: bool,
    results: Vec<FindHit>,
    /// 结果列表里被点中的那一行。
    selected: Option<usize>,
    /// 结果数量被 [`MAX_RESULTS`] 截断。
    truncated: bool,
    /// 至少执行过一次查找（区分"还没查"和"查了但没有结果"）。
    searched: bool,
    error: Option<String>,
    /// 刚打开窗口：下一帧把焦点放到输入框。
    focus: bool,
    /// 最近一次定位的要素范围（工程 CRS），供地图闪一圈高亮。
    pub(super) highlight: Option<Aabb>,
    /// 高亮过期时刻（`egui` 时间轴，秒）。
    pub(super) highlight_until: f64,
    /// 上一帧每个结果行的屏幕矩形（仅测试用：用来把鼠标点到具体某一行上）。
    #[cfg(test)]
    pub(super) row_rects: Vec<egui::Rect>,
}
impl RustGisApp {
    /// 打开（或聚焦）「查找」窗口。开着时只把焦点移回输入框，不丢已有结果。
    pub(super) fn open_find(&mut self) {
        self.find.open = true;
        self.find.focus = true;
    }

    /// 「查找」浮窗（非模态：开着时仍可缩放 / 平移地图）。
    pub(super) fn show_find(&mut self, ctx: &Context) {
        if !self.find.open {
            return;
        }
        let mut state = std::mem::take(&mut self.find);
        let lang = self.language;
        let mut open = true;
        let mut close = false;
        let mut search = false;
        let mut locate = None;
        let pos = egui::pos2(
            (ctx.screen_rect().left() + 340.0).max(16.0),
            ctx.screen_rect().top() + 96.0,
        );
        egui::Window::new(lang.t("find.title"))
            .id(Id::new("find_window"))
            .open(&mut open)
            .default_pos(pos)
            .default_width(460.0)
            .min_width(360.0)
            .show(ctx, |ui| {
                ui.add_space(2.0);
                ui.horizontal(|ui| {
                    ui.label(lang.t("find.query"));
                    let input = ui.add(
                        TextEdit::singleline(&mut state.query)
                            .id(ui.make_persistent_id("find_query"))
                            .hint_text(lang.t("find.placeholder"))
                            .desired_width(240.0),
                    );
                    if state.focus {
                        state.focus = false;
                        input.request_focus();
                    }
                    if ui.button(lang.t("find.search")).clicked() {
                        search = true;
                    }
                });
                ui.add_space(2.0);
                ui.horizontal(|ui| {
                    ui.checkbox(&mut state.case_sensitive, lang.t("find.caseSensitive"));
                    ui.checkbox(&mut state.only_selected, lang.t("find.onlySelected"));
                    ui.checkbox(&mut state.include_hidden, lang.t("find.includeHidden"));
                });
                // 回车 = 查找（光标通常就在输入框里）。
                if ui.input(|input| input.key_pressed(Key::Enter)) {
                    search = true;
                }
                if let Some(error) = &state.error {
                    ui.colored_label(style::DANGER, error);
                }
                ui.separator();
                find_results_ui(ui, &mut state, &lang, &mut locate);
                ui.separator();
                ui.horizontal(|ui| {
                    ui.weak(lang.t("find.hint"));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button(lang.t("find.close")).clicked() {
                            close = true;
                        }
                    });
                });
            });
        if close {
            open = false;
        }
        state.open = open;
        // 先放回窗口状态：查找执行体要同时用到输入框与图层数据。
        self.find = state;
        if search {
            self.apply_find_search(lang);
        }
        let now = ctx.input(|input| input.time);
        if let Some(index) = locate {
            self.locate_find_hit(index, now);
            // 缩放在下一帧的地图面板里生效：确保这一帧之后还会再画一帧。
            ctx.request_repaint();
        }
    }

    /// 执行一次查找（含空输入的校验），结果写回 `self.find`。
    fn apply_find_search(&mut self, lang: crate::i18n::Language) {
        if self.find.query.trim().is_empty() {
            self.find.results.clear();
            self.find.searched = false;
            self.find.truncated = false;
            self.find.selected = None;
            self.find.error = Some(lang.t("find.error.empty").to_string());
            return;
        }
        self.find.error = None;
        self.find.run_search(&self.layers);
    }

    /// 双击结果：把地图缩放到该要素，并闪一圈高亮（同时选中它所属的图层）。
    fn locate_find_hit(&mut self, index: usize, now: f64) {
        let Some(hit) = self.find.results.get(index).cloned() else {
            return;
        };
        let Some(node) = self.layers.node(hit.layer) else {
            return;
        };
        let (Some(layer), name) = (node.layer(), node.name().to_string()) else {
            return;
        };
        let Some(bounds) = layer.feature_bounds(hit.row) else {
            // 批量生成的结果图层（缓冲区等）没有"行号 → 几何"映射：
            // 只能提示，无法定位。
            self.status = Status::Message(self.tf(
                "find.statusNoGeometry",
                &[&name, &(hit.row + 1).to_string()],
            ));
            return;
        };
        let shown = match self.project_projection.as_ref() {
            Some(project) => display_aabb(bounds, &layer.projection, project),
            None => bounds,
        };
        self.layers.select(hit.layer);
        self.find.selected = Some(index);
        self.find.highlight = Some(shown);
        self.find.highlight_until = now + HIGHLIGHT_SECONDS;
        self.zoom_extent = Some(shown);
        self.status = Status::Message(self.tf("find.status", &[&name, &(hit.row + 1).to_string()]));
    }
}

/// 结果列表：表头 + 整行可点的结果（单击选中、双击定位）。
fn find_results_ui(
    ui: &mut egui::Ui,
    state: &mut Find,
    lang: &crate::i18n::Language,
    locate: &mut Option<usize>,
) {
    if !state.searched {
        ui.weak(lang.t("find.prompt"));
        return;
    }
    if state.results.is_empty() {
        ui.weak(lang.t("find.none"));
        return;
    }
    ui.weak(lang.tf("find.count", &[&state.results.len().to_string()]));
    if state.truncated {
        ui.colored_label(
            style::DANGER,
            lang.tf("find.truncated", &[&MAX_RESULTS.to_string()]),
        );
    }
    ui.add_space(2.0);
    #[cfg(test)]
    state.row_rects.clear();
    egui::ScrollArea::vertical()
        .id_salt("find_results_scroll")
        .max_height(260.0)
        .auto_shrink([false, true])
        .show(ui, |ui| {
            egui::Grid::new("find_results_grid")
                .num_columns(4)
                .striped(true)
                .spacing([12.0, 4.0])
                .show(ui, |ui| {
                    ui.strong(lang.t("find.col.layer"));
                    ui.strong(lang.t("find.col.fid"));
                    ui.strong(lang.t("find.col.field"));
                    ui.strong(lang.t("find.col.value"));
                    ui.end_row();
                    for (index, hit) in state.results.iter().enumerate() {
                        // 四个格子只负责显示，整行的点击交给后面一次 `interact` 统一接管：
                        // 只让最后一列（值）可点的话，双击其它列看起来就是"没反应"。
                        let name_cell = ui.label(short_text(&hit.layer_name, VALUE_CHARS)).rect;
                        let fid_cell = ui.label((hit.row + 1).to_string()).rect;
                        let field = match &hit.field {
                            FindField::Fid => lang.t("find.col.fid").to_string(),
                            FindField::Attr(name) => short_text(name, VALUE_CHARS),
                        };
                        let field_cell = ui.label(field).rect;
                        let value_cell = ui.label(short_text(&hit.value, VALUE_CHARS)).rect;
                        // 点击区域一直铺到列表右缘：文字右边那一片空白也算同一行。
                        let row_right = ui.max_rect().right().max(value_cell.right());
                        let row_rect = name_cell
                            .union(fid_cell)
                            .union(field_cell)
                            .union(value_cell)
                            .with_max_x(row_right)
                            .expand2(egui::vec2(2.0, 1.0));
                        let response = ui.interact(
                            row_rect,
                            ui.make_persistent_id(("find_row", index)),
                            egui::Sense::click(),
                        );
                        // 选中 / 悬停底色：画在文字之后，用半透明色只垫一层，不遮住文字。
                        let selected = state.selected == Some(index);
                        if selected {
                            ui.painter().rect_filled(
                                row_rect,
                                3.0,
                                style::ACCENT.linear_multiply(0.18),
                            );
                            ui.painter().rect_stroke(
                                row_rect,
                                3.0,
                                egui::Stroke::new(1.0_f32, style::ACCENT),
                                egui::StrokeKind::Inside,
                            );
                        } else if response.hovered() {
                            ui.painter().rect_filled(
                                row_rect,
                                3.0,
                                style::ACCENT.linear_multiply(0.08),
                            );
                        }
                        if response.hovered() {
                            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                            response.clone().on_hover_text(lang.t("find.rowHint"));
                        }
                        if response.clicked() {
                            state.selected = Some(index);
                        }
                        // 双击整行任意位置都定位到该要素。
                        if response.double_clicked() {
                            *locate = Some(index);
                        }
                        // 双击不灵光的时候（触控板 / 无障碍）还能右键定位。
                        response.context_menu(|ui| {
                            if ui.button(lang.t("find.locate")).clicked() {
                                *locate = Some(index);
                                ui.close_menu();
                            }
                        });
                        #[cfg(test)]
                        state.row_rects.push(row_rect);
                        ui.end_row();
                    }
                });
        });
}

/// 查找执行体：扫描目标图层的 FID 与全部字段值，收集命中项。
impl Find {
    fn run_search(&mut self, layers: &LayerManager) {
        self.results.clear();
        self.selected = None;
        self.truncated = false;
        self.searched = true;

        // 目标图层：勾选「仅当前图层」时只到选中节点子树里找，否则全工程。
        let mut targets: Vec<&Layer> = Vec::new();
        match (self.only_selected, layers.selected_id()) {
            (true, Some(id)) => {
                if let Some(node) = layers.node(id) {
                    collect_layers(node, true, self.include_hidden, &mut targets);
                }
            }
            _ => {
                for node in layers.roots() {
                    collect_layers(node, true, self.include_hidden, &mut targets);
                }
            }
        }

        let query = self.query.trim();
        // 不区分大小写时先把关键词转小写，逐值比较时只转换文本一次。
        let needle = if self.case_sensitive {
            query.to_string()
        } else {
            query.to_lowercase()
        };
        for layer in targets {
            for (row, attrs) in layer.attrs.iter().enumerate() {
                if self.results.len() >= MAX_RESULTS {
                    self.truncated = true;
                    return;
                }
                // FID 也参与匹配：ArcGIS 的查找同样支持直接用要素 ID 检索。
                let fid_text = (row + 1).to_string();
                if contains(&fid_text, &needle, self.case_sensitive) {
                    self.results.push(FindHit {
                        layer: layer.id,
                        layer_name: layer.name.clone(),
                        row,
                        field: FindField::Fid,
                        value: fid_text,
                    });
                    continue;
                }
                // 属性值：按字段顺序取第一个命中的列。
                let hit = attrs
                    .iter()
                    .enumerate()
                    .find(|(_, value)| contains(value, &needle, self.case_sensitive))
                    .map(|(column, value)| (column, value.clone()));
                let Some((column, value)) = hit else {
                    continue;
                };
                let field = layer
                    .fields
                    .get(column)
                    .cloned()
                    .unwrap_or_else(|| (column + 1).to_string());
                self.results.push(FindHit {
                    layer: layer.id,
                    layer_name: layer.name.clone(),
                    row,
                    field: FindField::Attr(field),
                    value,
                });
            }
        }
    }
}

/// 递归收集节点子树里的矢量图层（`hidden` 为祖先与自身的"有效可见"）。
fn collect_layers<'a>(
    node: &'a LayerNode,
    inherited_visible: bool,
    include_hidden: bool,
    out: &mut Vec<&'a Layer>,
) {
    let visible = inherited_visible && node.visible();
    match node {
        LayerNode::Layer(layer) => {
            if include_hidden || visible {
                out.push(layer);
            }
        }
        LayerNode::Group(group) => {
            for child in &group.children {
                collect_layers(child, visible, include_hidden, out);
            }
        }
        LayerNode::Raster(_) => {}
    }
}

/// 子串匹配（`needle` 已按大小写规则预处理）。
fn contains(text: &str, needle: &str, case_sensitive: bool) -> bool {
    if case_sensitive {
        text.contains(needle)
    } else {
        text.to_lowercase().contains(needle)
    }
}

/// 列表单元格的截断（超长值用省略号，避免把窗口撑到屏幕外）。
fn short_text(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let prefix: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{prefix}…")
    } else {
        prefix
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geo::{FeatureData, Geometry2D};
    use crate::i18n::Language;
    use crate::map::MapView;

    fn layer(name: &str, rows: &[(&str, &str)]) -> Layer {
        let features = rows
            .iter()
            .enumerate()
            .map(|(i, (code, value))| FeatureData {
                geometry: Geometry2D::Point([i as f64, i as f64]),
                attrs: vec![
                    ("NAME".to_string(), code.to_string()),
                    ("VALUE".to_string(), value.to_string()),
                ],
            })
            .collect();
        Layer::from_features(name.to_string(), features)
    }

    fn app_with_layers() -> RustGisApp {
        let ctx = Context::default();
        let mut app = RustGisApp::new(&ctx);
        app.language = Language::Chinese;
        app.map.view = Some(MapView {
            center: [0.0, 0.0],
            scale: 8.0,
        });
        app.layers
            .add(layer("河流", &[("长江", "6300"), ("黄河", "5464")]));
        app.layers.add(layer("湖泊", &[("洞庭湖", "2579")]));
        app
    }

    fn search(app: &mut RustGisApp, query: &str) -> Vec<FindHit> {
        app.find.query = query.to_string();
        app.find.run_search(&app.layers);
        app.find.results.clone()
    }

    #[test]
    fn searches_attribute_values_case_insensitively() {
        let mut app = app_with_layers();
        let hits = search(&mut app, "长江");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].layer_name, "河流");
        assert_eq!(hits[0].row, 0);
        assert_eq!(hits[0].field, FindField::Attr("NAME".to_string()));
        assert_eq!(hits[0].value, "长江");
    }

    #[test]
    fn digit_queries_match_numeric_fields_or_fid() {
        let mut app = app_with_layers();
        // "6300" 命中第一行 VALUE 列。
        let hits = search(&mut app, "6300");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].field, FindField::Attr("VALUE".to_string()));
        // "3" 同时命中 FID 3 与若干属性值，但每个要素最多一条结果。
        let hits = search(&mut app, "3");
        assert!(hits.iter().all(|hit| hit.row <= 2));
        let mut seen = std::collections::HashSet::new();
        assert!(hits.iter().all(|hit| seen.insert((hit.layer, hit.row))));
    }

    #[test]
    fn case_sensitive_flag_switches_matching() {
        let mut app = app_with_layers();
        app.find.case_sensitive = true;
        assert!(search(&mut app, "长江").len() == 1);
        assert!(search(&mut app, "长江x").is_empty());
    }

    #[test]
    fn hidden_layers_are_skipped_unless_requested() {
        let mut app = app_with_layers();
        let lake = app
            .layers
            .roots()
            .iter()
            .find(|node| node.name() == "湖泊")
            .map(|node| node.id())
            .expect("湖泊图层");
        app.layers.set_visible(lake, false);
        assert!(search(&mut app, "洞庭湖").is_empty());
        app.find.include_hidden = true;
        assert_eq!(search(&mut app, "洞庭湖").len(), 1);
    }

    #[test]
    fn empty_query_is_reported_instead_of_returning_everything() {
        let mut app = app_with_layers();
        app.find.query = "   ".to_string();
        app.apply_find_search(Language::Chinese);
        assert!(app.find.results.is_empty());
        assert_eq!(app.find.error.as_deref(), Some("请输入查找内容。"));
    }

    #[test]
    fn locating_a_hit_queues_zoom_and_highlight() {
        let mut app = app_with_layers();
        let hits = search(&mut app, "黄河");
        assert_eq!(hits.len(), 1);
        app.locate_find_hit(0, 10.0);
        assert!(app.zoom_extent.is_some());
        assert!(app.find.highlight.is_some());
        assert!(app.find.highlight_until > 10.0);
        assert!(matches!(app.status, Status::Message(_)));
    }

    #[test]
    fn double_clicking_a_result_locates_the_feature() {
        let ctx = Context::default();
        let mut app = app_with_layers();
        app.open_find();
        app.find.query = "黄河".to_string();
        app.find.run_search(&app.layers);
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1280.0, 800.0));
        let run = |ctx: &Context, app: &mut RustGisApp, time: f64, events: Vec<egui::Event>| {
            let input = egui::RawInput {
                screen_rect: Some(screen),
                time: Some(time),
                events,
                ..Default::default()
            };
            let _ = ctx.run(input, |ctx| app.show_find(ctx));
        };
        // 先跑几帧：`egui::Area`（窗口）要等交互层稳定下来才收得到点击。
        run(&ctx, &mut app, 1.0, Vec::new());
        run(&ctx, &mut app, 1.02, Vec::new());
        run(&ctx, &mut app, 1.04, Vec::new());
        assert_eq!(app.find.row_rects.len(), 1, "应有一行结果");
        let row = app.find.row_rects[0];
        assert!(row.width() > 200.0, "整行（含右侧空白）都应是点击区域");
        // 故意点在左侧的「图层」列（不是最右边的「值」列）：整行都要能双击定位。
        let pos = egui::pos2(row.left() + 8.0, row.center().y);
        let click = || {
            let button = |pressed| egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::default(),
            };
            vec![egui::Event::PointerMoved(pos), button(true), button(false)]
        };
        run(&ctx, &mut app, 1.1, click());
        assert_eq!(app.find.selected, Some(0), "单击应选中该行");
        assert!(app.zoom_extent.is_none(), "单击不定位");
        run(&ctx, &mut app, 1.15, click());
        assert!(app.zoom_extent.is_some(), "双击应排队缩放到该要素");
        assert!(app.find.highlight.is_some(), "双击应高亮该要素");
    }

    #[test]
    fn window_renders_without_panicking() {
        let ctx = Context::default();
        let mut app = app_with_layers();
        app.open_find();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1280.0, 800.0),
            )),
            ..Default::default()
        };
        let _ = ctx.run(input, |ctx| app.show_find(ctx));
        assert!(app.find.open);
    }

    #[test]
    fn long_values_are_ellipsized() {
        assert_eq!(short_text("abc", 64), "abc");
        let long = "x".repeat(80);
        assert!(short_text(&long, 64).ends_with('…'));
    }
}
