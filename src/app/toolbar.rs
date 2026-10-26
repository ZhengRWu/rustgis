//! Compact desktop GIS toolbars. Icons are drawn in a 16-point coordinate grid,
//! so they remain crisp at different display scales without image decoders.
use super::RustGisApp;
use egui::{Color32 as C, Pos2, Rect, RichText, Sense, Shape, Stroke, StrokeKind, Ui, Vec2};

#[derive(Clone, Copy, Debug)]
enum Icon {
    New,
    Open,
    Save,
    Print,
    Cut,
    Copy,
    Paste,
    Delete,
    Undo,
    Redo,
    Add,
    Edit,
    Table,
    Catalog,
    Search,
    Toolbox,
    Console,
    Model,
    ZoomIn,
    ZoomOut,
    Pan,
    Globe,
    Expand,
    Contract,
    Back,
    Forward,
    Select,
    Clear,
    Pointer,
    Info,
    Link,
    Tip,
    Measure,
    Find,
    Xy,
    Time,
}

impl Icon {
    /// 全部图标（测试用：保证每个图标都配了标题和说明）。
    #[cfg(test)]
    const ALL: [Icon; 36] = [
        Icon::New,
        Icon::Open,
        Icon::Save,
        Icon::Print,
        Icon::Cut,
        Icon::Copy,
        Icon::Paste,
        Icon::Delete,
        Icon::Undo,
        Icon::Redo,
        Icon::Add,
        Icon::Edit,
        Icon::Table,
        Icon::Catalog,
        Icon::Search,
        Icon::Toolbox,
        Icon::Console,
        Icon::Model,
        Icon::ZoomIn,
        Icon::ZoomOut,
        Icon::Pan,
        Icon::Globe,
        Icon::Expand,
        Icon::Contract,
        Icon::Back,
        Icon::Forward,
        Icon::Select,
        Icon::Clear,
        Icon::Pointer,
        Icon::Info,
        Icon::Link,
        Icon::Tip,
        Icon::Measure,
        Icon::Find,
        Icon::Xy,
        Icon::Time,
    ];
}

/// 每个工具按钮的悬停提示：(标题 key, 工具介绍 key)。
///
/// 标题优先复用菜单文案；图标语义与菜单项不同的（如铅笔 = 编辑器、X = 删除）
/// 用单独的 key，避免提示里出现牛头不对马嘴的名字。
fn tip_keys(icon: Icon) -> (&'static str, &'static str) {
    match icon {
        Icon::New => ("file.new", "tip.new"),
        Icon::Open => ("file.open", "tip.open"),
        Icon::Save => ("file.save", "tip.save"),
        Icon::Print => ("file.print", "tip.print"),
        Icon::Cut => ("edit.cut", "tip.cut"),
        Icon::Copy => ("edit.copy", "tip.copy"),
        Icon::Paste => ("edit.paste", "tip.paste"),
        Icon::Delete => ("edit.delete", "tip.delete"),
        Icon::Undo => ("edit.undo", "tip.undo"),
        Icon::Redo => ("edit.redo", "tip.redo"),
        Icon::Add => ("layer.add", "tip.add"),
        Icon::Edit => ("tools.editor", "tip.edit"),
        Icon::Table => ("window.contents", "tip.toc"),
        Icon::Catalog => ("window.catalog", "tip.catalog"),
        Icon::Search => ("window.find", "tip.find"),
        Icon::Toolbox => ("toolbox.open", "tip.toolbox"),
        Icon::Console => ("tools.python", "tip.console"),
        Icon::Model => ("tools.modelBuilder", "tip.model"),
        Icon::ZoomIn => ("view.zoomIn", "tip.zoomIn"),
        Icon::ZoomOut => ("view.zoomOut", "tip.zoomOut"),
        Icon::Pan => ("view.pan", "tip.pan"),
        Icon::Globe => ("view.zoomFull", "tip.globe"),
        Icon::Expand => ("view.zoomIn", "tip.expand"),
        Icon::Contract => ("view.zoomOut", "tip.contract"),
        Icon::Back => ("view.prevExtent", "tip.back"),
        Icon::Forward => ("view.nextExtent", "tip.forward"),
        Icon::Select => ("select.bySquare", "tip.select"),
        Icon::Clear => ("select.deselect", "tip.clear"),
        Icon::Pointer => ("select.features", "tip.pointer"),
        Icon::Info => ("edit.identify", "tip.info"),
        Icon::Link => ("insert.hyperlink", "tip.link"),
        Icon::Tip => ("help.tips", "tip.tip"),
        Icon::Measure => ("tools.measure", "tip.measure"),
        Icon::Find => ("window.find", "tip.find"),
        Icon::Xy => ("view.gotoXy", "tip.xy"),
        Icon::Time => ("view.timeSlider", "tip.time"),
    }
}

