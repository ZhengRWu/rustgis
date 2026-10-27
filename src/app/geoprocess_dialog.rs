//! 地理处理工具对话框（裁剪 / 相交 / 联合 / 合并 / 融合）+ 后台流水线。
//!
//! 与 [`super::buffer_dialog`] 完全同一套架构（缓冲区是同一个工具箱里的兄弟
//! 工具，只是它先落地）：对话框跑在**独立的系统窗口**里（egui 多视口），窗口
//! 回调必须 `Send + Sync + 'static`，所以界面状态放 `Arc<Mutex<..>>`、动作经
//! 邮箱投回主线程；主线程每帧把图层列表与当前语言文案同步进去。
//!
//! 与缓冲区对话框的差别只有"参数"这一块：
//!
//! - **输入要素是列表**（叠加类工具普遍是多输入）：勾选顺序就是叠加顺序，
//!   `inputs[0]` 是主输入，其余依次叠加；裁剪时第 2 个即"裁剪要素"；
//! - **融合字段**只在融合工具里出现（`OverlayOp::uses_fields`）；
//! - 参数没有距离 / 侧类型 / 末端类型这类缓冲特有的东西。
//!
//! 面板顶部还有"工具"一行——切换工具不必关掉窗口重开，参数会就地重算。

use std::path::PathBuf;
use std::sync::mpsc::{self, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use egui::{Context, RichText, TextEdit, ViewportBuilder, ViewportId};

use crate::dialog::Dialog;
use crate::geo::buffer::write_geojson;
use crate::geo::overlay::{self, OverlayOp};
use crate::geo::{Layer, LayerNode};
use crate::style;

use super::widgets;
use super::{RustGisApp, Status};

/// 表单列宽（ArcGIS 对话框左栏）。
const FORM_WIDTH: f32 = 440.0;
/// 帮助列宽（ArcGIS 对话框右栏）。
const HELP_WIDTH: f32 = 260.0;
/// 输入要素列表的最大高度（超出滚动）。
const INPUT_LIST_HEIGHT: f32 = 118.0;
/// 融合字段列表的最大高度（超出滚动）。
const FIELDS_LIST_HEIGHT: f32 = 118.0;
/// 独立窗口的初始尺寸（表单 + 帮助 + 边距）。
const WINDOW_WIDTH: f32 = FORM_WIDTH + HELP_WIDTH + 48.0;
const WINDOW_HEIGHT: f32 = 600.0;
/// 独立窗口的最小尺寸（缩小后表单改为滚动）。
const MIN_WINDOW_WIDTH: f32 = 520.0;
const MIN_WINDOW_HEIGHT: f32 = 400.0;

/// 右侧帮助面板当前展示的参数。
#[derive(Clone, Copy, PartialEq, Eq)]
enum HelpTopic {
    /// 工具说明（默认：讲这个工具干什么）。
    Tool,
    Inputs,
    Output,
    Fields,
}

/// 对话框中可选的输入图层（含属性字段名，供「融合字段」使用）。
struct LayerChoice {
    id: u64,
    name: String,
    fields: Vec<String>,
}

/// 独立窗口发给主线程的动作。
///
/// `Debug + PartialEq` 是为了测试里能直接 `assert_eq!(dialog.request(), ...)`。
#[derive(Debug, PartialEq)]
pub(super) enum GeoMsg {
    /// 确定：表单已通过校验。`input_ids` 的顺序即叠加顺序。
    Accept {
        input_ids: Vec<u64>,
        output_path: PathBuf,
    },
    /// 取消（「取消」按钮或窗口标题栏的关闭按钮）。
    Cancel,
    /// 输入要素右侧的文件夹按钮：从文件添加图层（等价菜单「添加数据」）。
    AddLayer,
    /// 尚未实现的入口（「环境...」/「工具帮助」），沿用统一占位提示。
    Dev(&'static str),
}

/// 独立的地理处理窗口：界面状态 + 与主线程通信的邮箱。
pub(super) struct GeoWindow {
    /// 视口 id：必须每帧用同一个 id 显示视口，窗口才会持续存在。
    id: ViewportId,
    /// 界面状态（窗口回调与主线程共享）。
    state: Arc<Mutex<GeoDialog>>,
    /// 窗口 → 主线程的动作邮箱。
    msgs: Arc<Mutex<Vec<GeoMsg>>>,
}

/// 「地理处理」窗口的全部界面状态。
struct GeoDialog {
    /// 当前工具。
    op: OverlayOp,
    /// 勾选的输入图层 id，顺序即叠加顺序。
    inputs: Vec<u64>,
    /// 输出要素类路径（GeoJSON）。
    output_path: String,
    /// 用户是否手改过输出路径：改过之后就不再跟随输入图层自动改名。
    output_edited: bool,
    /// 已勾选的融合字段（仅融合工具使用）。
    checked: Vec<String>,
    show_help: bool,
    topic: HelpTopic,
    /// 校验失败的原因（显示在窗口底部，直到下次点确定）。
    error: Option<String>,
    /// 刚打开 / 切工具：下一帧把键盘焦点抢到独立窗口上。
    focus: bool,
    /// 「输出要素类」文件夹按钮弹出的系统保存对话框（后台线程，不阻塞 UI）。
    browse: Option<mpsc::Receiver<Option<PathBuf>>>,
    /// 主线程每帧刷新：可选输入图层。
    choices: Vec<LayerChoice>,
    /// 主线程每帧刷新：当前语言的界面文案。
    texts: Texts,
}

impl Default for GeoDialog {
    fn default() -> Self {
        Self {
            op: OverlayOp::Clip,
            inputs: Vec::new(),
            output_path: String::new(),
            output_edited: false,
            checked: Vec::new(),
            show_help: true,
            topic: HelpTopic::Tool,
            error: None,
            focus: false,
            browse: None,
            choices: Vec::new(),
            texts: Texts::default(),
        }
    }
}

impl GeoDialog {
    fn choice(&self, id: u64) -> Option<&LayerChoice> {
        self.choices.iter().find(|c| c.id == id)
    }

    /// 主输入（列表第一项）——字段列表与默认输出名都取它的。
    fn primary(&self) -> Option<&LayerChoice> {
        self.inputs.first().and_then(|id| self.choice(*id))
    }

    fn fields(&self) -> Vec<String> {
        self.primary().map(|c| c.fields.clone()).unwrap_or_default()
    }

    /// 勾选过的字段里，仍然存在于当前输入图层上的那些（图层变了就自动失效）。
    fn checked_fields(&self) -> Vec<String> {
        let all = self.fields();
        self.checked
            .iter()
            .filter(|f| all.contains(f))
            .cloned()
            .collect()
    }

    /// 跟随输入图层刷新默认输出名（用户改过就不动）。
    fn sync_output(&mut self) {
        if self.output_edited {
            return;
        }
        self.output_path = match self.primary() {
            Some(choice) => default_output_path(self.op, &choice.name),
            None => String::new(),
        };
    }

    fn request(&self) -> Result<GeoMsg, &'static str> {
        if self.inputs.len() < self.op.min_inputs() {
            return Err("geo.error.fewInputs");
        }
        if self.inputs.iter().any(|id| self.choice(*id).is_none()) {
            return Err("buffer.error.noInput");
        }
        if self.output_path.trim().is_empty() {
            return Err("buffer.error.noOutput");
        }
        Ok(GeoMsg::Accept {
            input_ids: self.inputs.clone(),
            output_path: PathBuf::from(self.output_path.trim()),
        })
    }

    /// 把校验键翻成人话（`{0}` 换成该工具的最少输入数）。
    fn error_text(&self, key: &str) -> String {
        self.texts
            .error_text(key)
            .replace("{0}", &self.op.min_inputs().to_string())
    }
}

/// 后台任务的产物：结果图层 + 写出文件时的错误（写盘失败不该丢掉计算结果）。
struct GeoDone {
    layer: Layer,
    write_error: Option<String>,
}

/// 后台地理处理任务（主线程每帧非阻塞轮询）。
pub(super) struct GeoJob {
    rx: mpsc::Receiver<Result<GeoDone, String>>,
}

impl RustGisApp {
    /// 打开（或聚焦）地理处理窗口。[`OverlayOp`] 决定初始工具。
    pub(super) fn open_geoprocess_dialog(&mut self, op: OverlayOp) {
        if self.geo_job.is_some() {
            self.status = Status::Message(self.t("buffer.error.busy").to_string());
            return;
        }
        // 窗口已开着：就地切换工具（ArcGIS 里同一工具箱的工具是并列的按钮，
        // 这里用一个下拉，省得关掉窗口重开）。
        if let Some(window) = self.geo_dialog.as_ref() {
            if let Ok(mut guard) = window.state.lock() {
                let d = &mut *guard;
                if d.op != op {
                    d.op = op;
                    d.checked.clear();
                    d.output_edited = false;
                    d.sync_output();
                    d.topic = HelpTopic::Tool;
                }
                d.error = None;
                d.focus = true;
            }
            return;
        }

        let choices = self.geo_layer_choices();
        let mut dialog = GeoDialog {
            op,
            choices,
            ..GeoDialog::default()
        };
        // 预选：融合只要一个输入，其余工具至少两个。
        let want = op.min_inputs();
        dialog.inputs = dialog
            .choices
            .iter()
            .take(want)
            .map(|choice| choice.id)
            .collect();
        dialog.texts = Texts::new(self);
        dialog.sync_output();
        dialog.focus = true;
        self.geo_dialog = Some(GeoWindow {
            id: ViewportId::from_hash_of("rustgis_geoprocess_dialog"),
            state: Arc::new(Mutex::new(dialog)),
            msgs: Arc::new(Mutex::new(Vec::new())),
        });
    }

    /// 窗口里能选的图层（只列真正的图层，不含分组）。
    fn geo_layer_choices(&self) -> Vec<LayerChoice> {
        self.layers
            .panel_rows()
            .iter()
            .filter_map(|row| match row.node {
                LayerNode::Layer(layer) => Some(LayerChoice {
                    id: layer.id,
                    name: layer.name.clone(),
                    fields: layer.fields.clone(),
                }),
                LayerNode::Group(_) | LayerNode::Raster(_) => None,
            })
            .collect()
    }

    /// 每帧驱动独立窗口：同步状态 → 建窗口 → 取回动作。
    pub(super) fn show_geoprocess_dialog(&mut self, ctx: &Context) {
        let Some(window) = self.geo_dialog.as_ref() else {
            return;
        };
        let id = window.id;
        let state = Arc::clone(&window.state);
        let msgs = Arc::clone(&window.msgs);

        // 只有主线程才知道的东西：最新图层表 + 当前语言文案。
        let op_label = {
            let mut d = state.lock().unwrap_or_else(|e| e.into_inner());
            d.choices = self.geo_layer_choices();
            d.texts = Texts::new(self);
            let fields = d.fields();
            d.checked.retain(|f| fields.contains(f));
            d.texts.op_name(d.op).to_string()
        };

        let builder = ViewportBuilder::default()
            .with_title(self.tf("geo.windowTitle", &[&op_label]))
            .with_inner_size([WINDOW_WIDTH, WINDOW_HEIGHT])
            .with_min_inner_size([MIN_WINDOW_WIDTH, MIN_WINDOW_HEIGHT])
            .with_resizable(true)
            .with_active(true);
        let window_msgs = Arc::clone(&msgs);
        ctx.show_viewport_immediate(id, builder, move |ctx, _class| {
            geoprocess_window_ui(ctx, &state, &window_msgs);
        });

        let mut outgoing = Vec::new();
        if let Ok(mut queue) = msgs.lock() {
            outgoing.append(&mut queue);
        }
        for msg in outgoing {
            match msg {
                GeoMsg::Accept {
                    input_ids,
                    output_path,
                } => {
                    self.start_geo_job(input_ids, output_path);
                    return;
                }
                GeoMsg::Cancel => {
                    self.geo_dialog = None;
                    return;
                }
                GeoMsg::AddLayer => self.pick_and_add_layer(),
                GeoMsg::Dev(key) => {
                    self.status = Status::Message(self.tf("status.dev", &[self.t(key)]));
                }
            }
        }
    }

    /// 起后台任务：克隆输入图层 + 参数快照，算完把结果图层投回来。
    fn start_geo_job(&mut self, input_ids: Vec<u64>, path: PathBuf) {
        let mut layers = Vec::with_capacity(input_ids.len());
        for id in &input_ids {
            match self.layers.node(*id).and_then(|node| node.layer()) {
                Some(layer) => layers.push(layer.clone()),
                None => {
                    self.geo_error("buffer.error.noInput");
                    return;
                }
            }
        }
        let (op, fields) = match self.geo_dialog.as_ref() {
            Some(window) => match window.state.lock() {
                Ok(d) => (
                    d.op,
                    if d.op.uses_fields() {
                        d.checked_fields()
                    } else {
                        Vec::new()
                    },
                ),
                Err(_) => return,
            },
            None => return,
        };

        // 先关窗口：结果图层加进来之后不让独立窗口继续挂在上面。
        self.geo_dialog = None;
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let refs: Vec<&Layer> = layers.iter().collect();
            let out = overlay::run(op, &refs, &fields).map(|layer| {
                let write_error = write_geojson(&layer, &path).err();
                GeoDone { layer, write_error }
            });
            let _ = tx.send(out);
        });
        self.geo_job = Some(GeoJob { rx });
        self.status = Status::Message(self.t("geo.status.running").to_string());
    }

    /// 每帧非阻塞检查后台任务（50ms 一次重绘，不卡 UI）。
    pub(super) fn poll_geo_job(&mut self, ctx: &Context) {
        let Some(job) = self.geo_job.take() else {
            return;
        };
        match job.rx.try_recv() {
            Ok(Ok(done)) => {
                let count = done.layer.feature_count.to_string();
                let fit = !self.view_covers(&done.layer.bounds);
                self.adopt_project_default(&done.layer);
                self.layers.add(done.layer);
                self.needs_fit = fit;
                self.dirty = true;
                self.status = Status::Message(self.tf("geo.status.done", &[&count]));
                if let Some(err) = done.write_error {
                    self.dialog = Some(Dialog::Error { body: err });
                }
            }
            Ok(Err(err)) => {
                self.status = Status::Message(self.tf("status.loadError", &[&err]));
                self.dialog = Some(Dialog::Error { body: err });
            }
            Err(TryRecvError::Empty) => {
                self.geo_job = Some(job);
                ctx.request_repaint_after(Duration::from_millis(50));
            }
            Err(TryRecvError::Disconnected) => {
                self.status = Status::Message(self.t("status.loadAborted").to_string());
                self.dialog = Some(Dialog::Error {
                    body: self.t("status.loadAborted").to_string(),
                });
            }
        }
    }

    fn geo_error(&mut self, key: &str) {
        let text = self.t(key).to_string();
        self.status = Status::Message(text.clone());
        self.dialog = Some(Dialog::Error { body: text });
    }
}

