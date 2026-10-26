//! 「转到 XY」：输入工程坐标系下的 X / Y，居中地图并在目标位置短暂标记。
//!
//! 与 ArcGIS 的 Go To XY 一致：坐标按**当前工程 CRS**解释（经纬度工程就是
//! 经度 / 纬度，投影工程就是东 / 北坐标），定位只改变视图中心、不改变比例尺，
//! 并在目标点画一个几秒钟的红色十字环，避免"点完看不出跳到哪了"。

use egui::{Context, Id, Key, TextEdit};

use super::{RustGisApp, Status};
use crate::map::MapView;
use crate::style;

/// 定位标记的显示时长（秒）。
const MARKER_SECONDS: f64 = 4.0;

/// 「转到 XY」窗口状态（跨帧保存输入内容与定位标记）。
#[derive(Default)]
pub(super) struct GoToXy {
    pub open: bool,
    x_text: String,
    y_text: String,
    error: Option<String>,
    /// 最近一次定位点（工程 CRS）。
    pub(super) marker: Option<[f64; 2]>,
    /// 标记过期时刻（`egui` 时间轴，秒）。
    pub(super) marker_until: f64,
    /// 刚打开窗口：下一帧把焦点放到 X 输入框。
    focus: bool,
}

impl RustGisApp {
    /// 打开（或聚焦）「转到 XY」窗口。首次打开时用当前视图中心填好输入框。
    pub(super) fn open_goto_xy(&mut self) {
        let center = self.map.view.map(|view| view.center);
        let state = &mut self.goto_xy;
        if !state.open {
            if state.x_text.trim().is_empty()
                && state.y_text.trim().is_empty()
                && let Some(center) = center
            {
                state.x_text = format_input(center[0]);
                state.y_text = format_input(center[1]);
            }
            state.open = true;
            state.error = None;
        }
        state.focus = true;
    }

    /// 「转到 XY」浮窗（非模态：开着时仍可缩放 / 平移地图）。
    pub(super) fn show_goto_xy(&mut self, ctx: &Context) {
        if !self.goto_xy.open {
            return;
        }
        let mut state = std::mem::take(&mut self.goto_xy);
        let lang = self.language;
        let now = ctx.input(|input| input.time);
        let mut open = true;
        let mut cancel = false;
        let mut go = false;
        let pos = egui::pos2(
            (ctx.screen_rect().right() - 330.0).max(16.0),
            ctx.screen_rect().top() + 96.0,
        );
        egui::Window::new(lang.t("goto_xy.title"))
            .id(Id::new("goto_xy_window"))
            .open(&mut open)
            .default_pos(pos)
            .default_width(300.0)
            .resizable(false)
            .show(ctx, |ui| {
                ui.add_space(2.0);
                let x_id = ui.make_persistent_id("goto_xy_x");
                egui::Grid::new("goto_xy_form")
                    .num_columns(2)
                    .spacing([8.0, 6.0])
                    .show(ui, |ui| {
                        ui.label(lang.t("goto_xy.x"));
                        ui.add(
                            TextEdit::singleline(&mut state.x_text)
                                .id(x_id)
                                .desired_width(180.0),
                        );
                        ui.end_row();
                        ui.label(lang.t("goto_xy.y"));
                        ui.add(TextEdit::singleline(&mut state.y_text).desired_width(180.0));
                        ui.end_row();
                    });
                if state.focus {
                    state.focus = false;
                    ui.memory_mut(|memory| memory.request_focus(x_id));
                }
                ui.add_space(2.0);
                ui.weak(lang.t("goto_xy.hint"));
                if let Some(error) = &state.error {
                    ui.colored_label(style::DANGER, error);
                }
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if ui.button(lang.t("goto_xy.go")).clicked() {
                        go = true;
                    }
                    if ui.button(lang.t("dialog.cancel")).clicked() {
                        cancel = true;
                    }
                });
                // 回车 = 定位（窗口开着时用户通常就在输入框里）。
                if ui.input(|input| input.key_pressed(Key::Enter)) {
                    go = true;
                }
            });
        if cancel {
            open = false;
        }
        state.open = open;
        self.goto_xy = state;
        if go {
            self.apply_goto_xy(now);
        }
    }

    /// 校验输入并定位；失败时把原因留在窗口里，视图保持不变。
    fn apply_goto_xy(&mut self, now: f64) {
        let x_text = self.goto_xy.x_text.trim();
        let y_text = self.goto_xy.y_text.trim();
        if x_text.is_empty() || y_text.is_empty() {
            self.goto_xy.error = Some(self.t("goto_xy.error.empty").to_string());
            return;
        }
        let (Ok(x), Ok(y)) = (x_text.parse::<f64>(), y_text.parse::<f64>()) else {
            self.goto_xy.error = Some(self.t("goto_xy.error.invalid").to_string());
            return;
        };
        if !x.is_finite() || !y.is_finite() {
            self.goto_xy.error = Some(self.t("goto_xy.error.invalid").to_string());
            return;
        }
        self.goto_xy.error = None;
        match self.map.view.as_mut() {
            Some(view) => view.center = [x, y],
            // 视图尚未初始化（空工程刚打开窗口）：给一个可用的默认比例尺。
            None => {
                self.map.view = Some(MapView {
                    center: [x, y],
                    scale: 100.0,
                });
            }
        }
        self.goto_xy.marker = Some([x, y]);
        self.goto_xy.marker_until = now + MARKER_SECONDS;
        self.dirty = true;
        self.status =
            Status::Message(self.tf("goto_xy.status", &[&format_input(x), &format_input(y)]));
    }
}

