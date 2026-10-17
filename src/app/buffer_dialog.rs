//! 缓冲区分析对话框（对标 ArcGIS「缓冲区」工具）+ 后台缓冲流水线。
//!
//! 对话框跑在**独立的系统窗口**里：用 egui 的多视口（viewport）机制创建，
//! eframe 的原生集成会为它开一个真正的窗口（有自己的标题栏、可移动、可缩放），
//! 而不是画在主窗口内部。代价是窗口的 UI 回调必须是 `Send + Sync + 'static`，
//! 不能借用 [`RustGisApp`]，所以这里分成两半：
//!
//! - **界面状态**放在 `Arc<Mutex<BufferDialog>>` 里。主线程每帧把「只有它才知道」
//!   的东西同步进去（图层列表 + 当前语言的界面文案），因此切换语言、导入新图层
//!   都会立刻反映到独立窗口上；
//! - **窗口不直接改应用状态**：所有动作（确定 / 取消 / 从文件添加图层 / 占位入口）
//!   以 [`BufferMsg`] 投进邮箱，主线程下一帧取出执行。
//!
//! 窗口里仍然是左右两栏：左侧参数表单，右侧**上下文帮助**——鼠标悬停到哪个
//! 参数上，右侧就显示该项的说明（ArcGIS 里点「工具帮助」才出现的面板，这里常驻
//! 并在悬停时切换）。确定后把参数快照交给后台线程（复用 [`super::import`] 的
//! 「通道 + 每帧 50ms 非阻塞轮询」模式，UI 线程全程不阻塞）：后台做缓冲 + 融合 +
//! 三角剖分，写出 GeoJSON，主线程收到结果后把图层加入地图并按需自适应视图。