/// 独立窗口的界面（回调必须是 `Send + Sync`，所以只能拿到共享状态与邮箱）。
fn geoprocess_window_ui(ctx: &Context, state: &Mutex<GeoDialog>, msgs: &Mutex<Vec<GeoMsg>>) {
    let mut guard = state.lock().unwrap_or_else(|e| e.into_inner());
    let d = &mut *guard;
    let mut outgoing: Option<GeoMsg> = None;
    // 文案快照：本函数要可变借用 `d`，读文案不必每次都借 d。
    let texts = d.texts.clone();

    // 输出路径的保存对话框跑在后台线程上，这里只轮询结果。
    if let Some(rx) = d.browse.as_ref() {
        match rx.try_recv() {
            Ok(Some(path)) => {
                d.output_path = path.display().to_string();
                d.output_edited = true;
                d.browse = None;
            }
            Ok(None) | Err(TryRecvError::Disconnected) => d.browse = None,
            Err(TryRecvError::Empty) => ctx.request_repaint_after(Duration::from_millis(50)),
        }
    }

    if d.focus {
        d.focus = false;
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    }
    // 标题栏的关闭按钮 = 取消。
    if ctx.input(|i| i.viewport().close_requested()) {
        outgoing = Some(GeoMsg::Cancel);
    }

    let fields = d.fields();
    let hint = texts.order_hint(d.op).to_string();
    let dissolve_all = texts.dissolve_all.clone();

    egui::TopBottomPanel::bottom("geoprocess_dialog_buttons").show(ctx, |ui| {
        ui.add_space(6.0);
        if let Some(err) = d.error.as_deref() {
            ui.colored_label(style::DANGER, err);
        }
        ui.horizontal(|ui| {
            if ui.button(&texts.ok).clicked() {
                match d.request() {
                    Ok(msg) => {
                        d.error = None;
                        outgoing = Some(msg);
                    }
                    Err(key) => {
                        let text = d.error_text(key);
                        d.error = Some(text);
                    }
                }
            }
            if ui.button(&texts.cancel).clicked() {
                outgoing = Some(GeoMsg::Cancel);
            }
            if ui.button(&texts.environments).clicked() {
                outgoing = Some(GeoMsg::Dev("tools.options"));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button(&texts.tool_help).clicked() {
                    outgoing = Some(GeoMsg::Dev("help.docs"));
                }
                let toggle = if d.show_help {
                    &texts.hide_help
                } else {
                    &texts.show_help
                };
                if ui.button(toggle).clicked() {
                    d.show_help = !d.show_help;
                }
            });
        });
        ui.add_space(4.0);
    });

    egui::CentralPanel::default().show(ctx, |ui| {
        egui::ScrollArea::vertical()
            .id_salt("geoprocess_form")
            .show(ui, |ui| {
                ui.add_space(8.0);
                ui.horizontal_top(|ui| {
                    ui.vertical(|ui| {
                        ui.set_width(FORM_WIDTH);
                        tool_row(ui, d, &texts);
                        ui.add_space(10.0);
                        form_inputs(ui, d, &texts, &hint, &mut outgoing);
                        ui.add_space(10.0);
                        form_output(ui, d, &texts, ctx);
                        if d.op.uses_fields() {
                            ui.add_space(10.0);
                            form_fields(ui, d, &texts, &fields, &dissolve_all);
                        }
                    });
                    if d.show_help {
                        ui.separator();
                        ui.vertical(|ui| {
                            ui.set_width(HELP_WIDTH);
                            help_panel(ui, &texts, d.op, d.topic);
                        });
                    }
                });
                ui.add_space(8.0);
            });
    });

    drop(guard);
    if let Some(msg) = outgoing
        && let Ok(mut queue) = msgs.lock()
    {
        queue.push(msg);
    }
}