impl RustGisApp {
    pub(super) fn show_toolbars(&mut self, ui: &mut Ui) {
        // Horizontal scrolling preserves the two rows even in small windows.
        egui::ScrollArea::horizontal()
            .id_salt("toolbar_scroll")
            .auto_shrink([false, true])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing = Vec2::new(1.0, 1.0);
                ui.horizontal(|ui| {
                    grip(ui);
                    for (icon, key, enabled) in [
                        (Icon::New, "file.new", true),
                        (Icon::Open, "file.open", true),
                        (Icon::Save, "file.save", true),
                        (Icon::Print, "file.print", false),
                    ] {
                        self.tool_button(ui, icon, key, enabled, false);
                    }
                    separator(ui);
                    for (icon, key) in [
                        (Icon::Cut, "edit.cut"),
                        (Icon::Copy, "edit.copy"),
                        (Icon::Paste, "edit.paste"),
                        (Icon::Delete, "select.deselect"),
                    ] {
                        self.tool_button(ui, icon, key, false, false);
                    }
                    separator(ui);
                    self.tool_button(ui, Icon::Undo, "edit.undo", false, false);
                    self.tool_button(ui, Icon::Redo, "edit.redo", false, false);
                    separator(ui);
                    self.tool_button(ui, Icon::Add, "layer.add", true, false);
                    ui.menu_button("▾", |ui| {
                        for key in ["layer.add", "layer.addRaster", "layer.addSafe"] {
                            if ui.button(self.t(key)).clicked() {
                                self.on_menu_item(key);
                                ui.close_menu();
                            }
                        }
                    });
                    // Same conversion as the existing status bar; do not display a
                    // fabricated reference-image scale when no view exists.
                    let label = self
                        .map
                        .view
                        .map(|v| {
                            let ppp = f64::from(ui.ctx().pixels_per_point()).max(1.0);
                            format!("1 : {:.0}", 1.0 / (f64::from(v.scale) * ppp * 0.00028))
                        })
                        .unwrap_or_else(|| "1 : —".into());
                    egui::ComboBox::from_id_salt("toolbar_scale")
                        .width(140.0)
                        .selected_text(label)
                        .show_ui(ui, |ui| {
                            for denominator in
                                [1000., 5000., 10000., 25000., 50000., 100000., 1000000.]
                            {
                                if ui.button(format!("1 : {denominator:.0}")).clicked() {
                                    let ppp = f64::from(ui.ctx().pixels_per_point()).max(1.0);
                                    if let Some(v) = self.map.view.as_mut() {
                                        v.scale = (1.0 / (denominator * ppp * 0.00028)) as f32;
                                        self.dirty = true;
                                    }
                                    ui.close_menu();
                                }
                            }
                        });
                    separator(ui);
                    for (icon, key) in [
                        (Icon::Edit, "edit.properties"),
                        (Icon::Table, "window.contents"),
                        (Icon::Catalog, "window.catalog"),
                        (Icon::Search, "window.find"),
                        (Icon::Toolbox, "toolbox.open"),
                        (Icon::Console, "tools.options"),
                        (Icon::Model, "tools.modelBuilder"),
                    ] {
                        let enabled = matches!(icon, Icon::Catalog | Icon::Toolbox | Icon::Search);
                        self.tool_button(ui, icon, key, enabled, false);
                    }
                });
                ui.horizontal(|ui| {
                    grip(ui);
                    for (icon, key, enabled) in [
                        (Icon::ZoomIn, "view.zoomIn", true),
                        (Icon::ZoomOut, "view.zoomOut", true),
                        (Icon::Pan, "view.pan", true),
                        (Icon::Globe, "view.zoomFull", true),
                        (Icon::Expand, "view.zoomIn", true),
                        (Icon::Contract, "view.zoomOut", true),
                        (Icon::Back, "edit.undo", false),
                        (Icon::Forward, "edit.redo", false),
                    ] {
                        let selected = matches!(
                            (icon, self.map.tool),
                            (Icon::Pan, crate::map::Tool::Pan)
                                | (Icon::ZoomIn, crate::map::Tool::ZoomIn)
                                | (Icon::ZoomOut, crate::map::Tool::ZoomOut)
                                | (Icon::Measure, crate::map::Tool::Measure)
                        );
                        self.tool_button(ui, icon, key, enabled, selected);
                    }
                    separator(ui);
                    for (icon, key, enabled) in [
                        (Icon::Select, "select.bySquare", false),
                        (Icon::Clear, "select.deselect", false),
                        (Icon::Pointer, "select.byAttribute", false),
                        (Icon::Info, "edit.identify", true),
                        (Icon::Link, "insert.text", false),
                        (Icon::Tip, "help.help", false),
                        (Icon::Measure, "tools.measure", true),
                        (Icon::Find, "window.find", true),
                        (Icon::Xy, "view.gotoXy", true),
                        (Icon::Time, "view.layout", false),
                    ] {
                        let selected =
                            matches!((icon, self.map.tool), (Icon::Info, crate::map::Tool::Info) | (Icon::Measure, crate::map::Tool::Measure));
                        self.tool_button(ui, icon, key, enabled, selected);
                    }
                });
            });
    }

    fn tool_button(&mut self, ui: &mut Ui, icon: Icon, key: &str, enabled: bool, selected: bool) {
        let (title_key, desc_key) = tip_keys(icon);
        let title = self.t(title_key).to_string();
        let body = self.t(desc_key).to_string();
        let note = (!enabled).then(|| self.t("toolbar.notImplemented").to_string());
        let response = ui
            .add_enabled_ui(enabled, |ui| {
                let (r, response) = ui.allocate_exact_size(Vec2::splat(22.0), Sense::click());
                if selected || response.hovered() {
                    ui.painter().rect(
                        r,
                        0.0,
                        C::from_rgb(220, 237, 250),
                        Stroke::new(1.0_f32, C::from_rgb(90, 151, 199)),
                        StrokeKind::Inside,
                    );
                }
                draw_icon(ui.painter(), r.shrink(3.0), icon);
                response
            })
            .inner;
        // 灰色（禁用）按钮也要出提示：egui 的 on_hover_ui 对禁用控件不触发，
        // 必须走 on_disabled_hover_ui（它按控件矩形判断指针，不依赖 hovered）。
        let response = if enabled {
            response.on_hover_ui(|ui| tool_tip(ui, &title, &body, None))
        } else {
            response.on_disabled_hover_ui(|ui| tool_tip(ui, &title, &body, note.as_deref()))
        };
        if response.clicked() {
            match icon {
                Icon::ZoomIn | Icon::ZoomOut | Icon::Pan | Icon::Info | Icon::Measure => {
                    self.map.tool = match icon {
                        Icon::ZoomIn => crate::map::Tool::ZoomIn,
                        Icon::ZoomOut => crate::map::Tool::ZoomOut,
                        Icon::Info => crate::map::Tool::Info,
                        Icon::Measure => crate::map::Tool::Measure,
                        _ => crate::map::Tool::Pan,
                    };
                    self.map.zoom_drag_start = None;
                }
                _ => {
                    self.on_menu_item(key);
                }
            }
        }
    }
}