use std::path::PathBuf;
use std::sync::mpsc::{self, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use egui::{Context, RichText, TextEdit, ViewportBuilder, ViewportId};

use crate::dialog::Dialog;
use crate::geo::buffer::{
    self, BufferParams, DissolveType, DistanceSource, EndType, LinearUnit, SideType,
};
use crate::geo::{Aabb, Layer, LayerNode};
use crate::map::Projector;
use crate::style;

use super::widgets;
use super::{RustGisApp, Status};

/// 表单列宽（ArcGIS 对话框左栏）。
const FORM_WIDTH: f32 = 440.0;
/// 帮助列宽（ArcGIS 对话框右栏）。
const HELP_WIDTH: f32 = 260.0;
/// 融合字段列表的最大高度（超出滚动）。
const DISSOLVE_LIST_HEIGHT: f32 = 132.0;
/// 独立窗口的初始尺寸（表单 + 帮助 + 边距）。
const WINDOW_WIDTH: f32 = FORM_WIDTH + HELP_WIDTH + 48.0;
const WINDOW_HEIGHT: f32 = 560.0;
/// 独立窗口的最小尺寸（缩小后表单改为滚动）。
const MIN_WINDOW_WIDTH: f32 = 520.0;
const MIN_WINDOW_HEIGHT: f32 = 400.0;

/// 右侧帮助面板当前展示的参数。
#[derive(Clone, Copy, PartialEq, Eq)]
enum HelpTopic {
    Input,
    Output,
    Distance,
    Side,
    End,
    Dissolve,
    DissolveFields,
}

/// 对话框中可选的输入图层（含属性字段名，供「字段」与「融合字段」使用）。
struct LayerChoice {
    id: u64,
    name: String,
    fields: Vec<String>,
}

/// 独立窗口发给主线程的动作。
pub(super) enum BufferMsg {
    /// 确定：表单已通过校验。
    Accept {
        input_id: u64,
        params: BufferParams,
        output_path: PathBuf,
    },
    /// 取消（「取消」按钮或窗口标题栏的关闭按钮）。
    Cancel,
    /// 「输入要素」右侧的文件夹按钮：从文件添加图层（等价菜单「添加数据」）。
    AddLayer,
    /// 尚未实现的入口（「环境...」/「工具帮助」），沿用统一占位提示。
    Dev(&'static str),
}

/// 独立的「缓冲区」窗口：界面状态 + 与主线程通信的邮箱。
pub(super) struct BufferWindow {
    /// 视口 id：必须每帧用同一个 id 调 `show_viewport_deferred`，窗口才会持续存在。
    id: ViewportId,
    /// 界面状态（窗口回调与主线程共享）。
    state: Arc<Mutex<BufferDialog>>,
    /// 窗口 → 主线程的动作邮箱。
    msgs: Arc<Mutex<Vec<BufferMsg>>>,
}

/// 「缓冲区」窗口的全部界面状态。
struct BufferDialog {
    /// 选中的输入图层 id。
    input_id: Option<u64>,
    /// 输出要素类路径（GeoJSON）。
    output_path: String,
    /// 距离来源：`false` = 线性单位数值，`true` = 输入要素字段。
    use_field: bool,
    /// 距离数值的原始文本（保留用户输入，确定时才解析）。
    value_text: String,
    unit: LinearUnit,
    field: Option<String>,
    side: SideType,
    end: EndType,
    dissolve: DissolveType,
    /// 已勾选的融合字段。
    checked: Vec<String>,
    show_help: bool,
    topic: HelpTopic,
    /// 校验失败的原因（显示在窗口底部，直到下次点确定）。
    error: Option<String>,
    /// 刚打开：下一帧把键盘焦点抢到独立窗口上。
    focus: bool,
    /// 「输出要素类」文件夹按钮弹出的系统保存对话框（后台线程，不阻塞 UI）。
    browse: Option<mpsc::Receiver<Option<PathBuf>>>,
    /// 主线程每帧刷新：可选输入图层。
    choices: Vec<LayerChoice>,
    /// 主线程每帧刷新：当前语言的界面文案。
    texts: Texts,
}

impl Default for BufferDialog {
    fn default() -> Self {
        Self {
            input_id: None,
            output_path: String::new(),
            use_field: false,
            // 与 ArcGIS 默认值一致：500 米 / FULL / ROUND / NONE。
            value_text: "500".to_string(),
            unit: LinearUnit::Meters,
            field: None,
            side: SideType::Full,
            end: EndType::Round,
            dissolve: DissolveType::None,
            checked: Vec::new(),
            show_help: true,
            topic: HelpTopic::Input,
            error: None,
            focus: false,
            browse: None,
            choices: Vec::new(),
            texts: Texts::default(),
        }
    }
}

impl BufferDialog {
    /// 当前输入图层的属性字段（未选中图层时为空）。
    fn fields(&self) -> Vec<String> {
        self.choices
            .iter()
            .find(|c| Some(c.id) == self.input_id)
            .map(|c| c.fields.clone())
            .unwrap_or_default()
    }

    /// 表单 → 后台缓冲参数；校验失败时返回 i18n key（窗口里就地提示，不丢输入）。
    fn request(&self) -> Result<BufferMsg, &'static str> {
        let Some(input_id) = self.input_id else {
            return Err("buffer.error.noInput");
        };

        // 距离：线性单位数值，或输入要素的字段。
        let distance = if self.use_field {
            match self.field.as_deref() {
                Some(f) if !f.is_empty() => DistanceSource::Field(f.to_string()),
                _ => return Err("buffer.error.badDistance"),
            }
        } else {
            // 非零即可：负距离是「侵蚀」（ArcGIS 同样接受负值，面会向内缩）。
            match self.value_text.trim().parse::<f64>() {
                Ok(v) if v.is_finite() && v != 0.0 => DistanceSource::Value(v),
                _ => return Err("buffer.error.badDistance"),
            }
        };

        // LIST 融合必须至少勾选一个字段（否则所有要素同键，等价于 ALL）。
        if self.dissolve == DissolveType::List && self.checked.is_empty() {
            return Err("buffer.error.noDissolveField");
        }

        let output = self.output_path.trim();
        if output.is_empty() {
            return Err("buffer.error.noOutput");
        }

        Ok(BufferMsg::Accept {
            input_id,
            params: BufferParams {
                distance,
                unit: self.unit,
                side: self.side,
                end: self.end,
                dissolve: self.dissolve,
                dissolve_fields: self.checked.clone(),
            },
            output_path: PathBuf::from(output),
        })
    }
}

/// 后台缓冲任务的结果：成功图层 + 可选的「写文件失败」原因。
struct BufferDone {
    layer: Layer,
    write_error: Option<String>,
}

/// 后台缓冲流水线（确定之后、加入图层之前）。
pub(super) struct BufferJob {
    rx: mpsc::Receiver<Result<BufferDone, String>>,
}

impl RustGisApp {
    /// 打开「缓冲区」对话框（分析菜单 → 缓冲区）：新建独立窗口，已打开则抢焦点。
    pub(super) fn open_buffer_dialog(&mut self) {
        if self.buffer_job.is_some() {
            // 上一次分析还在跑：提示并忽略，避免两个任务抢同一份状态。
            let msg = self.t("buffer.error.busy").to_string();
            self.status = Status::Message(msg);
            return;
        }
        if let Some(window) = self.buffer_dialog.as_ref() {
            // 已经开着：不重复开窗，只把焦点抢回来（窗口可能被主窗口压住了）。
            let mut d = window.state.lock().unwrap_or_else(|e| e.into_inner());
            d.focus = true;
            return;
        }

        // 默认选中第一个图层；输出路径按 ArcGIS 习惯预填 `{图层名}_Buffer.geojson`。
        let mut dialog = BufferDialog::default();
        let choices = self.layer_choices();
        if let Some(first) = choices.first() {
            dialog.input_id = Some(first.id);
            dialog.output_path = default_output_path(&first.name);
        }
        dialog.choices = choices;
        dialog.texts = Texts::new(self);
        dialog.focus = true;

        self.buffer_dialog = Some(BufferWindow {
            id: ViewportId::from_hash_of("rustgis_buffer_dialog"),
            state: Arc::new(Mutex::new(dialog)),
            msgs: Arc::new(Mutex::new(Vec::new())),
        });
    }

    /// 图层下拉框的可选列表（内容列表顺序，只含真正的图层节点）。
    fn layer_choices(&self) -> Vec<LayerChoice> {
        self.layers
            .panel_rows()
            .iter()
            .filter_map(|row| match row.node {
                LayerNode::Layer(l) => Some(LayerChoice {
                    id: l.id,
                    name: l.name.clone(),
                    fields: l.fields.clone(),
                }),
                _ => None,
            })
            .collect()
    }

    /// 推进「缓冲区」独立窗口（每帧一次；未打开时什么都不做）。
    ///
    /// 每帧重新登记一次视口 ⇒ 窗口持续存在；停止登记（`buffer_dialog` 变 `None`）
    /// 后 eframe 会销毁它。
    pub(super) fn show_buffer_dialog(&mut self, ctx: &Context) {
        // 只借用窗口一小会儿：后面改 `self`（关窗 / 起任务）时不能还挂着它的借用。
        let (id, state, msgs) = match self.buffer_dialog.as_ref() {
            Some(window) => (
                window.id,
                Arc::clone(&window.state),
                Arc::clone(&window.msgs),
            ),
            None => return,
        };

        // 主线程 → 窗口：图层列表 + 界面文案（语言切换 / 新图层 / 属性变化即时可见）。
        {
            let choices = self.layer_choices();
            let texts = Texts::new(self);
            let mut d = state.lock().unwrap_or_else(|e| e.into_inner());
            d.choices = choices;
            d.texts = texts;
        }

        let builder = ViewportBuilder::default()
            .with_title(self.t("buffer.title"))
            .with_inner_size([WINDOW_WIDTH, WINDOW_HEIGHT])
            .with_min_inner_size([MIN_WINDOW_WIDTH, MIN_WINDOW_HEIGHT])
            .with_resizable(true)
            .with_active(true);
        // 邮箱再克隆一份给闭包：本帧的窗口回调与稍后的取消息各用一份。
        let window_msgs = Arc::clone(&msgs);
        ctx.show_viewport_deferred(id, builder, move |ctx, _class| {
            buffer_window_ui(ctx, &state, &window_msgs);
        });

        // 窗口 → 主线程：取出本帧的动作。
        let pending: Vec<BufferMsg> = {
            let mut queue = msgs.lock().unwrap_or_else(|e| e.into_inner());
            std::mem::take(&mut *queue)
        };
        for msg in pending {
            match msg {
                BufferMsg::Accept {
                    input_id,
                    params,
                    output_path,
                } => {
                    self.start_buffer_job(input_id, params, output_path);
                    return;
                }
                BufferMsg::Cancel => {
                    // 不再登记视口 ⇒ 窗口下一帧被销毁，状态一并丢弃（下次打开回到默认值）。
                    self.buffer_dialog = None;
                    return;
                }
                BufferMsg::AddLayer => self.pick_and_add_layer(),
                BufferMsg::Dev(key) => {
                    let feature = self.t(key).to_string();
                    self.status = Status::Message(self.tf("status.dev", &[&feature]));
                }
            }
        }
    }

    /// 确定：快照参数 → 后台线程执行（表单校验已在窗口里完成）。
    fn start_buffer_job(&mut self, input_id: u64, params: BufferParams, path: PathBuf) {
        // 输入图层（克隆一份给后台线程：缓冲区结果图层必须独立存在）。
        let layer = self
            .layers
            .node(input_id)
            .and_then(|n| n.layer())
            .cloned();
        let Some(layer) = layer else {
            // 图层在点确定前被删掉了：保留窗口内容，只报错。
            self.buffer_error("buffer.error.noInput");
            return;
        };

        // 关掉窗口：任务跑起来后不该再让参数被编辑（ArcGIS 同样如此）。
        self.buffer_dialog = None;

        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            // 缓冲 + 融合 + 三角剖分全在后台线程；写 GeoJSON 失败不算分析失败，
            // 结果图层照样加入地图，只把原因带回主线程提示。
            let out = buffer::run(&layer, &params).map(|layer| {
                let write_error = buffer::write_geojson(&layer, &path).err();
                BufferDone { layer, write_error }
            });
            let _ = tx.send(out);
        });

        self.buffer_job = Some(BufferJob { rx });
        self.status = Status::Message(self.t("buffer.status.running").to_string());
    }

    /// 后台缓冲流水线推进（每帧一次，非阻塞）。
    pub(super) fn poll_buffer_job(&mut self, ctx: &Context) {
        let Some(job) = self.buffer_job.take() else {
            return;
        };
        match job.rx.try_recv() {
            Ok(Ok(done)) => {
                let count = done.layer.feature_count.to_string();
                // 结果范围不在当前视图内时才重新适配（结果通常紧邻输入图层，
                // 用户刚看过输入图层，无谓的缩放反而会打断操作）。
                let fit = !self.view_covers(&done.layer.bounds);
                self.adopt_project_default(&done.layer);
                self.layers.add(done.layer);
                self.needs_fit = fit;
                self.dirty = true;
                self.status = Status::Message(self.tf("buffer.status.done", &[&count]));
                if let Some(e) = done.write_error {
                    // 分析成功但落盘失败：结果已在地图上，仍如实告知用户。
                    self.dialog = Some(Dialog::Error { body: e });
                }
            }
            Ok(Err(e)) => {
                self.status = Status::Message(self.tf("status.loadError", &[&e]));
                self.dialog = Some(Dialog::Error { body: e });
            }
            Err(TryRecvError::Empty) => {
                self.buffer_job = Some(job);
                // 分析中持续唤醒：否则结果要等到下一次用户操作才被取回。
                ctx.request_repaint_after(Duration::from_millis(50));
            }
            Err(TryRecvError::Disconnected) => {
                let msg = self.t("status.loadAborted").to_string();
                self.status = Status::Message(msg.clone());
                self.dialog = Some(Dialog::Error { body: msg });
            }
        }
    }

    /// 当前地图视图是否已完整包含给定范围（无视图 / 无地图区域时视为不含）。
    ///
    /// 缓冲区与地理处理（[`super::geoprocess_dialog`]）两处结果落点判断共用。
    pub(super) fn view_covers(&self, bounds: &Aabb) -> bool {
        match (self.map.view, self.map_rect) {
            (Some(view), Some(rect)) => Projector::new(rect, &view)
                .visible_world()
                .contains_aabb(bounds),
            _ => false,
        }
    }

    /// 参数校验失败：状态栏 + 错误对话框。
    fn buffer_error(&mut self, key: &str) {
        let msg = self.t(key).to_string();
        self.status = Status::Message(msg.clone());
        self.dialog = Some(Dialog::Error { body: msg });
    }
}