/// 参数区第一行：选择工具（换工具就地重算参数）。
///
/// 返回「标签」与「下拉」两个响应，测试据此断言下拉不会盖住左边的「工具」。
fn tool_row(
    ui: &mut egui::Ui,
    d: &mut GeoDialog,
    texts: &Texts,
) -> (egui::Response, egui::Response) {
    // 从左往右排：下拉只占「标签右侧的剩余宽度」。若按整行宽度（FORM_WIDTH）
    // 去画，再加上右对齐布局，下拉会从行首开始铺，正好把「工具」两个字压住。
    let (label, combo) = ui
        .horizontal(|ui| {
            let label = ui.label(&texts.tool);
            let width = ui.available_width();
            let before = d.op;
            let combo = combo(ui, "geoprocess_tool", &texts.tools, &mut d.op, width);
            if d.op != before {
                d.checked.clear();
                d.output_edited = false;
                d.sync_output();
                d.topic = HelpTopic::Tool;
                d.error = None;
            }
            (label, combo)
        })
        .inner;
    // 悬停标签或下拉都算「指着工具这一项」，右侧帮助跟着切到工具说明。
    if label.hovered() || combo.hovered() {
        d.topic = HelpTopic::Tool;
    }
    (label, combo)
}

/// 「输入要素」：勾选顺序即叠加顺序。
fn form_inputs(
    ui: &mut egui::Ui,
    d: &mut GeoDialog,
    texts: &Texts,
    hint: &str,
    outgoing: &mut Option<GeoMsg>,
) {
    ui.horizontal(|ui| {
        let resp = ui.label(&texts.input);
        if resp.hovered() {
            d.topic = HelpTopic::Inputs;
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if widgets::icon_button(ui, &texts.browse_layer, true, widgets::draw_group_icon)
                .clicked()
            {
                *outgoing = Some(GeoMsg::AddLayer);
            }
        });
    });

    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.set_width(FORM_WIDTH - 8.0);
        egui::ScrollArea::vertical()
            .id_salt("geoprocess_inputs")
            .max_height(INPUT_LIST_HEIGHT)
            .show(ui, |ui| {
                if d.choices.is_empty() {
                    ui.weak(&texts.no_layer);
                }
                // 已勾选：按叠加顺序显示，带序号。
                let ordered: Vec<(u64, String)> = d
                    .inputs
                    .iter()
                    .filter_map(|id| d.choice(*id).map(|c| (*id, c.name.clone())))
                    .collect();
                let mut remove: Option<u64> = None;
                for (index, (id, name)) in ordered.iter().enumerate() {
                    let mut on = true;
                    let label = format!("{}. {name}", index + 1);
                    if ui.checkbox(&mut on, label).changed() && !on {
                        remove = Some(*id);
                    }
                }
                if let Some(id) = remove {
                    d.inputs.retain(|x| *x != id);
                    d.checked.clear();
                    d.sync_output();
                }
                // 未勾选：点一下追加到末尾。
                let others: Vec<(u64, String)> = d
                    .choices
                    .iter()
                    .filter(|c| !d.inputs.contains(&c.id))
                    .map(|c| (c.id, c.name.clone()))
                    .collect();
                for (id, name) in others {
                    let mut on = false;
                    if ui.checkbox(&mut on, name).changed() && on {
                        d.inputs.push(id);
                        d.sync_output();
                    }
                }
            });
    });
    ui.add_space(2.0);
    ui.weak(hint);
}