/// 悬停提示的排版：粗体标题 + 工具介绍；禁用工具再补一行「尚未实现」。
fn tool_tip(ui: &mut Ui, title: &str, body: &str, note: Option<&str>) {
    ui.set_max_width(280.0);
    ui.label(RichText::new(title).strong());
    ui.add_space(2.0);
    ui.label(body);
    if let Some(note) = note {
        ui.add_space(2.0);
        ui.weak(note);
    }
}

fn grip(ui: &mut Ui) {
    let (r, _) = ui.allocate_exact_size(Vec2::new(4.0, 22.0), Sense::hover());
    for y in [5., 8., 11., 14., 17.] {
        ui.painter()
            .circle_filled(r.min + Vec2::new(1., y), 0.6, C::GRAY);
    }
}
fn separator(ui: &mut Ui) {
    let (r, _) = ui.allocate_exact_size(Vec2::new(5., 22.), Sense::hover());
    ui.painter().line_segment(
        [
            r.center_top() + Vec2::new(0., 3.),
            r.center_bottom() - Vec2::new(0., 3.),
        ],
        Stroke::new(1.0_f32, C::from_gray(210)),
    );
}

fn draw_icon(p: &egui::Painter, r: Rect, icon: Icon) {
    let pt = |x: f32, y: f32| {
        Pos2::new(
            r.min.x + x * r.width() / 16.,
            r.min.y + y * r.height() / 16.,
        )
    };
    let dark = C::from_rgb(58, 69, 75);
    let blue = C::from_rgb(48, 137, 195);
    let gold = C::from_rgb(235, 195, 91);
    let green = C::from_rgb(111, 158, 73);
    let line = |a: (f32, f32), b: (f32, f32), c: C| {
        p.line_segment([pt(a.0, a.1), pt(b.0, b.1)], Stroke::new(1.0_f32, c));
    };
    let rect = |x, y, w, h, c| {
        p.rect(
            Rect::from_min_max(pt(x, y), pt(x + w, y + h)),
            0.,
            c,
            Stroke::new(0.8_f32, dark),
            StrokeKind::Inside,
        );
    };
    let poly = |v: &[(f32, f32)], c| {
        p.add(Shape::convex_polygon(
            v.iter().map(|&(x, y)| pt(x, y)).collect(),
            c,
            Stroke::new(0.8_f32, dark),
        ));
    };
    match icon {
        Icon::New | Icon::Copy | Icon::Paste => {
            if matches!(icon, Icon::Paste) {
                rect(1., 2., 11., 13., gold);
                rect(4., 1., 5., 3., C::LIGHT_GRAY);
            }
            if matches!(icon, Icon::Copy) {
                rect(1., 1., 9., 12., C::WHITE);
            }
            poly(
                &[(5., 2.), (11., 2.), (14., 5.), (14., 15.), (5., 15.)],
                C::WHITE,
            );
            line((11., 2.), (11., 5.), blue);
            line((11., 5.), (14., 5.), blue);
            if !matches!(icon, Icon::New) {
                for y in [7., 9., 11.] {
                    line((7., y), (12., y), blue);
                }
            }
        }
        Icon::Open | Icon::Catalog => {
            rect(1., 4., 13., 10., gold);
            rect(2., 2., 6., 4., gold);
            poly(
                &[(3., 7.), (16., 7.), (13., 14.), (1., 14.)],
                C::from_rgb(250, 220, 136),
            );
            if matches!(icon, Icon::Catalog) {
                rect(8., 3., 6., 9., blue);
            }
        }
        Icon::Save => {
            rect(2., 1., 12., 14., blue);
            rect(4., 1., 7., 5., C::LIGHT_GRAY);
            rect(4., 9., 8., 6., C::WHITE);
            line((9., 2.), (9., 5.), dark);
        }
        Icon::Print => {
            rect(4., 1., 9., 6., C::WHITE);
            rect(1., 6., 14., 7., C::LIGHT_GRAY);
            rect(4., 10., 9., 5., C::WHITE);
            line((3., 8.), (5., 8.), green);
        }
        Icon::Cut => {
            for x in [4., 11.] {
                p.circle_stroke(pt(x, 12.), 2.2, Stroke::new(1.0_f32, blue));
            }
            line((4., 10.), (12., 1.), dark);
            line((11., 10.), (4., 1.), dark);
        }
        Icon::Delete | Icon::Clear => {
            if matches!(icon, Icon::Clear) {
                rect(2., 2., 11., 11., C::TRANSPARENT);
            }
            line((4., 4.), (12., 12.), dark);
            line((12., 4.), (4., 12.), dark);
        }
        Icon::Undo | Icon::Redo | Icon::Back | Icon::Forward => {
            let right = matches!(icon, Icon::Redo | Icon::Forward);
            let x = |v| if right { 16. - v } else { v };
            poly(
                &[
                    (x(1.), 8.),
                    (x(7.), 3.),
                    (x(7.), 6.),
                    (x(14.), 6.),
                    (x(14.), 10.),
                    (x(7.), 10.),
                    (x(7.), 13.),
                ],
                blue,
            );
        }
        Icon::Add => {
            poly(&[(8., 1.), (15., 7.), (8., 13.), (1., 7.)], gold);
            line((8., 3.), (8., 11.), dark);
            line((4., 7.), (12., 7.), dark);
        }
        Icon::ZoomIn | Icon::ZoomOut | Icon::Search => {
            p.circle_filled(pt(6., 6.), 4.8, C::from_rgb(231, 245, 250));
            p.circle_stroke(pt(6., 6.), 4.8, Stroke::new(1.0_f32, dark));
            p.line_segment([pt(10., 10.), pt(15., 15.)], Stroke::new(2.5_f32, dark));
            line((3., 6.), (9., 6.), dark);
            if matches!(icon, Icon::ZoomIn) {
                line((6., 3.), (6., 9.), dark);
            }
        }
        Icon::Pan => {
            poly(
                &[
                    (5., 15.),
                    (2., 10.),
                    (1., 7.),
                    (3., 6.),
                    (5., 9.),
                    (4., 3.),
                    (6., 2.),
                    (7., 7.),
                    (7., 1.),
                    (9., 1.),
                    (10., 7.),
                    (11., 2.),
                    (13., 3.),
                    (12., 8.),
                    (14., 5.),
                    (15., 6.),
                    (13., 13.),
                    (11., 15.),
                ],
                C::from_rgb(247, 239, 213),
            );
        }
        Icon::Globe => {
            p.circle_filled(pt(8., 8.), 7., blue);
            poly(&[(4., 2.), (8., 2.), (9., 5.), (6., 7.), (3., 6.)], green);
            poly(
                &[(9., 8.), (14., 7.), (13., 12.), (10., 14.), (8., 11.)],
                green,
            );
            p.circle_stroke(pt(8., 8.), 7., Stroke::new(1.0_f32, dark));
        }
        Icon::Expand | Icon::Contract => {
            for (x, y, sx, sy) in [
                (1., 1., 1., 1.),
                (15., 1., -1., 1.),
                (1., 15., 1., -1.),
                (15., 15., -1., -1.),
            ] {
                let (x, y, sx, sy) = if matches!(icon, Icon::Contract) {
                    (x + sx * 5., y + sy * 5., -sx, -sy)
                } else {
                    (x, y, sx, sy)
                };
                poly(&[(x, y), (x + sx * 5., y), (x, y + sy * 5.)], dark);
            }
        }
        Icon::Pointer | Icon::Select => {
            if matches!(icon, Icon::Select) {
                rect(1., 1., 10., 10., C::from_rgb(221, 241, 249));
            }
            poly(
                &[
                    (5., 3.),
                    (5., 14.),
                    (8., 11.),
                    (11., 15.),
                    (13., 14.),
                    (10., 10.),
                    (14., 10.),
                ],
                dark,
            );
        }
        Icon::Info => {
            p.circle_filled(pt(8., 8.), 7., blue);
            p.text(
                pt(8., 8.),
                egui::Align2::CENTER_CENTER,
                "i",
                egui::FontId::proportional(15.),
                C::WHITE,
            );
        }
        Icon::Link => {
            poly(
                &[(9., 0.), (3., 9.), (8., 9.), (6., 16.), (14., 6.), (9., 6.)],
                gold,
            );
        }
        Icon::Tip => {
            rect(1., 2., 14., 10., C::from_rgb(251, 248, 210));
            poly(&[(4., 11.), (4., 15.), (8., 11.)], C::WHITE);
            for y in [5., 8.] {
                line((4., y), (12., y), blue);
            }
        }
        Icon::Measure => {
            rect(1., 8., 14., 6., gold);
            for x in [3., 6., 9., 12.] {
                line((x, 8.), (x, 11.), dark);
            }
            line((2., 3.), (14., 3.), blue);
        }
        Icon::Find => {
            for x in [1., 9.] {
                rect(x, 6., 6., 8., dark);
                rect(x + 1., 2., 4., 5., C::GRAY);
            }
            line((6., 8.), (10., 8.), dark);
        }
        Icon::Xy => {
            p.text(
                pt(8., 8.),
                egui::Align2::CENTER_CENTER,
                "XY",
                egui::FontId::monospace(11.),
                blue,
            );
            line((8., 0.), (8., 3.), dark);
            line((8., 13.), (8., 16.), dark);
        }
        Icon::Time => {
            p.circle_stroke(pt(8., 8.), 6., Stroke::new(1.0_f32, dark));
            line((8., 8.), (8., 3.), blue);
            line((8., 8.), (12., 10.), blue);
        }
        Icon::Toolbox => {
            rect(1., 5., 14., 9., C::from_rgb(185, 73, 55));
            rect(5., 2., 6., 4., C::TRANSPARENT);
            line((1., 9.), (15., 9.), gold);
        }
        Icon::Model => {
            line((4., 4.), (12., 12.), dark);
            line((4., 12.), (12., 4.), dark);
            for (x, y, c) in [
                (1., 1., gold),
                (10., 1., blue),
                (1., 10., green),
                (10., 10., gold),
            ] {
                rect(x, y, 5., 5., c);
            }
        }
        Icon::Edit => {
            rect(1., 2., 13., 12., C::WHITE);
            poly(
                &[(3., 12.), (5., 8.), (13., 1.), (15., 3.), (7., 11.)],
                gold,
            );
        }
        Icon::Table | Icon::Console => {
            rect(1., 2., 14., 12., C::WHITE);
            rect(1., 2., 14., 3., blue);
            if matches!(icon, Icon::Console) {
                line((4., 7.), (6., 9.), dark);
                line((6., 9.), (4., 11.), dark);
                line((8., 11.), (12., 11.), dark);
            } else {
                for y in [8., 11.] {
                    line((2., y), (14., y), C::GRAY);
                }
                for x in [5., 10.] {
                    line((x, 6.), (x, 13.), C::GRAY);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::Language;

    #[test]
    fn every_icon_has_title_and_description_in_all_languages() {
        for icon in Icon::ALL {
            let (title_key, desc_key) = tip_keys(icon);
            for lang in Language::ALL {
                assert_ne!(lang.t(title_key), title_key, "{icon:?} 缺标题 {title_key}");
                assert_ne!(lang.t(desc_key), desc_key, "{icon:?} 缺说明 {desc_key}");
            }
        }
    }
}