/// 独立窗口的 UI 回调（eframe 每帧为这个视口调用一次）。
///
/// 只读/写共享的界面状态，产生的动作全部投进邮箱，由主线程执行。
fn buffer_window_ui(ctx: &Context, state: &Mutex<BufferDialog>, msgs: &Mutex<Vec<BufferMsg>>) {
    let mut guard = state.lock().unwrap_or_else(|e| e.into_inner());
    let d = &mut *guard;
    let mut outgoing: Option<BufferMsg> = None;

    // 「输出要素类」的系统保存对话框是后台线程弹的：这里非阻塞取回结果。
    if let Some(rx) = d.browse.take() {
        match rx.try_recv() {
            Ok(Some(path)) => d.output_path = path.display().to_string(),
            Ok(None) => {}
            Err(TryRecvError::Empty) => d.browse = Some(rx),
            Err(TryRecvError::Disconnected) => {}
        }
        if d.browse.is_some() {
            ctx.request_repaint_after(Duration::from_millis(50));
        }
    }

    // 刚由菜单打开：把键盘焦点抢到独立窗口上（否则用户得先点一下窗口）。
    if d.focus {
        d.focus = false;
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    }

    // 窗口标题栏的关闭按钮（X）等价于「取消」。
    if ctx.input(|i| i.viewport().close_requested()) {
        outgoing = Some(BufferMsg::Cancel);
    }

    // 每帧重算：切图层后字段列表要跟着变。
    let fields = d.fields();

    // ── 底部按钮行（固定在窗口底部，不随表单滚动）──
    egui::TopBottomPanel::bottom("buffer_dialog_buttons").show(ctx, |ui| {
        ui.add_space(4.0);
        if let Some(err) = &d.error {
            ui.colored_label(style::DANGER, err);
            ui.add_space(2.0);
        }
        ui.horizontal(|ui| {
            if ui.button(&d.texts.ok).clicked() {
                match d.request() {
                    Ok(msg) => outgoing = Some(msg),
                    Err(key) => {
                        // 校验失败：原地提示，窗口与输入都保留。
                        d.error = Some(d.texts.error_text(key));
                    }
                }
            }
            if ui.button(&d.texts.cancel).clicked() {
                outgoing = Some(BufferMsg::Cancel);
            }
            if ui.button(&d.texts.environments).clicked() {
                outgoing = Some(BufferMsg::Dev("tools.options"));
            }
            let toggle = if d.show_help {
                &d.texts.hide_help
            } else {
                &d.texts.show_help
            };
            if ui.button(toggle).clicked() {
                d.show_help = !d.show_help;
            }
            if ui.button(&d.texts.tool_help).clicked() {
                outgoing = Some(BufferMsg::Dev("help.docs"));
            }
        });
        ui.add_space(4.0);
    });

    // ── 表单 + 上下文帮助 ──
    egui::CentralPanel::default().show(ctx, |ui| {
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.horizontal_top(|ui| {
                // 左栏：参数表单
                ui.vertical(|ui| {
                    ui.set_width(FORM_WIDTH);
                    ui.add_space(2.0);

                    // 输入要素
                    if ui.label(&d.texts.input).hovered() {
                        d.topic = HelpTopic::Input;
                    }
                    ui.horizontal(|ui| {
                        let selected = d
                            .choices
                            .iter()
                            .find(|c| Some(c.id) == d.input_id)
                            .map(|c| c.name.as_str())
                            .unwrap_or(d.texts.no_layer.as_str());
                        egui::ComboBox::from_id_salt("buffer_input")
                            .selected_text(selected)
                            .width(FORM_WIDTH - 28.0)
                            .show_ui(ui, |ui| {
                                for c in &d.choices {
                                    ui.selectable_value(
                                        &mut d.input_id,
                                        Some(c.id),
                                        c.name.as_str(),
                                    );
                                }
                            });
                        // 文件夹按钮：直接从文件添加图层（与菜单「添加数据」等价）。
                        if widgets::icon_button(
                            ui,
                            d.texts.browse_layer.as_str(),
                            true,
                            widgets::draw_group_icon,
                        )
                        .clicked()
                        {
                            outgoing = Some(BufferMsg::AddLayer);
                        }
                    });
                    ui.add_space(6.0);

                    // 输出要素类
                    if ui.label(&d.texts.output).hovered() {
                        d.topic = HelpTopic::Output;
                    }
                    ui.horizontal(|ui| {
                        ui.add(
                            TextEdit::singleline(&mut d.output_path)
                                .desired_width(FORM_WIDTH - 28.0),
                        );
                        if widgets::icon_button(
                            ui,
                            d.texts.browse_output.as_str(),
                            true,
                            widgets::draw_group_icon,
                        )
                        .clicked()
                            && d.browse.is_none()
                        {
                            // 系统文件对话框会阻塞调用线程，因此丢到后台线程里弹。
                            let name = d
                                .output_path
                                .rsplit(['/', '\\'])
                                .next()
                                .filter(|s| !s.is_empty())
                                .map(|s| s.to_string())
                                .unwrap_or_else(|| d.texts.default_output.clone());
                            let (tx, rx) = mpsc::channel();
                            std::thread::spawn(move || {
                                let chosen = rfd::FileDialog::new()
                                    .set_file_name(name)
                                    .add_filter("GeoJSON", &["geojson"])
                                    .save_file();
                                let _ = tx.send(chosen);
                            });
                            d.browse = Some(rx);
                        }
                    });
                    ui.add_space(6.0);

                    // 距离 [值或字段]
                    if ui.label(&d.texts.distance).hovered() {
                        d.topic = HelpTopic::Distance;
                    }
                    ui.horizontal(|ui| {
                        ui.radio_value(&mut d.use_field, false, &d.texts.linear_unit);
                        ui.add_enabled(
                            !d.use_field,
                            TextEdit::singleline(&mut d.value_text).desired_width(70.0),
                        );
                        egui::ComboBox::from_id_salt("buffer_unit")
                            .selected_text(label_of(&d.texts.units, d.unit))
                            .show_ui(ui, |ui| {
                                for (u, text) in &d.texts.units {
                                    ui.selectable_value(&mut d.unit, *u, text.as_str());
                                }
                            });
                    });
                    ui.horizontal(|ui| {
                        ui.radio_value(&mut d.use_field, true, &d.texts.field_label);
                        if fields.is_empty() {
                            ui.weak(&d.texts.no_field);
                        } else {
                            let selected = d.field.clone().unwrap_or_default();
                            egui::ComboBox::from_id_salt("buffer_field")
                                .selected_text(selected)
                                .show_ui(ui, |ui| {
                                    for f in &fields {
                                        ui.selectable_value(
                                            &mut d.field,
                                            Some(f.clone()),
                                            f.as_str(),
                                        );
                                    }
                                });
                        }
                    });
                    ui.add_space(6.0);

                    // 侧类型
                    if ui.label(&d.texts.side).hovered() {
                        d.topic = HelpTopic::Side;
                    }
                    combo(ui, "buffer_side", &d.texts.sides, &mut d.side);

                    // 末端类型
                    if ui.label(&d.texts.end).hovered() {
                        d.topic = HelpTopic::End;
                    }
                    combo(ui, "buffer_end", &d.texts.ends, &mut d.end);

                    // 融合类型
                    if ui.label(&d.texts.dissolve).hovered() {
                        d.topic = HelpTopic::Dissolve;
                    }
                    combo(ui, "buffer_dissolve", &d.texts.dissolves, &mut d.dissolve);

                    // 融合字段（复选框列表 + 全选 / 取消全选 / 添加字段）
                    if ui.label(&d.texts.dissolve_fields).hovered() {
                        d.topic = HelpTopic::DissolveFields;
                    }
                    egui::Frame::group(ui.style()).show(ui, |ui| {
                        ui.set_width(FORM_WIDTH - 8.0);
                        egui::ScrollArea::vertical()
                            .max_height(DISSOLVE_LIST_HEIGHT)
                            .show(ui, |ui| {
                                ui.set_width(FORM_WIDTH - 24.0);
                                if fields.is_empty() {
                                    ui.weak(&d.texts.no_field);
                                }
                                for f in &fields {
                                    let mut on = d.checked.iter().any(|c| c == f);
                                    if ui.checkbox(&mut on, f.as_str()).changed() {
                                        set_checked(&mut d.checked, f, on);
                                    }
                                }
                            });
                    });
                    ui.horizontal(|ui| {
                        if ui.button(&d.texts.select_all).clicked() {
                            d.checked = fields.clone();
                        }
                        if ui.button(&d.texts.deselect_all).clicked() {
                            d.checked.clear();
                        }
                        // ArcGIS 里「添加字段」只服务于 ModelBuilder，这里禁用。
                        ui.add_enabled(false, egui::Button::new(&d.texts.add_field))
                            .on_disabled_hover_text(&d.texts.add_field_tip);
                    });
                });

                // 右栏：上下文帮助
                if d.show_help {
                    ui.separator();
                    ui.vertical(|ui| {
                        ui.set_width(HELP_WIDTH);
                        ui.add_space(2.0);
                        ui.label(RichText::new(d.texts.topic_title(d.topic)).strong());
                        ui.add_space(4.0);
                        ui.label(d.texts.topic_body(d.topic));
                    });
                }
            });
        });
    });

    // 界面状态放回去再投消息（避免与主线程的取消息步骤形成嵌套加锁）。
    drop(guard);
    if let Some(msg) = outgoing
        && let Ok(mut queue) = msgs.lock()
    {
        queue.push(msg);
    }
}