/// 「输出要素类」：路径 + 另存为按钮。
fn form_output(ui: &mut egui::Ui, d: &mut GeoDialog, texts: &Texts, ctx: &Context) {
    ui.horizontal(|ui| {
        let resp = ui.label(&texts.output);
        if resp.hovered() {
            d.topic = HelpTopic::Output;
        }
    });
    ui.horizontal(|ui| {
        let edit = ui.add(
            TextEdit::singleline(&mut d.output_path)
                .desired_width(FORM_WIDTH - 28.0)
                .hint_text(&texts.default_output),
        );
        if edit.changed() {
            d.output_edited = true;
        }
        let can_browse = d.browse.is_none();
        if widgets::icon_button(
            ui,
            &texts.browse_output,
            can_browse,
            widgets::draw_group_icon,
        )
        .clicked()
        {
            let name = PathBuf::from(&d.output_path)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "output.geojson".to_string());
            let (tx, rx) = mpsc::channel();
            std::thread::spawn(move || {
                let picked = rfd::FileDialog::new()
                    .set_file_name(name)
                    .add_filter("GeoJSON", &["geojson"])
                    .save_file();
                let _ = tx.send(picked);
            });
            d.browse = Some(rx);
            ctx.request_repaint();
        }
    });
}

/// 「融合字段」：只有融合工具才有（对标 ArcGIS 的 Dissolve Fields 多选列表）。
fn form_fields(
    ui: &mut egui::Ui,
    d: &mut GeoDialog,
    texts: &Texts,
    fields: &[String],
    dissolve_all: &str,
) {
    ui.horizontal(|ui| {
        let resp = ui.label(&texts.fields);
        if resp.hovered() {
            d.topic = HelpTopic::Fields;
        }
    });
    if fields.is_empty() {
        ui.weak(&texts.no_field);
    }
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.set_width(FORM_WIDTH - 8.0);
        egui::ScrollArea::vertical()
            .id_salt("geoprocess_fields")
            .max_height(FIELDS_LIST_HEIGHT)
            .show(ui, |ui| {
                for name in fields {
                    let mut on = d.checked.contains(name);
                    if ui.checkbox(&mut on, name).changed() {
                        set_checked(&mut d.checked, name, on);
                    }
                }
            });
    });
    ui.horizontal(|ui| {
        if ui.button(&texts.select_all).clicked() {
            d.checked = fields.to_vec();
        }
        if ui.button(&texts.deselect_all).clicked() {
            d.checked.clear();
        }
        ui.add_enabled(false, egui::Button::new(&texts.add_field))
            .on_disabled_hover_text(&texts.add_field_tip);
    });
    ui.add_space(2.0);
    ui.weak(dissolve_all);
}