/// 输入框预填值：最多 6 位小数，去掉无意义的尾零。
fn format_input(value: f64) -> String {
    let text = format!("{value:.6}");
    text.trim_end_matches('0').trim_end_matches('.').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::Language;

    fn app_at(center: [f64; 2]) -> RustGisApp {
        let ctx = Context::default();
        let mut app = RustGisApp::new(&ctx);
        app.language = Language::Chinese;
        app.map.view = Some(MapView { center, scale: 8.0 });
        app
    }

    #[test]
    fn open_prefills_current_view_center_once() {
        let mut app = app_at([116.4, 39.9]);
        app.open_goto_xy();
        assert!(app.goto_xy.open);
        assert_eq!(app.goto_xy.x_text, "116.4");
        assert_eq!(app.goto_xy.y_text, "39.9");
        // 用户改过输入后重新打开：保留用户输入，不覆盖成新的视图中心。
        app.goto_xy.x_text = "120".to_string();
        app.goto_xy.y_text = "30".to_string();
        app.map.view = Some(MapView {
            center: [0.0, 0.0],
            scale: 8.0,
        });
        app.open_goto_xy();
        assert_eq!(app.goto_xy.x_text, "120");
    }

    #[test]
    fn valid_coordinates_center_the_view_and_set_marker() {
        let mut app = app_at([0.0, 0.0]);
        app.open_goto_xy();
        app.goto_xy.x_text = "400000".to_string();
        app.goto_xy.y_text = "5000000".to_string();
        app.apply_goto_xy(10.0);
        assert_eq!(app.map.view.expect("view").center, [400000.0, 5000000.0]);
        assert_eq!(app.goto_xy.marker, Some([400000.0, 5000000.0]));
        assert!(app.goto_xy.marker_until > 10.0);
        assert!(app.goto_xy.error.is_none());
        assert!(matches!(app.status, Status::Message(_)));
    }

    #[test]
    fn invalid_coordinates_keep_the_view_and_report_errors() {
        let mut app = app_at([1.0, 2.0]);
        app.open_goto_xy();
        app.goto_xy.x_text.clear();
        app.goto_xy.y_text = "30".to_string();
        app.apply_goto_xy(0.0);
        assert_eq!(app.goto_xy.error.as_deref(), Some("请输入 X、Y 坐标。"));
        assert_eq!(app.map.view.expect("view").center, [1.0, 2.0]);

        app.goto_xy.x_text = "abc".to_string();
        app.apply_goto_xy(0.0);
        assert_eq!(app.goto_xy.error.as_deref(), Some("坐标必须是有效数字。"));
        assert_eq!(app.map.view.expect("view").center, [1.0, 2.0]);
    }

    #[test]
    fn window_renders_without_panicking() {
        let ctx = Context::default();
        let mut app = app_at([10.0, 20.0]);
        app.open_goto_xy();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1280.0, 800.0),
            )),
            ..Default::default()
        };
        let _ = ctx.run(input, |ctx| app.show_goto_xy(ctx));
        assert!(app.goto_xy.open);
    }

    #[test]
    fn input_formatter_drops_trailing_zeros() {
        assert_eq!(format_input(116.4), "116.4");
        assert_eq!(format_input(400000.0), "400000");
        assert_eq!(format_input(0.1234567), "0.123457");
    }
}