/// 界面字符串快照：窗口回调里没有 `self`，所以主线程每帧翻译好送进来。
///
/// `Default` 是全空文本，仅用于刚构造、还没被主线程刷新过的那一瞬间。
#[derive(Default)]
struct Texts {
    input: String,
    output: String,
    distance: String,
    linear_unit: String,
    field_label: String,
    no_field: String,
    side: String,
    end: String,
    dissolve: String,
    dissolve_fields: String,
    select_all: String,
    deselect_all: String,
    add_field: String,
    add_field_tip: String,
    environments: String,
    hide_help: String,
    show_help: String,
    tool_help: String,
    ok: String,
    cancel: String,
    browse_layer: String,
    browse_output: String,
    no_layer: String,
    default_output: String,
    /// 帮助面板正文（按参数分条）。
    help_input: String,
    help_output: String,
    help_distance: String,
    help_side: String,
    help_end: String,
    help_dissolve: String,
    help_dissolve_fields: String,
    /// 校验失败的提示（`request()` 返回的 key → 文案）。
    error_no_input: String,
    error_bad_distance: String,
    error_no_dissolve_field: String,
    error_no_output: String,
    /// 枚举下拉框标签表：枚举值 → 当前语言显示文本。
    units: Vec<(LinearUnit, String)>,
    sides: Vec<(SideType, String)>,
    ends: Vec<(EndType, String)>,
    dissolves: Vec<(DissolveType, String)>,
}