/// 右侧上下文帮助：鼠标悬停到哪个参数上就显示哪一项的说明。
fn help_panel(ui: &mut egui::Ui, texts: &Texts, op: OverlayOp, topic: HelpTopic) {
    ui.add_space(2.0);
    ui.label(RichText::new(texts.topic_title(topic, op)).strong());
    ui.add_space(4.0);
    let body = match topic {
        HelpTopic::Tool => texts.tool_help(op),
        HelpTopic::Inputs => texts.help_inputs.as_str(),
        HelpTopic::Output => texts.help_output.as_str(),
        HelpTopic::Fields => texts.help_fields.as_str(),
    };
    ui.label(body);
}

/// 每帧从 i18n 快照出来的界面文案（换语言立刻生效）。
#[derive(Clone, Default)]
struct Texts {
    tool: String,
    ok: String,
    cancel: String,
    environments: String,
    hide_help: String,
    show_help: String,
    tool_help: String,
    input: String,
    output: String,
    fields: String,
    no_field: String,
    select_all: String,
    deselect_all: String,
    add_field: String,
    add_field_tip: String,
    no_layer: String,
    browse_output: String,
    browse_layer: String,
    default_output: String,
    order_clip: String,
    order_multi: String,
    order_single: String,
    dissolve_all: String,
    help_inputs: String,
    help_output: String,
    help_fields: String,
    err_few_inputs: String,
    err_no_input: String,
    err_no_output: String,
    /// 工具下拉的选项（工具 → 名字）。
    tools: Vec<(OverlayOp, String)>,
    /// 每个工具的帮助正文。
    tool_bodies: Vec<(OverlayOp, String)>,
}

impl Texts {
    fn new(app: &RustGisApp) -> Self {
        Self {
            tool: app.t("geo.tool").to_string(),
            ok: app.t("dialog.ok").to_string(),
            cancel: app.t("dialog.cancel").to_string(),
            environments: app.t("buffer.environments").to_string(),
            hide_help: app.t("buffer.hideHelp").to_string(),
            show_help: app.t("buffer.showHelp").to_string(),
            tool_help: app.t("buffer.toolHelp").to_string(),
            input: app.t("geo.input").to_string(),
            output: app.t("geo.output").to_string(),
            fields: app.t("geo.fields").to_string(),
            no_field: app.t("buffer.noField").to_string(),
            select_all: app.t("buffer.selectAll").to_string(),
            deselect_all: app.t("buffer.deselectAll").to_string(),
            add_field: app.t("buffer.addField").to_string(),
            add_field_tip: app.t("buffer.addFieldTip").to_string(),
            no_layer: app.t("buffer.noLayer").to_string(),
            browse_output: app.t("buffer.browseOutput").to_string(),
            browse_layer: app.t("layers.addTip").to_string(),
            default_output: app.t("buffer.defaultOutput").to_string(),
            order_clip: app.t("geo.orderClip").to_string(),
            order_multi: app.t("geo.orderMulti").to_string(),
            order_single: app.t("geo.orderSingle").to_string(),
            dissolve_all: app.t("geo.dissolveAll").to_string(),
            help_inputs: app.t("geo.help.inputs").to_string(),
            help_output: app.t("geo.help.output").to_string(),
            help_fields: app.t("geo.help.fields").to_string(),
            err_few_inputs: app.t("geo.error.fewInputs").to_string(),
            err_no_input: app.t("buffer.error.noInput").to_string(),
            err_no_output: app.t("buffer.error.noOutput").to_string(),
            tools: OverlayOp::ALL
                .iter()
                .map(|op| (*op, app.t(op.i18n_key()).to_string()))
                .collect(),
            tool_bodies: OverlayOp::ALL
                .iter()
                .map(|op| (*op, app.t(op.help_key()).to_string()))
                .collect(),
        }
    }

    fn op_name(&self, op: OverlayOp) -> &str {
        label_of(&self.tools, op)
    }

    fn tool_help(&self, op: OverlayOp) -> &str {
        label_of(&self.tool_bodies, op)
    }

    fn topic_title(&self, topic: HelpTopic, op: OverlayOp) -> &str {
        match topic {
            HelpTopic::Tool => self.op_name(op),
            HelpTopic::Inputs => &self.input,
            HelpTopic::Output => &self.output,
            HelpTopic::Fields => &self.fields,
        }
    }

    /// 输入列表下面的提示：不同工具对输入个数 / 顺序的要求不一样。
    fn order_hint(&self, op: OverlayOp) -> &str {
        match op {
            OverlayOp::Clip => &self.order_clip,
            OverlayOp::Dissolve => &self.order_single,
            _ => &self.order_multi,
        }
    }

    /// 校验键 → 文案；`{0}` 由 [`GeoDialog::error_text`] 填上该工具的最少输入数。
    fn error_text(&self, key: &str) -> String {
        match key {
            "geo.error.fewInputs" => self.err_few_inputs.clone(),
            "buffer.error.noInput" => self.err_no_input.clone(),
            _ => self.err_no_output.clone(),
        }
    }
}

/// 在标签表里找值对应的文案（找不到就退回第一个）。
fn label_of<T: PartialEq + Copy>(labels: &[(T, String)], value: T) -> &str {
    labels
        .iter()
        .find(|(key, _)| *key == value)
        .map(|(_, label)| label.as_str())
        .unwrap_or("")
}