impl Texts {
    fn new(app: &RustGisApp) -> Self {
        Self {
            input: app.t("buffer.input").to_string(),
            output: app.t("buffer.output").to_string(),
            distance: app.t("buffer.distance").to_string(),
            linear_unit: app.t("buffer.distance.linear").to_string(),
            field_label: app.t("buffer.distance.field").to_string(),
            no_field: app.t("buffer.noField").to_string(),
            side: app.t("buffer.side").to_string(),
            end: app.t("buffer.end").to_string(),
            dissolve: app.t("buffer.dissolve").to_string(),
            dissolve_fields: app.t("buffer.dissolveFields").to_string(),
            select_all: app.t("buffer.selectAll").to_string(),
            deselect_all: app.t("buffer.deselectAll").to_string(),
            add_field: app.t("buffer.addField").to_string(),
            add_field_tip: app.t("buffer.addFieldTip").to_string(),
            environments: app.t("buffer.environments").to_string(),
            hide_help: app.t("buffer.hideHelp").to_string(),
            show_help: app.t("buffer.showHelp").to_string(),
            tool_help: app.t("buffer.toolHelp").to_string(),
            ok: app.t("dialog.ok").to_string(),
            cancel: app.t("dialog.cancel").to_string(),
            browse_layer: app.t("layers.addTip").to_string(),
            browse_output: app.t("buffer.browseOutput").to_string(),
            no_layer: app.t("buffer.noLayer").to_string(),
            default_output: app.t("buffer.defaultOutput").to_string(),
            help_input: app.t("buffer.help.input").to_string(),
            help_output: app.t("buffer.help.output").to_string(),
            help_distance: app.t("buffer.help.distance").to_string(),
            help_side: app.t("buffer.help.side").to_string(),
            help_end: app.t("buffer.help.end").to_string(),
            help_dissolve: app.t("buffer.help.dissolve").to_string(),
            help_dissolve_fields: app.t("buffer.help.dissolveFields").to_string(),
            error_no_input: app.t("buffer.error.noInput").to_string(),
            error_bad_distance: app.t("buffer.error.badDistance").to_string(),
            error_no_dissolve_field: app.t("buffer.error.noDissolveField").to_string(),
            error_no_output: app.t("buffer.error.noOutput").to_string(),
            units: LinearUnit::ALL
                .iter()
                .map(|u| (*u, app.t(u.i18n_key()).to_string()))
                .collect(),
            sides: SideType::ALL
                .iter()
                .map(|s| (*s, app.t(s.i18n_key()).to_string()))
                .collect(),
            ends: EndType::ALL
                .iter()
                .map(|e| (*e, app.t(e.i18n_key()).to_string()))
                .collect(),
            dissolves: DissolveType::ALL
                .iter()
                .map(|v| (*v, app.t(v.i18n_key()).to_string()))
                .collect(),
        }
    }

    /// 帮助面板标题（复用表单标签，保证两处措辞一致）。
    fn topic_title(&self, topic: HelpTopic) -> &str {
        match topic {
            HelpTopic::Input => &self.input,
            HelpTopic::Output => &self.output,
            HelpTopic::Distance => &self.distance,
            HelpTopic::Side => &self.side,
            HelpTopic::End => &self.end,
            HelpTopic::Dissolve => &self.dissolve,
            HelpTopic::DissolveFields => &self.dissolve_fields,
        }
    }

    fn topic_body(&self, topic: HelpTopic) -> &str {
        match topic {
            HelpTopic::Input => &self.help_input,
            HelpTopic::Output => &self.help_output,
            HelpTopic::Distance => &self.help_distance,
            HelpTopic::Side => &self.help_side,
            HelpTopic::End => &self.help_end,
            HelpTopic::Dissolve => &self.help_dissolve,
            HelpTopic::DissolveFields => &self.help_dissolve_fields,
        }
    }

    /// 校验失败提示（`BufferDialog::request` 的 key → 文案）。
    fn error_text(&self, key: &str) -> String {
        match key {
            "buffer.error.noInput" => self.error_no_input.clone(),
            "buffer.error.badDistance" => self.error_bad_distance.clone(),
            "buffer.error.noDissolveField" => self.error_no_dissolve_field.clone(),
            "buffer.error.noOutput" => self.error_no_output.clone(),
            other => other.to_string(),
        }
    }
}