/// 下拉框：`width` 是按钮宽度，与「缓冲区」对话框同一套样式。
fn combo<T: PartialEq + Copy>(
    ui: &mut egui::Ui,
    id: &str,
    labels: &[(T, String)],
    value: &mut T,
    width: f32,
) -> egui::Response {
    let selected = label_of(labels, *value).to_string();
    egui::ComboBox::from_id_salt(id)
        .selected_text(selected)
        .width(width)
        .show_ui(ui, |ui| {
            for (key, label) in labels {
                ui.selectable_value(value, *key, label);
            }
        })
        .response
}

/// 勾选 / 取消勾选一个字段（`Vec<String>` 去重）。
fn set_checked(list: &mut Vec<String>, name: &str, on: bool) {
    list.retain(|item| item != name);
    if on {
        list.push(name.to_string());
    }
}

/// 默认输出路径：`{输入名}{后缀}.geojson`。
fn default_output_path(op: OverlayOp, layer_name: &str) -> String {
    format!("{}.geojson", op.default_output_name(layer_name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geo::{Coord2D, PolygonData};
    use std::path::Path;

    /// 一个 100x100 的正方形面图层（带一个属性字段）。
    fn square_layer(name: &str, x: f64, y: f64) -> Layer {
        let size = 100.0;
        let ring: Vec<Coord2D> = vec![
            [x, y],
            [x + size, y],
            [x + size, y + size],
            [x, y + size],
            [x, y],
        ];
        let polygon = PolygonData {
            outer: ring,
            holes: Vec::new(),
        };
        let mut layer =
            Layer::from_geometry(name.to_string(), Vec::new(), Vec::new(), vec![polygon]);
        layer.fields = vec!["Id".to_string()];
        layer.attrs = vec![vec!["1".to_string()]];
        layer.ring_rows = vec![0];
        layer
    }

    fn frame(app: &mut RustGisApp, ctx: &Context) {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1280.0, 900.0),
            )),
            ..Default::default()
        };
        let _ = ctx.run(input, |ctx| app.show_geoprocess_dialog(ctx));
    }

    fn temp_out(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rustgis_geo_{tag}_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir.join(format!("{tag}.geojson"))
    }

    #[test]
    fn geoprocessing_menu_items_reach_the_dialog() {
        use crate::app::MenuAction;
        use crate::menus::{Entry, MENUS};

        // 「地理处理」菜单里列出的 key 必须都能被 on_menu_item 认领：
        // menus.rs 与 app/mod.rs 是两处字符串，靠这个测试保证不会走偏。
        let items: Vec<&str> = MENUS
            .iter()
            .flat_map(|menu| menu.entries.iter())
            .filter_map(|entry| match entry {
                Entry::Item(key) => Some(*key),
                _ => None,
            })
            .collect();
        assert!(MENUS.iter().any(|menu| menu.label == "menu.geoprocessing"));
        assert!(items.contains(&"overlay.buffer"), "{items:?}");
        for op in OverlayOp::ALL {
            assert!(
                items.contains(&op.menu_key()),
                "菜单缺 {}: {items:?}",
                op.menu_key()
            );
        }

        // 逐个点击：缓冲区复用缓冲区窗口，其余 5 个切换地理处理窗口的工具。
        let ctx = Context::default();
        let mut app = RustGisApp::new(&ctx);
        app.layers.add(square_layer("a", 0.0, 0.0));
        app.layers.add(square_layer("b", 50.0, 50.0));

        assert!(matches!(
            app.on_menu_item("overlay.buffer"),
            MenuAction::None
        ));
        assert!(app.buffer_dialog.is_some(), "缓冲区菜单项应打开缓冲区窗口");
        assert!(app.geo_dialog.is_none());

        for op in OverlayOp::ALL {
            assert!(matches!(app.on_menu_item(op.menu_key()), MenuAction::None));
            let window = app.geo_dialog.as_ref().expect("地理处理窗口");
            let d = window.state.lock().expect("lock");
            assert_eq!(d.op, op, "{} 应切换到对应的工具", op.menu_key());
            assert!(
                app.buffer_dialog.is_some(),
                "两个窗口各自独立，不应互相关闭"
            );
        }
    }

    #[test]
    fn tool_row_combo_does_not_cover_the_tool_label() {
        let ctx = Context::default();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(800.0, 600.0),
            )),
            ..Default::default()
        };
        let mut dialog = GeoDialog::default();
        let mut rects = None;
        let _ = ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.set_width(FORM_WIDTH);
                let texts = Texts {
                    tool: "工具".to_string(),
                    tools: vec![(OverlayOp::Clip, "裁剪".to_string())],
                    ..Texts::default()
                };
                let (label, combo) = tool_row(ui, &mut dialog, &texts);
                rects = Some((label.rect, combo.rect));
            });
        });
        let (label, combo) = rects.expect("工具行应被布局");
        assert!(
            combo.left() >= label.right(),
            "下拉压住了「工具」标签：label={label:?} combo={combo:?}"
        );
        assert!(
            combo.width() > FORM_WIDTH / 2.0,
            "下拉应占满标签右侧的剩余宽度：{combo:?}"
        );
    }

    #[test]
    fn request_reports_missing_pieces() {
        let mut dialog = GeoDialog {
            op: OverlayOp::Clip,
            ..GeoDialog::default()
        };
        assert_eq!(dialog.request(), Err("geo.error.fewInputs"));

        dialog.inputs = vec![1, 2];
        dialog.choices = vec![
            LayerChoice {
                id: 1,
                name: "a".to_string(),
                fields: Vec::new(),
            },
            LayerChoice {
                id: 2,
                name: "b".to_string(),
                fields: Vec::new(),
            },
        ];
        assert_eq!(dialog.request(), Err("buffer.error.noOutput"));

        dialog.output_path = "  out.geojson  ".to_string();
        assert!(matches!(
            dialog.request(),
            Ok(GeoMsg::Accept { input_ids, .. }) if input_ids == vec![1, 2]
        ));

        // 图层被删掉之后必须报「没有输入」，而不是静默跑一遍。
        dialog.choices.retain(|c| c.id != 2);
        assert_eq!(dialog.request(), Err("buffer.error.noInput"));
    }

    #[test]
    fn dissolve_only_needs_one_input_and_syncs_output_name() {
        let mut dialog = GeoDialog {
            op: OverlayOp::Dissolve,
            choices: vec![LayerChoice {
                id: 7,
                name: "zones".to_string(),
                fields: vec!["Kind".to_string()],
            }],
            ..GeoDialog::default()
        };
        dialog.inputs = vec![7];
        dialog.sync_output();
        assert_eq!(dialog.output_path, "zones_Dissolve.geojson");
        assert!(matches!(dialog.request(), Ok(GeoMsg::Accept { .. })));
    }

    #[test]
    fn open_switch_tool_and_cancel() {
        let ctx = Context::default();
        let mut app = RustGisApp::new(&ctx);
        app.layers.add(square_layer("a", 0.0, 0.0));
        app.layers.add(square_layer("b", 50.0, 50.0));

        app.open_geoprocess_dialog(OverlayOp::Clip);
        {
            let window = app.geo_dialog.as_ref().expect("window");
            let d = window.state.lock().expect("lock");
            // 预选前两个输入，并按主输入命名输出。
            // 注意：图层树把最新添加的图层排在最上面（ArcGIS 风格），
            // 所以面板前两行是 b、a —— 主输入就是 b。
            assert_eq!(d.inputs.len(), 2);
            assert_eq!(d.output_path, "b_Clip.geojson");
            assert!(d.show_help);
        }
        // 再点同一个工具只聚焦，不重开窗口。
        frame(&mut app, &ctx);
        app.open_geoprocess_dialog(OverlayOp::Union);
        {
            let window = app.geo_dialog.as_ref().expect("window");
            let d = window.state.lock().expect("lock");
            assert_eq!(d.op, OverlayOp::Union);
            assert_eq!(d.output_path, "b_Union.geojson");
        }

        let msgs = Arc::clone(&app.geo_dialog.as_ref().expect("window").msgs);
        msgs.lock().expect("lock").push(GeoMsg::Cancel);
        frame(&mut app, &ctx);
        assert!(app.geo_dialog.is_none());
    }

    #[test]
    fn accept_runs_overlay_and_adds_result_layer() {
        let ctx = Context::default();
        let mut app = RustGisApp::new(&ctx);
        app.layers.add(square_layer("a", 0.0, 0.0));
        app.layers.add(square_layer("b", 50.0, 50.0));

        let out = temp_out("clip");
        let _ = std::fs::remove_file(&out);
        app.open_geoprocess_dialog(OverlayOp::Clip);
        let msgs = Arc::clone(&app.geo_dialog.as_ref().expect("window").msgs);
        msgs.lock().expect("lock").push(GeoMsg::Accept {
            input_ids: vec![
                app.layers.panel_rows()[0].node.id(),
                app.layers.panel_rows()[1].node.id(),
            ],
            output_path: out.clone(),
        });
        frame(&mut app, &ctx);
        assert!(app.geo_dialog.is_none());
        assert!(app.geo_job.is_some());

        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while app.geo_job.is_some() && std::time::Instant::now() < deadline {
            app.poll_geo_job(&ctx);
            std::thread::sleep(Duration::from_millis(10));
        }
        app.poll_geo_job(&ctx);
        assert!(app.geo_job.is_none(), "后台任务应在超时前完成");

        assert!(Path::new(&out).exists(), "结果应写出 GeoJSON");
        let names: Vec<String> = app
            .layers
            .panel_rows()
            .iter()
            .filter_map(|row| match row.node {
                LayerNode::Layer(layer) => Some(layer.name.clone()),
                LayerNode::Group(_) | LayerNode::Raster(_) => None,
            })
            .collect();
        // 主输入是图层树最上面的 b（最新添加），因此结果图层名为 b_Clip。
        assert!(names.contains(&"b_Clip".to_string()), "{names:?}");
        let _ = std::fs::remove_file(&out);
    }
}