/// 在下拉框标签表里找当前值的显示文本。
fn label_of<T: PartialEq>(labels: &[(T, String)], value: T) -> &str {
    labels
        .iter()
        .find(|(v, _)| *v == value)
        .map(|(_, text)| text.as_str())
        .unwrap_or("")
}

/// 通用枚举下拉框：`labels` 是「枚举值 → 显示文本」表（按当前语言生成）。
fn combo<T: PartialEq + Copy>(ui: &mut egui::Ui, id: &str, labels: &[(T, String)], value: &mut T) {
    egui::ComboBox::from_id_salt(id)
        .selected_text(label_of(labels, *value))
        .width(FORM_WIDTH - 8.0)
        .show_ui(ui, |ui| {
            for (v, text) in labels {
                ui.selectable_value(value, *v, text.as_str());
            }
        });
}

/// 勾选 / 取消勾选一个融合字段（保持列表里无重复项）。
fn set_checked(list: &mut Vec<String>, name: &str, on: bool) {
    list.retain(|f| f != name);
    if on {
        list.push(name.to_string());
    }
}

/// 默认输出文件名：`{图层名}_Buffer.geojson`（与 ArcGIS 的 `_Buffer` 后缀一致）。
fn default_output_path(layer_name: &str) -> String {
    format!("{}.geojson", buffer::default_output_name(layer_name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geo::PolygonData;

    /// 1000x1000 的方形面（世界坐标即投影单位，测缓冲区几何用不到真实坐标系）。
    fn square_layer(name: &str) -> Layer {
        let mut l = Layer::from_geometry(
            name.to_string(),
            Vec::new(),
            Vec::new(),
            vec![PolygonData {
                outer: vec![
                    [0.0, 0.0],
                    [1000.0, 0.0],
                    [1000.0, 1000.0],
                    [0.0, 1000.0],
                    [0.0, 0.0],
                ],
                holes: Vec::new(),
            }],
        );
        l.fields = vec!["Id".to_string(), "dis".to_string()];
        l.attrs = vec![vec!["7".to_string(), "100".to_string()]];
        l
    }

    /// 无头跑一帧界面（`Context::default()` 时 egui 把视口内嵌 ⇒ 回调当场执行，
    /// 因此对话框的绘制路径（含面板 / 滚动区 / 下拉框）在测试里也走一遍）。
    fn frame(app: &mut RustGisApp, ctx: &Context) {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(1280.0, 900.0),
            )),
            ..Default::default()
        };
        let _ = ctx.run(input, |ctx| app.show_buffer_dialog(ctx));
    }

    /// 取校验失败原因（`BufferMsg` 没有 `Debug`，不能直接 `unwrap_err`）。
    fn err_key(r: Result<BufferMsg, &'static str>) -> &'static str {
        match r {
            Err(key) => key,
            Ok(_) => panic!("表单应当校验失败"),
        }
    }

    fn temp_out(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "rustgis_buffer_dialog_{}_{}.geojson",
            std::process::id(),
            tag
        ));
        p
    }

    /// 表单校验：缺输入图层 / 距离非法 / LIST 未选字段 / 输出为空 各自的 i18n key。
    #[test]
    fn request_reports_each_invalid_field() {
        let mut d = BufferDialog::default();
        assert_eq!(err_key(d.request()), "buffer.error.noInput");

        d.input_id = Some(1);
        d.output_path = "out.geojson".to_string();
        // 默认 500 米 / FULL / ROUND / NONE ⇒ 合法。
        let Ok(BufferMsg::Accept { params, .. }) = d.request() else {
            panic!("默认表单应校验通过");
        };
        assert_eq!(params.distance, DistanceSource::Value(500.0));
        assert_eq!(params.unit, LinearUnit::Meters);
        assert_eq!(params.dissolve, DissolveType::None);

        // 线性单位：零 / 非数字 / 无穷 都不合法，负值是允许的（侵蚀）。
        for bad in ["0", "abc", "inf", ""] {
            d.value_text = bad.to_string();
            assert_eq!(err_key(d.request()), "buffer.error.badDistance", "{bad}");
        }
        d.value_text = "-120.5".to_string();
        let Ok(BufferMsg::Accept { params, .. }) = d.request() else {
            panic!("负距离是合法的侵蚀缓冲");
        };
        assert_eq!(params.distance, DistanceSource::Value(-120.5));

        // 字段距离：没选字段不合法，选了就进参数。
        d.use_field = true;
        assert_eq!(err_key(d.request()), "buffer.error.badDistance");
        d.field = Some("dis".to_string());
        let Ok(BufferMsg::Accept { params, .. }) = d.request() else {
            panic!("选了字段应校验通过");
        };
        assert_eq!(params.distance, DistanceSource::Field("dis".to_string()));

        // LIST 融合必须至少勾一个字段。
        d.use_field = false;
        d.dissolve = DissolveType::List;
        assert_eq!(err_key(d.request()), "buffer.error.noDissolveField");
        d.checked = vec!["Id".to_string()];
        let Ok(BufferMsg::Accept { params, .. }) = d.request() else {
            panic!("勾了融合字段应校验通过");
        };
        assert_eq!(params.dissolve_fields, vec!["Id".to_string()]);

        // 输出路径是必填（去掉首尾空白后仍为空视为没填）。
        d.checked.clear();
        d.dissolve = DissolveType::None;
        d.output_path = "   ".to_string();
        assert_eq!(err_key(d.request()), "buffer.error.noOutput");
    }

    /// 打开 → 每帧刷新共享状态 → 取消消息关窗：独立窗口的生命周期由主线程的
    /// 「每帧登记视口」决定，这里验证登记 / 注销与状态同步。
    #[test]
    fn open_refresh_and_cancel_window() {
        let ctx = Context::default();
        let mut app = RustGisApp::new(&ctx);
        let id = app.layers.add(square_layer("sq"));

        app.open_buffer_dialog();
        let window = app.buffer_dialog.as_ref().expect("对话框应已打开");
        {
            let d = window.state.lock().unwrap();
            assert_eq!(d.input_id, Some(id), "默认选中第一个图层");
            assert_eq!(d.output_path, "sq_Buffer.geojson");
        }

        // 主线程每帧把图层列表与当前语言文案刷进共享状态。
        frame(&mut app, &ctx);
        {
            let window = app.buffer_dialog.as_ref().expect("渲染后窗口仍在");
            let d = window.state.lock().unwrap();
            assert_eq!(d.choices.len(), 1);
            assert_eq!(d.choices[0].fields, vec!["Id", "dis"]);
            assert!(!d.texts.input.is_empty(), "文案应已翻译");
            assert!(!d.texts.topic_body(HelpTopic::Dissolve).is_empty());
        }

        // 再打开一次：不重复开窗，只抢焦点。
        app.open_buffer_dialog();
        assert!(
            app.buffer_dialog
                .as_ref()
                .unwrap()
                .state
                .lock()
                .unwrap()
                .focus,
            "重复打开应只抢焦点"
        );

        // 窗口发 Cancel ⇒ 主线程不再登记视口（窗口下一帧被销毁）。
        app.buffer_dialog
            .as_ref()
            .unwrap()
            .msgs
            .lock()
            .unwrap()
            .push(BufferMsg::Cancel);
        frame(&mut app, &ctx);
        assert!(app.buffer_dialog.is_none(), "取消应关闭独立窗口");
    }

    /// 确定 → 后台缓冲 → 轮询 → 结果图层加入图层树（端到端，含写盘与再导入）。
    #[test]
    fn accept_runs_job_and_adds_result_layer() {
        let ctx = Context::default();
        let mut app = RustGisApp::new(&ctx);
        let id = app.layers.add(square_layer("sq"));
        let out_path = temp_out("accept");

        app.open_buffer_dialog();
        {
            let window = app.buffer_dialog.as_ref().unwrap();
            let mut d = window.state.lock().unwrap();
            d.input_id = Some(id);
            d.value_text = "100".to_string();
            d.output_path = out_path.display().to_string();
        }
        frame(&mut app, &ctx);
        app.buffer_dialog
            .as_ref()
            .unwrap()
            .msgs
            .lock()
            .unwrap()
            .push(BufferMsg::Accept {
                input_id: id,
                params: BufferParams {
                    distance: DistanceSource::Value(100.0),
                    unit: LinearUnit::Meters,
                    side: SideType::Full,
                    end: EndType::Round,
                    dissolve: DissolveType::None,
                    dissolve_fields: Vec::new(),
                },
                output_path: out_path.clone(),
            });
        frame(&mut app, &ctx);
        assert!(app.buffer_dialog.is_none(), "确定后应关闭窗口");
        assert!(app.buffer_job.is_some(), "确定后应起后台任务");

        // 等后台线程（缓冲 + 三角剖分 + 写盘）回来。
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while app.buffer_job.is_some() && std::time::Instant::now() < deadline {
            app.poll_buffer_job(&ctx);
            std::thread::sleep(Duration::from_millis(10));
        }
        app.poll_buffer_job(&ctx);
        assert!(app.buffer_job.is_none(), "后台任务应在超时前完成");
        assert!(out_path.exists(), "结果应写出 GeoJSON");

        let names: Vec<String> = app
            .layers
            .panel_rows()
            .iter()
            .filter_map(|r| r.node.layer().map(|l| l.name.clone()))
            .collect();
        assert!(
            names.iter().any(|n| n == "sq_Buffer"),
            "结果图层应加入图层树：{names:?}"
        );
        let _ = std::fs::remove_file(&out_path);
    }
}
