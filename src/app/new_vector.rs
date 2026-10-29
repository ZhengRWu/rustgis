//! 新建矢量图层：对话框 → 数字化绘制 → 落盘。
//!
//! 对标 QGIS「浏览器面板 → 右键目录 → New → ShapeFile…」
//! （`QgsAppDirectoryItemGuiProvider::populateContextMenu` +
//! `QgsNewVectorLayerDialog`），但把 QGIS 的「建空文件 → 手动开启编辑 → 画 →
//! 保存编辑」四步压成一步半：
//!
//! 1. **对话框**（[`NewVectorDialog`]）与 QGIS 同构：文件名 / 文件格式（Shapefile /
//!    GeoJSON）/ 几何类型（点 线 面）/ 坐标系 / 新字段（名称 类型 长度 精度）/
//!    字段列表（上移 下移 删除）；默认预置一个 `id` 整数字段，与 QGIS 一致；
//! 2. **确定** → 建空图层加入内容列表 + 自动切到数字化工具
//!    [`crate::map::Tool::Edit`]（0 要素时先不落盘：Shapefile 的 shape type 要等
//!    第一个要素写进去才确定，抢先写只会得到一份导不回来的「Null 类型」空文件）；
//! 3. **绘制**：交互完整对标 QGIS `QgsMapToolCapture`
//!    （`src/gui/maptools/qgsmaptoolcapture.cpp`，CapturePolygon / CaptureLine 模式）：
//!    左键加点、右键结束要素（面的环自动闭合，无需点回起点）、
//!    Backspace / Delete 撤销最后一个节点、Esc 放弃草图；
//!    中键拖动平移、滚轮缩放照常；橡皮筋预览用 QGIS 数字化默认配色
//!    （主带实线 + 半透明填充，临时带虚线）；
//! 4. **保存**：`保存编辑` 后台线程写盘（[`crate::geo::vector_file`]），
//!    停止编辑时有未保存改动会先用系统原生询问框确认。
//!
//! 对话框跑在独立系统窗口里（与「缓冲区」同一套 `Arc<Mutex<..>>` + 邮箱机制，
//! 见 [`super::buffer_dialog`] 的模块注释），UI 线程全程不阻塞。

use std::path::PathBuf;
use std::sync::mpsc::{self, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use egui::{Context, TextEdit, ViewportBuilder, ViewportId};

use crate::dialog::{self, Confirm, Dialog};
use crate::geo::vector_file::{FieldDef, FieldKind, GeometryKind, VectorFormat};
use crate::geo::{self, Coord2D, Geometry2D, Layer, ProjectionInfo};
use crate::map;
use crate::style;

use super::{RustGisApp, Status};

/// 对话框窗口的初始 / 最小尺寸。
const WINDOW_WIDTH: f32 = 560.0;
const WINDOW_HEIGHT: f32 = 620.0;
const MIN_WINDOW_WIDTH: f32 = 460.0;
const MIN_WINDOW_HEIGHT: f32 = 460.0;

/// 表单左侧的固定宽度标签（egui 没有 `add_sized_text`，这里统一一个入口，
/// 让「标签 / 输入」两列的宽度在对话框里对齐）。
fn form_label(ui: &mut egui::Ui, width: f32, text: &str) {
    ui.add_sized(egui::vec2(width, 20.0), egui::Label::new(text));
}

// ---------------------------------------------------------------------------
// 编辑会话
// ---------------------------------------------------------------------------

/// 一次数字化会话：新建矢量后自动进入，停止编辑后销毁。
///
/// 字段对标 QGIS `QgsMapToolCapture` 的采集状态：
/// `draft` ↔ `mCaptureCurve`（已确认节点），`cursor` ↔ 临时橡皮筋的鼠标终点；
/// 「是否采集中」由 `draft` 是否非空体现（↔ `mCapturing`）。
pub(super) struct Editing {
    /// 正在被画入的图层。
    pub layer_id: u64,
    pub geom: GeometryKind,
    /// 新建对话框里定义的字段（写盘时要用到类型 / 长度 / 精度）。
    pub fields: Vec<FieldDef>,
    /// 落盘路径与格式。
    pub path: PathBuf,
    pub format: VectorFormat,
    /// 已点下、尚未成形的节点（世界坐标）。
    pub draft: Vec<Coord2D>,
    /// 鼠标位置（橡皮筋终点）。
    pub cursor: Option<Coord2D>,
    /// 当前要素的属性输入值（与 `fields` 等长）。
    pub values: Vec<String>,
    /// 有尚未落盘的改动。
    pub dirty: bool,
}

impl Editing {
    /// 当前草图成形为几何。面要素在这里自动闭合（对标 QGIS 右键结束时的
    /// `closePolygon()` → `mCaptureCurve.close()`：把第一个节点补到环末尾，
    /// 用户不必点回起点；Shapefile 的 polygon 环同样要求首尾闭合）。
    fn draft_geometry(&self) -> Option<Geometry2D> {
        if self.draft.len() < self.geom.min_vertices() {
            return None;
        }
        Some(match self.geom {
            GeometryKind::Point => Geometry2D::Point(self.draft[0]),
            GeometryKind::Line => Geometry2D::Line(self.draft.clone()),
            GeometryKind::Polygon => {
                let mut outer = self.draft.clone();
                if let Some(first) = self.draft.first().copied()
                    && outer.last() != Some(&first)
                {
                    outer.push(first);
                }
                Geometry2D::Polygon {
                    outer,
                    holes: Vec::new(),
                }
            }
        })
    }
}

/// 后台保存任务（返回已写入的要素数）。
pub(super) struct SaveJob {
    rx: mpsc::Receiver<Result<usize, String>>,
}

// ---------------------------------------------------------------------------
// 新建矢量对话框（独立窗口）
// ---------------------------------------------------------------------------

/// 独立窗口 → 主线程的动作。
enum NewVectorMsg {
    /// 确定：表单已通过校验。
    Accept {
        path: PathBuf,
        format: VectorFormat,
        geom: GeometryKind,
        crs: CrsChoice,
        fields: Vec<FieldDef>,
    },
    Cancel,
}

/// 独立窗口的句柄（视口 id + 共享状态 + 邮箱）。
pub(super) struct NewVectorWindow {
    id: ViewportId,
    state: Arc<Mutex<NewVectorDialog>>,
    msgs: Arc<Mutex<Vec<NewVectorMsg>>>,
}

/// 坐标系选择（「跟随工程」用工程当前投影，未设置时按 WGS 84）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CrsChoice {
    Follow,
    Wgs84,
    Cgcs2000,
    WebMercator,
}

impl CrsChoice {
    fn resolve(self, project: Option<&ProjectionInfo>) -> ProjectionInfo {
        match self {
            Self::Follow => project.cloned().unwrap_or_else(ProjectionInfo::wgs84),
            Self::Wgs84 => ProjectionInfo::wgs84(),
            Self::Cgcs2000 => ProjectionInfo::cgcs2000(),
            Self::WebMercator => ProjectionInfo::web_mercator(),
        }
    }
}

/// 已通过校验、等待「覆盖确认」的创建请求。
pub(super) struct PendingCreate {
    path: PathBuf,
    format: VectorFormat,
    geom: GeometryKind,
    crs: CrsChoice,
    fields: Vec<FieldDef>,
}

/// 对话框的全部界面状态。
struct NewVectorDialog {
    path: String,
    format: VectorFormat,
    geom: Option<GeometryKind>,
    crs: CrsChoice,
    field_name: String,
    field_kind: FieldKind,
    length_text: String,
    precision_text: String,
    fields: Vec<FieldDef>,
    selected: Option<usize>,
    error: Option<String>,
    /// 「文件名」右侧的系统保存对话框（后台线程，不阻塞 UI）。
    browse: Option<mpsc::Receiver<Option<PathBuf>>>,
    texts: Texts,
}

impl Default for NewVectorDialog {
    fn default() -> Self {
        let (length_text, precision_text) = FieldKind::Integer.default_metrics();
        Self {
            path: String::new(),
            format: VectorFormat::Shapefile,
            geom: None,
            crs: CrsChoice::Follow,
            field_name: String::new(),
            field_kind: FieldKind::Integer,
            length_text: length_text.to_string(),
            precision_text: precision_text.to_string(),
            // QGIS 新建 Shapefile 时预置一个 `id` 整数字段。
            fields: vec![FieldDef::default_id()],
            selected: None,
            error: None,
            browse: None,
            texts: Texts::default(),
        }
    }
}

/// 窗口里用到的界面文案（主线程每帧按当前语言刷新）。
#[derive(Default)]
struct Texts {
    title: String,
    file: String,
    browse: String,
    format: String,
    geom: String,
    point: String,
    line: String,
    polygon: String,
    crs: String,
    crs_follow: String,
    crs_wgs84: String,
    crs_cgcs2000: String,
    crs_web: String,
    new_field: String,
    name: String,
    kind: String,
    length: String,
    precision: String,
    kind_text: String,
    kind_int: String,
    kind_real: String,
    kind_date: String,
    add_field: String,
    fields: String,
    up: String,
    down: String,
    remove: String,
    create: String,
    cancel: String,
    err_no_file: String,
    err_no_geom: String,
    err_field_name: String,
    err_dup_field: String,
    err_bad_number: String,
    err_pending_field: String,
    err_name_long: String,
    fmt_shapefile: String,
    fmt_geojson: String,
}

impl Texts {
    fn new(app: &RustGisApp) -> Self {
        let t = |k: &str| app.t(k).to_string();
        Self {
            title: t("newvector.title"),
            file: t("newvector.file"),
            browse: t("newvector.browse"),
            format: t("newvector.format"),
            geom: t("newvector.geom"),
            point: t("newvector.geom.point"),
            line: t("newvector.geom.line"),
            polygon: t("newvector.geom.polygon"),
            crs: t("newvector.crs"),
            crs_follow: app.tf(
                "newvector.crs.follow",
                &[&app
                    .project_projection
                    .as_ref()
                    .map(|p| p.crs.label())
                    .unwrap_or_else(|| app.t("projection.none").to_string())],
            ),
            crs_wgs84: t("newvector.crs.wgs84"),
            crs_cgcs2000: t("newvector.crs.cgcs2000"),
            crs_web: t("newvector.crs.web"),
            new_field: t("newvector.newField"),
            name: t("newvector.field.name"),
            kind: t("newvector.field.type"),
            length: t("newvector.field.length"),
            precision: t("newvector.field.precision"),
            kind_text: t("newvector.type.text"),
            kind_int: t("newvector.type.int"),
            kind_real: t("newvector.type.real"),
            kind_date: t("newvector.type.date"),
            add_field: t("newvector.addField"),
            fields: t("newvector.fields"),
            up: t("newvector.up"),
            down: t("newvector.down"),
            remove: t("newvector.remove"),
            create: t("newvector.create"),
            cancel: t("dialog.cancel"),
            err_no_file: t("newvector.error.noFile"),
            err_no_geom: t("newvector.error.noGeom"),
            err_field_name: t("newvector.error.fieldName"),
            err_dup_field: t("newvector.error.dupField"),
            err_bad_number: t("newvector.error.badNumber"),
            err_pending_field: t("newvector.error.pendingField"),
            err_name_long: t("newvector.error.nameTooLong"),
            fmt_shapefile: t("newvector.format.shapefile"),
            fmt_geojson: t("newvector.format.geojson"),
        }
    }
}

impl NewVectorDialog {
    /// 校验 + 组装「确定」动作；失败时返回要显示在窗口底部的错误文案。
    fn request(&self) -> Result<NewVectorMsg, String> {
        let texts = &self.texts;
        let path = self.path.trim();
        if path.is_empty() {
            return Err(texts.err_no_file.clone());
        }
        let Some(geom) = self.geom else {
            return Err(texts.err_no_geom.clone());
        };
        // QGIS 的 `accept()`：输入框里还有没加进列表的字段名时先拦一下，
        // 避免用户以为它已经生效。
        let pending = self.field_name.trim();
        if !pending.is_empty() && !self.fields.iter().any(|f| f.name == pending) {
            return Err(texts.err_pending_field.replace("{0}", pending));
        }
        if self.format == VectorFormat::Shapefile {
            for f in &self.fields {
                if f.name.as_bytes().len() > 10 {
                    return Err(texts.err_name_long.replace("{0}", &f.name));
                }
            }
        }
        Ok(NewVectorMsg::Accept {
            path: PathBuf::from(path),
            format: self.format,
            geom,
            crs: self.crs,
            fields: self.fields.clone(),
        })
    }

    /// 「添加到字段列表」：名称非空、不与已有字段重名、长度 / 精度是整数。
    fn add_field(&mut self) {
        let name = self.field_name.trim().to_string();
        if name.is_empty() {
            self.error = Some(self.texts.err_field_name.clone());
            return;
        }
        if self.fields.iter().any(|f| f.name == name) {
            self.error = Some(self.texts.err_dup_field.replace("{0}", &name));
            return;
        }
        let (length, precision) = match (
            self.length_text.trim().parse::<u8>(),
            self.precision_text.trim().parse::<u8>(),
        ) {
            (Ok(l), Ok(p)) => (l, p),
            _ => {
                self.error = Some(self.texts.err_bad_number.clone());
                return;
            }
        };
        self.fields.push(FieldDef {
            name,
            kind: self.field_kind,
            length,
            precision,
        });
        self.selected = Some(self.fields.len() - 1);
        self.field_name.clear();
        self.error = None;
    }
}

impl RustGisApp {
    /// 打开「新建矢量图层」对话框（目录栏右键 → 新建 → 矢量）。
    pub(super) fn open_new_vector_dialog(&mut self) {
        if self.new_vector.is_some() {
            // 已开着：不再弹第二个，保持当前内容（用户可能已填了一堆字段）。
            return;
        }
        let mut dialog = NewVectorDialog::default();
        // 默认落在目录栏当前目录下，文件名沿用 QGIS 的 `new_layer.shp`。
        let dir = self.catalog.location.clone();
        let name = format!("new_layer.{}", VectorFormat::Shapefile.extension());
        dialog.path = dir.join(&name).display().to_string();
        dialog.texts = Texts::new(self);
        self.new_vector = Some(NewVectorWindow {
            id: ViewportId::from_hash_of("rustgis_new_vector_dialog"),
            state: Arc::new(Mutex::new(dialog)),
            msgs: Arc::new(Mutex::new(Vec::new())),
        });
    }

    /// 每帧推进对话框窗口（未打开时什么都不做）。
    pub(super) fn show_new_vector_dialog(&mut self, ctx: &Context) {
        let Some(window) = self.new_vector.as_ref() else {
            return;
        };
        let (id, state, msgs) = (
            window.id,
            Arc::clone(&window.state),
            Arc::clone(&window.msgs),
        );
        {
            let mut d = state.lock().unwrap_or_else(|e| e.into_inner());
            d.texts = Texts::new(self);
        }
        let builder = ViewportBuilder::default()
            .with_title(self.t("newvector.title"))
            .with_inner_size([WINDOW_WIDTH, WINDOW_HEIGHT])
            .with_min_inner_size([MIN_WINDOW_WIDTH, MIN_WINDOW_HEIGHT])
            .with_resizable(true)
            .with_active(true);
        let window_msgs = Arc::clone(&msgs);
        ctx.show_viewport_immediate(id, builder, move |ctx, _class| {
            new_vector_window_ui(ctx, &state, &window_msgs);
        });

        let pending: Vec<NewVectorMsg> = {
            let mut queue = msgs.lock().unwrap_or_else(|e| e.into_inner());
            std::mem::take(&mut *queue)
        };
        for msg in pending {
            match msg {
                NewVectorMsg::Accept {
                    path,
                    format,
                    geom,
                    crs,
                    fields,
                } => {
                    // 目标已存在：与 QGIS 一样先问一句再覆盖（写盘是截断写，
                    // 静默覆盖会把用户原有数据抹掉）。
                    if path.exists() && self.overwrite_prompt.is_none() {
                        let title = self.t("newvector.title").to_string();
                        let text = self.tf("newvector.exists", &[&path.display().to_string()]);
                        let yes = self.t("newvector.overwrite").to_string();
                        let no = self.t("dialog.cancel").to_string();
                        self.overwrite_prompt = Some((
                            PendingCreate {
                                path,
                                format,
                                geom,
                                crs,
                                fields,
                            },
                            Confirm::open(title, text, yes, no, dialog::Level::Warning),
                        ));
                        return;
                    }
                    self.new_vector = None;
                    self.create_vector_layer(path, format, geom, crs, fields);
                    return;
                }
                NewVectorMsg::Cancel => {
                    self.new_vector = None;
                    return;
                }
            }
        }
    }

    /// 确定：建空图层 → 加入内容列表 → 落盘 0 要素数据集 → 进入数字化。
    fn create_vector_layer(
        &mut self,
        path: PathBuf,
        format: VectorFormat,
        geom: GeometryKind,
        crs: CrsChoice,
        fields: Vec<FieldDef>,
    ) {
        let projection = crs.resolve(self.project_projection.as_ref());
        let name = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.t("window.untitled").to_string());
        let mut layer = Layer::default();
        layer.name = name;
        layer.projection = projection.clone();
        layer.fields = fields.iter().map(|f| f.name.clone()).collect();

        let layer_id = self.layers.add(layer);
        if self.project_projection.is_none() {
            self.project_projection = Some(projection);
            self.title_dirty = true;
        }
        self.dirty = true;

        self.editing = Some(Editing {
            layer_id,
            geom,
            fields,
            path,
            format,
            draft: Vec::new(),
            cursor: None,
            values: Vec::new(),
            dirty: false,
        });
        self.map.tool = map::Tool::Edit;
        // 立刻落一份 0 要素的数据集：目录栏马上能看到 .shp（与 QGIS 建完就有文件一致）。
        self.save_editing(true);
        let name = self
            .layers
            .node(layer_id)
            .and_then(|n| n.layer())
            .map(|l| l.name.clone())
            .unwrap_or_default();
        self.status = Status::Message(self.tf("newvector.created", &[&name]));
    }

    /// 后台保存：把当前编辑图层写成 `path` 指定的数据集。
    ///
    /// `initial` 为 `true` 时是「建完即落盘」，不弹状态栏提示（避免刚建好就报
    /// "已保存"），只在失败时报错。
    fn save_editing(&mut self, initial: bool) {
        let Some(ed) = self.editing.as_ref() else {
            return;
        };
        let layer = self
            .layers
            .node(ed.layer_id)
            .and_then(|n| n.layer())
            .cloned();
        let Some(layer) = layer else {
            return;
        };
        // 0 要素不落盘：Shapefile 的 shape type 要等第一个要素写进去才能确定，
        // 抢先写只会得到一个「Null 类型」的空文件（连自己都导不回来）。
        // 画完第一个要素后的每次保存都至少有一个要素，不受这条影响。
        if layer.feature_count == 0 {
            return;
        }
        let (fields, path, format, geom) =
            (ed.fields.clone(), ed.path.clone(), ed.format, ed.geom);
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let out = geo::vector_file::write_layer(&layer, &fields, &path, format, geom)
                .map(|()| layer.feature_count);
            let _ = tx.send(out);
        });
        self.save_job = Some(SaveJob { rx });
        if !initial {
            self.status = Status::Message(self.t("digitize.saving").to_string());
        }
    }

    /// 后台保存流水线推进（每帧一次，非阻塞）。
    pub(super) fn poll_save_job(&mut self, ctx: &Context) {
        let Some(job) = self.save_job.take() else {
            return;
        };
        match job.rx.try_recv() {
            Ok(Ok(count)) => {
                if let Some(ed) = self.editing.as_mut() {
                    ed.dirty = false;
                }
                let count = count.to_string();
                self.status = Status::Message(self.tf("digitize.savedToast", &[&count]));
            }
            Ok(Err(e)) => {
                self.status = Status::Message(self.tf("status.loadError", &[&e]));
                self.dialog = Some(Dialog::Error { body: e });
            }
            Err(TryRecvError::Empty) => {
                self.save_job = Some(job);
                ctx.request_repaint_after(Duration::from_millis(50));
            }
            Err(TryRecvError::Disconnected) => {
                let msg = self.t("status.loadAborted").to_string();
                self.status = Status::Message(msg.clone());
                self.dialog = Some(Dialog::Error { body: msg });
            }
        }
    }

    /// 停止编辑：有未保存改动时先用系统原生询问框确认（保存 / 不保存）。
    fn stop_editing(&mut self) {
        let Some(ed) = self.editing.as_ref() else {
            return;
        };
        if ed.dirty && self.stop_prompt.is_none() {
            let name = self
                .layers
                .node(ed.layer_id)
                .and_then(|n| n.layer())
                .map(|l| l.name.clone())
                .unwrap_or_default();
            let title = self.t("digitize.stopTitle").to_string();
            let text = self.tf("digitize.stopBody", &[&name]);
            let yes = self.t("digitize.stopSave").to_string();
            let no = self.t("digitize.stopDiscard").to_string();
            self.stop_prompt = Some(Confirm::open(
                title,
                text,
                yes,
                no,
                dialog::Level::Warning,
            ));
            return;
        }
        self.editing = None;
        self.map.tool = map::Tool::Pan;
        self.status = Status::Message(self.t("digitize.stopped").to_string());
    }

    /// 「目标文件已存在，是否覆盖」的询问结果。
    pub(super) fn poll_overwrite_prompt(&mut self) {
        let Some((pending, prompt)) = self.overwrite_prompt.take() else {
            return;
        };
        match prompt.poll() {
            Some(true) => {
                self.new_vector = None;
                let PendingCreate {
                    path,
                    format,
                    geom,
                    crs,
                    fields,
                } = pending;
                self.create_vector_layer(path, format, geom, crs, fields);
            }
            Some(false) => {}
            None => self.overwrite_prompt = Some((pending, prompt)),
        }
    }

    /// 询问框结果轮询（「停止编辑」时是否保存）。
    pub(super) fn poll_stop_prompt(&mut self) {
        let Some(prompt) = self.stop_prompt.take() else {
            return;
        };
        match prompt.poll() {
            Some(true) => {
                self.save_editing(false);
                // 保存跑在后台，编辑会话到此结束（落盘用的是图层快照）。
                self.editing = None;
                self.map.tool = map::Tool::Pan;
            }
            Some(false) => {
                self.editing = None;
                self.map.tool = map::Tool::Pan;
            }
            None => self.stop_prompt = Some(prompt),
        }
    }

    // -----------------------------------------------------------------------
    // 数字化交互（由 `map_view` 每帧调用）
    //
    // 手势完整对标 QGIS `QgsMapToolCapture`（CapturePolygon / CaptureLine）：
    // - 左键单击        → `addVertex`：追加节点；
    // - 右键单击        → 右键释放分支：结束要素（唯一结束手段；QGIS 里双击
    //                     只是连续两次加点，并无「双击 / 回车结束」）；
    // - Backspace/Delete → `undo`：撤销最后一个节点（只剩 1 个节点时短路，
    //                     与 QGIS `size() <= 1` 一致，首个节点靠 Esc 重来）；
    // - Esc             → `stopCapturing`：放弃当前草图（已完成要素不受影响）。
    // -----------------------------------------------------------------------

    /// 地图上的数字化交互：加点 / 结束 / 撤点 / 放弃 + 中键平移。
    pub(super) fn handle_digitize(
        &mut self,
        ui: &mut egui::Ui,
        response: &egui::Response,
        rect: egui::Rect,
        view: &map::MapView,
    ) {
        if self.editing.is_none() {
            return;
        }
        // 编辑会话期间工具就锁定为数字化：否则用户点了工具条上的「平移」后，
        // 左键拖动会平移、双击又会变成"缩放至全图"，和加节点抢同一个手势。
        // 平移仍可用中键拖动，缩放用滚轮。
        if self.map.tool != map::Tool::Edit {
            self.map.tool = map::Tool::Edit;
        }
        if response.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Crosshair);
        }
        // 编辑期间左键归数字化，平移改中键拖动（滚轮缩放仍照常）。
        if response.dragged_by(egui::PointerButton::Middle) {
            let mut view = *view;
            let d = response.drag_delta();
            view.center[0] -= d.x as f64 / view.scale as f64;
            view.center[1] += d.y as f64 / view.scale as f64;
            self.map.view = Some(view);
        }
        // 键盘：Esc 放弃（`QgsMapToolDigitizeFeature::keyPressEvent`），
        // Backspace / Delete 撤点（`QgsMapToolCapture::keyPressEvent`）。
        let escape = ui.input(|i| i.key_pressed(egui::Key::Escape));
        let backspace = ui.input(|i| i.key_pressed(egui::Key::Backspace));
        let delete = ui.input(|i| i.key_pressed(egui::Key::Delete));
        if escape {
            self.stop_capturing();
        } else if backspace || delete {
            self.undo();
        }

        // 鼠标移动：光标世界坐标即临时橡皮筋的终点（`cadCanvasMoveEvent`）。
        if let Some(pos) = response.hover_pos() {
            if rect.contains(pos) {
                let world = map::world_at(pos, rect, view);
                if let Some(ed) = self.editing.as_mut() {
                    ed.cursor = Some(world);
                }
            }
        }

        // 左键：加点（`cadCanvasReleaseEvent` 左键分支 → `addVertex`）。
        // 点要素：一个点即成形（QGIS CapturePoint 分支：加完即 `geometryCaptured`）。
        if response.clicked_by(egui::PointerButton::Primary)
            && let Some(pos) = response.interact_pointer_pos()
        {
            let world = map::world_at(pos, rect, view);
            self.add_vertex(world);
            if self.editing.as_ref().is_some_and(|e| e.geom == GeometryKind::Point) {
                self.finish_feature();
            }
        }
        // 右键：结束要素（QGIS 线 / 面唯一的结束手段）。
        if response.clicked_by(egui::PointerButton::Secondary) {
            self.finish_feature();
        }
    }

    /// 追加一个节点（对标 `QgsMapToolCapture::addVertex`：节点进采集曲线，
    /// 两块橡皮筋随之重画；第一个节点同时作为自动闭合的「第一点」）。
    fn add_vertex(&mut self, world: Coord2D) {
        if let Some(ed) = self.editing.as_mut() {
            ed.draft.push(world);
        }
    }

    /// 当前草图成形为一个要素并写进图层（属性取编辑面板里的输入值）。
    ///
    /// 对标 `QgsMapToolCapture::cadCanvasReleaseEvent` 右键分支：线不足 2 点 /
    /// 面不足 3 点时 `stopCapturing()` 直接丢弃草图（这里多给一条状态栏提示）；
    /// 足够时先自动闭合（面），再构造几何写进图层。
    fn finish_feature(&mut self) {
        let Some(mut ed) = self.editing.take() else {
            return;
        };
        let Some(geometry) = ed.draft_geometry() else {
            // 节点数不够：与 QGIS 一样丢弃草图（`stopCapturing`）。
            let msg = self.t(match ed.geom {
                GeometryKind::Point => "digitize.needPoints.line",
                GeometryKind::Line => "digitize.needPoints.line",
                GeometryKind::Polygon => "digitize.needPoints.polygon",
            });
            ed.draft.clear();
            ed.cursor = None;
            self.editing = Some(ed);
            self.status = Status::Message(msg.to_string());
            return;
        };
        let mut added = false;
        if let Some(node) = self.layers.node_mut(ed.layer_id)
            && let geo::LayerNode::Layer(layer) = node
        {
            let mut values = ed.values.clone();
            values.resize(layer.fields.len(), String::new());
            geo::vector_file::push_geometry(layer, geometry, values);
            added = true;
        }
        if !added {
            // 图层被删掉了：结束会话，避免继续往空气里画。
            self.map.tool = map::Tool::Pan;
            let msg = self.t("status.loadAborted").to_string();
            self.status = Status::Message(msg);
            return;
        }
        self.layers.touch();
        let count = self
            .layers
            .node(ed.layer_id)
            .and_then(|n| n.layer())
            .map(|l| l.feature_count)
            .unwrap_or(0);
        ed.draft.clear();
        ed.cursor = None;
        ed.values = vec![String::new(); ed.fields.len()];
        ed.dirty = true;
        self.dirty = true;
        self.editing = Some(ed);
        let count = count.to_string();
        self.status = Status::Message(self.tf("digitize.added", &[&count]));
    }

    /// 撤销最后一个节点（对标 `QgsMapToolCapture::undo`：已确认节点 ≤ 1 时
    /// 直接返回——QGIS 里 `size() <= 1` 短路，第一个节点只能 Esc 重来）。
    fn undo(&mut self) {
        let Some(ed) = self.editing.as_mut() else {
            return;
        };
        if ed.draft.len() <= 1 {
            return;
        }
        ed.draft.pop();
    }

    /// 放弃当前草图（对标 `QgsMapToolCapture::stopCapturing`：清掉采集曲线与
    /// 橡皮筋；已完成的要素不受影响，编辑会话保持）。
    fn stop_capturing(&mut self) {
        if let Some(ed) = self.editing.as_mut() {
            ed.draft.clear();
        }
    }

    /// 草图的实时预览：两块橡皮筋，配色取 QGIS 数字化设置的默认值
    /// （`qgssettingsregistrycore.cpp`：线色 rgb(255,0,0) alpha 200、
    /// 填充 alpha 30、临时带 alpha ×0.75）：
    /// - **主橡皮筋**（`mRubberBand`）：已确认节点的环——面要素闭合显示 +
    ///   半透明填充（`QgsRubberBand` 对 Polygon 类型自动闭合），线要素为折线；
    /// - **临时橡皮筋**（`mTempRubberBand`，`createCurveRubberBand`：虚线、
    ///   alpha 缩放）：最后一个节点连到光标；面要素再从光标连回第一个节点
    ///   （`tempCurve->close()` 的闭合预览）。
    pub(super) fn paint_draft(
        &self,
        painter: &egui::Painter,
        rect: egui::Rect,
        view: &map::MapView,
    ) {
        let Some(ed) = self.editing.as_ref() else {
            return;
        };
        if ed.draft.is_empty() {
            return;
        }
        // 预乘 alpha 的写法（该 egui 版本里只有 premultiplied 是 const fn）：
        // 255·(200/255)=200、255·(150/255)=150、255·(30/255)=30，颜色不变。
        const STROKE_COLOR: egui::Color32 = egui::Color32::from_rgba_premultiplied(200, 0, 0, 200);
        const TEMP_STROKE_COLOR: egui::Color32 =
            egui::Color32::from_rgba_premultiplied(150, 0, 0, 150);
        const FILL_COLOR: egui::Color32 = egui::Color32::from_rgba_premultiplied(30, 0, 0, 30);
        // QGIS 数字化默认线宽为 1（`settingsDigitizingLineWidth`）。
        let stroke = egui::Stroke::new(1.2_f32, STROKE_COLOR);
        let temp_stroke = egui::Stroke::new(1.2_f32, TEMP_STROKE_COLOR);

        let nodes: Vec<egui::Pos2> = ed
            .draft
            .iter()
            .map(|p| map::world_to_screen(*p, rect, view))
            .collect();

        // 主橡皮筋：已确认节点。面的填充不走 `PathShape`（egui 的路径填充遇到
        // 自相交环会把交叉面积糊成一整块），而是与最终渲染同源——偶奇拆分 +
        // earcut 三角片，交叉路径也只填「有效面积」（QGIS 对无效面的偶奇效果）。
        if ed.geom == GeometryKind::Polygon && nodes.len() >= 3 {
            let mut closed = ed.draft.clone();
            if let (Some(first), Some(last)) = (closed.first().copied(), closed.last().copied())
                && first != last
            {
                closed.push(first);
            }
            let parts = crate::geo::tri::polygon_parts(&closed, &[]);
            let (outers, hole_sets): (Vec<_>, Vec<_>) = parts
                .iter()
                .map(|(o, h)| (o.clone(), h.clone()))
                .unzip();
            for t in crate::geo::tri::triangulate_polygons(&outers, &hole_sets) {
                let a = map::world_to_screen([t[0], t[1]], rect, view);
                let b = map::world_to_screen([t[2], t[3]], rect, view);
                let c = map::world_to_screen([t[4], t[5]], rect, view);
                painter.add(egui::Shape::convex_polygon(
                    vec![a, b, c],
                    FILL_COLOR,
                    egui::Stroke::NONE,
                ));
            }
            // 描边：闭合一圈（QGIS rubber band 对面要素显示闭合环）。
            painter.add(egui::Shape::Path(egui::epaint::PathShape {
                points: nodes.clone(),
                closed: true,
                fill: egui::Color32::TRANSPARENT,
                stroke: stroke.into(),
            }));
        } else if nodes.len() >= 2 {
            painter.add(egui::Shape::line(nodes.clone(), stroke));
        }

        // 临时橡皮筋：最后节点 → 光标（面再补 光标 → 第一节点 的闭合段）。
        if let (Some(cursor), Some(last)) = (ed.cursor, nodes.last().copied()) {
            let c = map::world_to_screen(cursor, rect, view);
            painter.add(egui::Shape::line(vec![last, c], temp_stroke));
            if ed.geom == GeometryKind::Polygon
                && let Some(first) = nodes.first().copied()
            {
                painter.add(egui::Shape::line(vec![c, first], temp_stroke));
            }
        }

        // 节点标记：白底方块 + 红框（QGIS rubber band 的顶点方块）；
        // 最后一个是「当前节点」，描边加粗。
        let last = nodes.len().saturating_sub(1);
        for (i, p) in nodes.iter().enumerate() {
            let r = egui::Rect::from_center_size(*p, egui::vec2(8.0, 8.0));
            painter.rect_filled(r, 1.0, style::WHITE);
            painter.rect_stroke(
                r,
                1.0,
                egui::Stroke::new(if i == last { 2.0_f32 } else { 1.2_f32 }, STROKE_COLOR),
                egui::StrokeKind::Inside,
            );
        }
    }

    // -----------------------------------------------------------------------
    // 编辑面板（浮窗）
    // -----------------------------------------------------------------------

    /// 「编辑」浮窗：属性输入 + 完成 / 撤销 / 保存 / 停止编辑。
    pub(super) fn show_edit_panel(&mut self, ctx: &Context) {
        if self.editing.is_none() {
            return;
        }
        let layer_id = self.editing.as_ref().unwrap().layer_id;
        let Some(layer) = self.layers.node(layer_id).and_then(|n| n.layer()) else {
            // 图层被删了：会话失去目标，直接退出编辑。
            self.editing = None;
            self.map.tool = map::Tool::Pan;
            return;
        };
        let name = layer.name.clone();
        let count = layer.feature_count;
        let (fields, draft_len, dirty, geom) = {
            let ed = self.editing.as_ref().unwrap();
            (ed.fields.clone(), ed.draft.len(), ed.dirty, ed.geom)
        };
        let title = self.tf("digitize.title", &[&name]);
        let hint = self.t("digitize.hint").to_string();
        let attrs_label = self.t("digitize.attrs").to_string();
        let vertices = self.tf("digitize.vertices", &[&draft_len.to_string()]);
        let finish = self.t("digitize.finish").to_string();
        let undo = self.t("digitize.undoVertex").to_string();
        let discard = self.t("digitize.discard").to_string();
        let save = self.t("digitize.save").to_string();
        let stop = self.t("digitize.stop").to_string();
        let features = self.tf("digitize.features", &[&count.to_string()]);
        let state = if dirty {
            self.t("digitize.unsaved").to_string()
        } else {
            self.t("digitize.saved").to_string()
        };
        let saving = self.save_job.is_some();
        let kind_label = self
            .t(match geom {
                GeometryKind::Point => "newvector.geom.point",
                GeometryKind::Line => "newvector.geom.line",
                GeometryKind::Polygon => "newvector.geom.polygon",
            })
            .to_string();

        let mut finish_clicked = false;
        let mut undo_clicked = false;
        let mut discard_clicked = false;
        let mut save_clicked = false;
        let mut stop_clicked = false;
        egui::Window::new(title)
            .id(egui::Id::new("vector_edit_panel"))
            .collapsible(false)
            .resizable(false)
            .default_pos(egui::pos2(320.0, 120.0))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.weak(kind_label.clone());
                    ui.separator();
                    ui.weak(features.clone());
                    ui.separator();
                    ui.colored_label(
                        if dirty { style::DANGER } else { style::TEXT_WEAK },
                        state.clone(),
                    );
                });
                ui.label(hint.as_str());
                ui.separator();
                ui.strong(&attrs_label);
                // 属性输入：每个字段一行（与 QGIS 新建要素时弹的属性表单同构）。
                let ed = self.editing.as_mut().unwrap();
                if ed.values.len() != fields.len() {
                    ed.values = vec![String::new(); fields.len()];
                }
                for (i, field) in fields.iter().enumerate() {
                    ui.horizontal(|ui| {
                        let w = 90.0;
                        form_label(ui, w, &field.name);
                        ui.add(
                            TextEdit::singleline(&mut ed.values[i])
                                .desired_width(ui.available_width().max(60.0))
                                .hint_text(match field.kind {
                                    FieldKind::Integer | FieldKind::Real => "0",
                                    FieldKind::Date => "2026-01-01",
                                    FieldKind::Text => "",
                                }),
                        );
                    });
                }
                ui.separator();
                ui.weak(&vertices);
                ui.horizontal(|ui| {
                    finish_clicked = ui.button(&finish).clicked();
                    undo_clicked = ui.button(&undo).clicked();
                    discard_clicked = ui.button(&discard).clicked();
                });
                ui.separator();
                ui.horizontal(|ui| {
                    save_clicked = ui.add_enabled(!saving, egui::Button::new(&save)).clicked();
                    stop_clicked = ui.button(&stop).clicked();
                });
            });

        if finish_clicked {
            self.finish_feature();
        }
        if undo_clicked {
            self.undo();
        }
        if discard_clicked {
            self.stop_capturing();
        }
        if save_clicked {
            self.save_editing(false);
        }
        if stop_clicked {
            self.stop_editing();
        }
    }
}

// ---------------------------------------------------------------------------
// 独立窗口的 UI 回调
// ---------------------------------------------------------------------------

/// 对话框的界面（eframe 每帧为这个视口调用一次）。
fn new_vector_window_ui(
    ctx: &Context,
    state: &Mutex<NewVectorDialog>,
    msgs: &Mutex<Vec<NewVectorMsg>>,
) {
    let mut d = state.lock().unwrap_or_else(|e| e.into_inner());
    let texts = std::mem::take(&mut d.texts);

    // 文件名：系统保存对话框跑在后台线程，结果下一帧回填。
    if let Some(rx) = d.browse.as_ref()
        && let Ok(Some(path)) = rx.try_recv()
    {
        d.path = path.display().to_string();
        d.browse = None;
    }

    let mut accept = None;
    let mut cancel = false;
    egui::CentralPanel::default().show(ctx, |ui| {
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.heading(&texts.title);
            ui.separator();

            // ── 文件名 / 格式 ──
            ui.horizontal(|ui| {
                form_label(ui, 90.0, &texts.file);
                ui.add(
                    TextEdit::singleline(&mut d.path)
                        .desired_width(ui.available_width() - 80.0)
                        .hint_text("..."),
                );
                if ui.small_button(&texts.browse).clicked() && d.browse.is_none() {
                    let (tx, rx) = mpsc::channel();
                    d.browse = Some(rx);
                    let default_name = format!("new_layer.{}", d.format.extension());
                    let filters: Vec<&str> = d.format.filters().to_vec();
                    std::thread::spawn(move || {
                        let picked = rfd::FileDialog::new()
                            .set_file_name(&default_name)
                            .add_filter("Vector", &filters)
                            .save_file();
                        let _ = tx.send(picked);
                    });
                }
            });
            ui.horizontal(|ui| {
                form_label(ui, 90.0, &texts.format);
                egui::ComboBox::from_id_salt("newvector_format")
                    .selected_text(match d.format {
                        VectorFormat::Shapefile => &texts.fmt_shapefile,
                        VectorFormat::GeoJson => &texts.fmt_geojson,
                    })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut d.format,
                            VectorFormat::Shapefile,
                            &texts.fmt_shapefile,
                        );
                        ui.selectable_value(
                            &mut d.format,
                            VectorFormat::GeoJson,
                            &texts.fmt_geojson,
                        );
                    });
            });

            // ── 几何类型 / 坐标系 ──
            ui.horizontal(|ui| {
                form_label(ui, 90.0, &texts.geom);
                let label = match d.geom {
                    Some(GeometryKind::Point) => &texts.point,
                    Some(GeometryKind::Line) => &texts.line,
                    Some(GeometryKind::Polygon) => &texts.polygon,
                    None => "",
                };
                egui::ComboBox::from_id_salt("newvector_geom")
                    .selected_text(label)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut d.geom,
                            Some(GeometryKind::Point),
                            &texts.point,
                        );
                        ui.selectable_value(&mut d.geom, Some(GeometryKind::Line), &texts.line);
                        ui.selectable_value(
                            &mut d.geom,
                            Some(GeometryKind::Polygon),
                            &texts.polygon,
                        );
                    });
            });
            ui.horizontal(|ui| {
                form_label(ui, 90.0, &texts.crs);
                let label = match d.crs {
                    CrsChoice::Follow => &texts.crs_follow,
                    CrsChoice::Wgs84 => &texts.crs_wgs84,
                    CrsChoice::Cgcs2000 => &texts.crs_cgcs2000,
                    CrsChoice::WebMercator => &texts.crs_web,
                };
                egui::ComboBox::from_id_salt("newvector_crs")
                    .selected_text(label)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut d.crs, CrsChoice::Follow, &texts.crs_follow);
                        ui.selectable_value(&mut d.crs, CrsChoice::Wgs84, &texts.crs_wgs84);
                        ui.selectable_value(&mut d.crs, CrsChoice::Cgcs2000, &texts.crs_cgcs2000);
                        ui.selectable_value(&mut d.crs, CrsChoice::WebMercator, &texts.crs_web);
                    });
            });

            // ── 新字段 ──
            ui.add_space(6.0);
            ui.separator();
            ui.strong(texts.new_field.as_str());
            // 分两行摆：QGIS 是一行塞五个控件，但 egui 的 `horizontal` 不会换行，
            // 窗口窄于 ~660px 时这一行会横向溢出、把后面按钮行的可用宽度挤成零
            // （egui 于是整行不渲染）——实测 560px 宽时「确定 / 取消」直接消失。
            ui.horizontal(|ui| {
                form_label(ui, 52.0, &texts.name);
                ui.add(TextEdit::singleline(&mut d.field_name).desired_width(160.0));
                form_label(ui, 40.0, &texts.kind);
                let kind_label = match d.field_kind {
                    FieldKind::Text => &texts.kind_text,
                    FieldKind::Integer => &texts.kind_int,
                    FieldKind::Real => &texts.kind_real,
                    FieldKind::Date => &texts.kind_date,
                };
                egui::ComboBox::from_id_salt("newvector_field_kind")
                    .selected_text(kind_label)
                    .show_ui(ui, |ui| {
                        for (kind, label) in [
                            (FieldKind::Text, &texts.kind_text),
                            (FieldKind::Integer, &texts.kind_int),
                            (FieldKind::Real, &texts.kind_real),
                            (FieldKind::Date, &texts.kind_date),
                        ] {
                            if ui
                                .selectable_label(d.field_kind == kind, label)
                                .clicked()
                                && d.field_kind != kind
                            {
                                d.field_kind = kind;
                                // 切类型时长度 / 精度回落到该类型的默认值（同 QGIS）。
                                let (l, p) = kind.default_metrics();
                                d.length_text = l.to_string();
                                d.precision_text = p.to_string();
                            }
                        }
                    });
            });
            ui.horizontal(|ui| {
                let (length_on, precision_on) = d.field_kind.editable();
                form_label(ui, 52.0, &texts.length);
                ui.add_enabled(
                    length_on,
                    TextEdit::singleline(&mut d.length_text).desired_width(60.0),
                );
                form_label(ui, 40.0, &texts.precision);
                ui.add_enabled(
                    precision_on,
                    TextEdit::singleline(&mut d.precision_text).desired_width(60.0),
                );
                ui.add_space(8.0);
                if ui.button(&texts.add_field).clicked() {
                    d.add_field();
                }
            });

            // ── 字段列表 ──
            ui.add_space(6.0);
            ui.strong(texts.fields.as_str());
            let mut remove = None;
            let mut swap = None;
            let mut selected = d.selected;
            egui::Grid::new("newvector_field_list")
                .num_columns(4)
                .striped(true)
                .min_col_width(64.0)
                .show(ui, |ui| {
                    ui.strong(texts.name.as_str());
                    ui.strong(texts.kind.as_str());
                    ui.strong(texts.length.as_str());
                    ui.strong(texts.precision.as_str());
                    ui.end_row();
                    for (i, field) in d.fields.iter().enumerate() {
                        if ui
                            .selectable_label(selected == Some(i), &field.name)
                            .clicked()
                        {
                            selected = Some(i);
                        }
                        ui.label(match field.kind {
                            FieldKind::Text => &texts.kind_text,
                            FieldKind::Integer => &texts.kind_int,
                            FieldKind::Real => &texts.kind_real,
                            FieldKind::Date => &texts.kind_date,
                        });
                        ui.label(field.length.to_string());
                        ui.label(field.precision.to_string());
                        ui.end_row();
                    }
                });
            d.selected = selected;
            ui.horizontal(|ui| {
                let has = d.selected.is_some_and(|i| i < d.fields.len());
                if ui.add_enabled(has, egui::Button::new("↑")).on_hover_text(texts.up.as_str()).clicked()
                    && let Some(i) = d.selected
                    && i > 0
                {
                    swap = Some((i, i - 1));
                }
                if ui.add_enabled(has, egui::Button::new("↓")).on_hover_text(texts.down.as_str()).clicked()
                    && let Some(i) = d.selected
                    && i + 1 < d.fields.len()
                {
                    swap = Some((i, i + 1));
                }
                if ui
                    .add_enabled(has, egui::Button::new(&texts.remove))
                    .clicked()
                {
                    remove = d.selected;
                }
            });
            if let Some(i) = remove
                && i < d.fields.len()
            {
                d.fields.remove(i);
                d.selected = None;
            }
            if let Some((a, b)) = swap {
                d.fields.swap(a, b);
                d.selected = Some(b);
            }

            // ── 校验提示 ──
            if let Some(error) = &d.error {
                ui.add_space(6.0);
                ui.colored_label(style::DANGER, error);
            }

            // ── 新建 / 取消 ──
            ui.add_space(10.0);
            ui.separator();
            // 显式给出行矩形（`max_rect` 而不是 `available_rect`）：上面任何一行
            // 横向溢出都会把 `available_rect` 挤窄甚至挤成零宽，按钮就整行不渲染
            // （560px 宽窗口下实测「确定 / 取消」会凭空消失）。用 `max_rect` 锚定
            // 后，无论上面多宽，按钮都稳稳落在面板底部右侧。
            ui.allocate_ui_with_layout(
                egui::vec2(ui.max_rect().width(), 34.0),
                egui::Layout::right_to_left(egui::Align::Center),
                |ui| {
                    if ui.button(&texts.cancel).clicked() {
                        cancel = true;
                    }
                    if ui.button(&texts.create).clicked() {
                        match d.request() {
                            Ok(msg) => accept = Some(msg),
                            Err(e) => d.error = Some(e),
                        }
                    }
                },
            );
        });
    });
    d.texts = texts;

    if let Some(msg) = accept {
        msgs.lock().unwrap_or_else(|e| e.into_inner()).push(msg);
    }
    if cancel {
        msgs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(NewVectorMsg::Cancel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geo::vector_file::FieldDef;
    use egui::Context;

    /// 无头跑一帧界面（`Context::default()` 时 egui 把视口内嵌 ⇒ 回调当场执行，
    /// 因此对话框与编辑面板的绘制路径在测试里也走一遍）。
    fn frame(app: &mut RustGisApp, ctx: &Context) {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(1280.0, 900.0),
            )),
            ..Default::default()
        };
        let _ = ctx.run(input, |ctx| {
            app.show_new_vector_dialog(ctx);
            app.show_edit_panel(ctx);
        });
    }

    fn temp_path(tag: &str, ext: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("rustgis_newvector_ui_{}_{}.{}", std::process::id(), tag, ext));
        p
    }

    /// 表单校验：文件名 / 几何类型 / 未加入列表的字段名，各自的拦截顺序。
    #[test]
    fn request_reports_each_invalid_field() {
        let ctx = Context::default();
        let app = RustGisApp::new(&ctx);
        let mut d = NewVectorDialog::default();
        d.texts = Texts::new(&app);

        assert!(d.request().is_err(), "没填文件名应失败");
        d.path = "a.shp".to_string();
        assert!(d.request().is_err(), "没选几何类型应失败");
        d.geom = Some(GeometryKind::Polygon);
        assert!(d.request().is_ok(), "默认 id 字段 + 面 ⇒ 合法");

        // 输入框里有没加进列表的字段名：与 QGIS 的 accept() 一样先拦下来。
        d.field_name = "备注".to_string();
        assert!(d.request().is_err());
        d.add_field();
        assert_eq!(d.fields.len(), 2);
        assert!(d.field_name.is_empty(), "加完应清空输入框");
        assert!(d.request().is_ok());

        // 重名 / 空名 各自报错。
        d.field_name = "id".to_string();
        d.add_field();
        assert!(d.error.as_ref().is_some_and(|e| e.contains("id")));
        d.field_name.clear();
        d.add_field();
        assert!(d.error.as_ref().is_some());
        assert_eq!(d.fields.len(), 2);
    }

    /// 打开 → 渲染一帧 → 投 Accept → 图层进入内容列表并开始编辑。
    #[test]
    fn accept_creates_layer_and_enters_editing() {
        let ctx = Context::default();
        let mut app = RustGisApp::new(&ctx);
        app.catalog.location = std::env::temp_dir();
        let path = temp_path("accept", "shp");

        app.open_new_vector_dialog();
        let (state, msgs) = {
            let w = app.new_vector.as_ref().expect("对话框应已打开");
            (Arc::clone(&w.state), Arc::clone(&w.msgs))
        };
        {
            let mut d = state.lock().unwrap();
            assert_eq!(d.fields.len(), 1, "默认预置一个 id 字段（同 QGIS）");
            assert_eq!(d.fields[0].name, "id");
            d.path = path.display().to_string();
            d.geom = Some(GeometryKind::Polygon);
            d.crs = CrsChoice::Wgs84;
        }
        frame(&mut app, &ctx); // 走一遍窗口 UI（下拉框 / 字段列表 / 按钮）
        assert!(app.new_vector.is_some(), "渲染后窗口仍在");

        msgs.lock().unwrap().push(NewVectorMsg::Accept {
            path: path.clone(),
            format: VectorFormat::Shapefile,
            geom: GeometryKind::Polygon,
            crs: CrsChoice::Wgs84,
            fields: vec![FieldDef::default_id()],
        });
        frame(&mut app, &ctx);

        assert!(app.new_vector.is_none(), "确定后窗口关闭");
        let ed = app.editing.as_ref().expect("应进入编辑");
        assert_eq!(ed.geom, GeometryKind::Polygon);
        assert_eq!(app.map.tool, map::Tool::Edit);
        let layer = app
            .layers
            .node(ed.layer_id)
            .and_then(|n| n.layer())
            .expect("图层应在内容列表里");
        assert_eq!(layer.fields, vec!["id".to_string()]);
        assert_eq!(layer.feature_count, 0);
        assert_eq!(layer.projection.crs, crate::geo::CrsId::Epsg(4326));
        assert_eq!(app.project_projection.as_ref().unwrap().crs, layer.projection.crs);

        // 0 要素不落盘（`save_editing` 会跳过），此时目录栏里还没有文件。
        wait_saved(&mut app, &ctx);
        assert!(!path.exists(), "空数据集不该抢先写出（shape type 未定）");

        // 画一个面再保存 ⇒ 数据集出现，且能被自己的导入路径原样读回。
        for p in [[0.0, 0.0], [10.0, 0.0], [10.0, 10.0]] {
            app.add_vertex(p);
        }
        app.finish_feature();
        app.save_editing(false);
        wait_saved(&mut app, &ctx);
        assert!(path.exists(), "画完要素后应写出 .shp");
        assert!(path.with_extension("prj").exists(), "应写出 .prj");
        let back = crate::geo::load_file(&path).expect("写出的 Shapefile 应能读回");
        assert_eq!(back.feature_count, 1);
        assert_eq!(back.fields, vec!["id".to_string()]);
        for ext in ["shp", "shx", "dbf", "prj"] {
            let _ = std::fs::remove_file(path.with_extension(ext));
        }
    }

    /// 数字化：点三个点 → 完成要素 → 几何 / 属性 / 三角片都进图层；保存后能读回。
    #[test]
    fn digitize_adds_feature_and_saves_file() {
        let ctx = Context::default();
        let mut app = RustGisApp::new(&ctx);
        let path = temp_path("digitize", "shp");
        app.create_vector_layer(
            path.clone(),
            VectorFormat::Shapefile,
            GeometryKind::Polygon,
            CrsChoice::Wgs84,
            vec![
                FieldDef::default_id(),
                FieldDef {
                    name: "name".to_string(),
                    kind: FieldKind::Text,
                    length: 20,
                    precision: 0,
                },
            ],
        );
        let id = app.editing.as_ref().unwrap().layer_id;
        app.editing.as_mut().unwrap().values = vec!["7".to_string(), "地块A".to_string()];

        // 节点不足 3 个时不该成形；右键结束路径与 QGIS 一样直接丢弃草图。
        app.add_vertex([0.0, 0.0]);
        app.finish_feature();
        assert_eq!(
            app.layers.node(id).unwrap().layer().unwrap().feature_count,
            0,
            "面要素不足 3 个节点不应成形"
        );
        assert!(app.editing.as_ref().unwrap().draft.is_empty(), "不足节点数的草图应被丢弃");
        // 重新画一个三角（QGIS：草图被丢弃后从零开始，不会残留旧节点）。
        app.add_vertex([10.0, 0.0]);
        app.add_vertex([10.0, 10.0]);
        app.add_vertex([0.0, 10.0]);
        app.finish_feature();

        let layer = app.layers.node(id).unwrap().layer().unwrap();
        assert_eq!(layer.feature_count, 1);
        assert_eq!(layer.outer_rings.len(), 1);
        assert_eq!(layer.triangles.len(), 1, "三角形应剖成 1 个三角");
        assert_eq!(layer.attrs, vec![vec!["7".to_string(), "地块A".to_string()]]);
        assert!(app.editing.as_ref().unwrap().draft.is_empty());
        assert!(app.editing.as_ref().unwrap().dirty, "画完应标记为未保存");

        // 撤点（QGIS：只剩 1 个节点时撤点短路）/ 放弃草图。
        app.add_vertex([0.0, 0.0]);
        app.undo();
        assert_eq!(app.editing.as_ref().unwrap().draft.len(), 1, "单个节点不应被撤掉");
        app.add_vertex([5.0, 5.0]);
        app.undo();
        assert_eq!(app.editing.as_ref().unwrap().draft.len(), 1, "撤掉的是最后一个节点");
        app.stop_capturing();
        assert!(app.editing.as_ref().unwrap().draft.is_empty());

        app.save_editing(false);
        wait_saved(&mut app, &ctx);
        assert!(!app.editing.as_ref().unwrap().dirty, "保存后应清掉未保存标记");
        // 落盘结果能被生产代码的导入路径原样读回。
        let back = crate::geo::load_file(&path).expect("写出的 Shapefile 应能读回");
        assert_eq!(back.feature_count, 1);
        let name_col = back.fields.iter().position(|f| f == "name").unwrap();
        assert_eq!(back.attrs[0][name_col], "地块A");
        for ext in ["shp", "shx", "dbf", "prj"] {
            let _ = std::fs::remove_file(path.with_extension(ext));
        }
    }

    /// 数字化路径交叉（蝴蝶结）：结束要素后仍是一个要素，但拆成两瓣简单环，
    /// 三角片只覆盖有效面积（与 QGIS 对无效面的偶奇渲染一致）。
    #[test]
    fn self_intersecting_draft_splits_into_parts() {
        let ctx = Context::default();
        let mut app = RustGisApp::new(&ctx);
        app.create_vector_layer(
            PathBuf::from("unused_self_intersect.geojson"),
            VectorFormat::GeoJson,
            GeometryKind::Polygon,
            CrsChoice::Follow,
            vec![FieldDef::default_id()],
        );
        // 蝴蝶结：路径在 (5,5) 交叉，两瓣各 25 平方单位。
        for p in [[0.0, 0.0], [10.0, 10.0], [10.0, 0.0], [0.0, 10.0]] {
            app.add_vertex(p);
        }
        app.finish_feature();

        let layer = app
            .layers
            .node(app.editing.as_ref().unwrap().layer_id)
            .unwrap()
            .layer()
            .unwrap();
        assert_eq!(layer.feature_count, 1, "仍是一个要素");
        assert_eq!(layer.attrs.len(), 1);
        assert_eq!(layer.outer_rings.len(), 2, "蝴蝶结应拆成两瓣");
        assert_eq!(layer.ring_rows, vec![0, 0], "两瓣共用一行属性");
        let area: f64 = layer
            .triangles
            .iter()
            .map(|t| ((t[2] - t[0]) * (t[5] - t[1]) - (t[4] - t[0]) * (t[3] - t[1])).abs() / 2.0)
            .sum();
        assert!((area - 50.0).abs() < 1e-9, "area = {area}");
        assert!(app.editing.as_ref().unwrap().draft.is_empty());
    }

    /// 停止编辑：干净状态下直接退出，工具退回平移。
    #[test]
    fn stop_editing_returns_to_pan_tool() {
        let ctx = Context::default();
        let mut app = RustGisApp::new(&ctx);
        let path = temp_path("stop", "geojson");
        app.create_vector_layer(
            path.clone(),
            VectorFormat::GeoJson,
            GeometryKind::Point,
            CrsChoice::Follow,
            vec![FieldDef::default_id()],
        );
        app.editing.as_mut().unwrap().dirty = false;
        app.stop_editing();
        assert!(app.editing.is_none());
        assert_eq!(app.map.tool, map::Tool::Pan);
        let _ = std::fs::remove_file(&path);
    }

    /// 本帧画出的所有文字（含嵌套 `Shape::Vec`），用于断言某个控件真的渲染了。
    fn painted_texts(shapes: &[egui::epaint::ClippedShape]) -> Vec<String> {
        let mut out = Vec::new();
        for cs in shapes {
            match &cs.shape {
                egui::epaint::Shape::Text(t) => out.push(t.galley.text().to_string()),
                egui::epaint::Shape::Vec(v) => {
                    for inner in v {
                        if let egui::Shape::Text(t) = inner {
                            out.push(t.galley.text().to_string());
                        }
                    }
                }
                _ => {}
            }
        }
        out
    }

    /// 回归：窗口只有 560×620 时「新建 / 取消」也必须画出来。
    ///
    /// 「新字段」那一行原本把 名称/类型/长度/精度/按钮 全塞进一个 `horizontal`，
    /// 窗口窄于 ~660px 就横向溢出，把随后按钮行的 `available_rect` 挤成零宽，
    /// egui 于是整行不渲染（用户截图里底部按钮凭空消失）。现在新字段拆两行 +
    /// 按钮行用 `max_rect` 显式锚定。
    #[test]
    fn buttons_render_in_narrow_window() {
        // 默认尺寸与窗口被缩到最小尺寸，两种都不能把按钮行挤掉。
        for (w, h) in [(560.0, 620.0), (MIN_WINDOW_WIDTH, MIN_WINDOW_HEIGHT)] {
            let ctx = Context::default();
            let mut app = RustGisApp::new(&ctx);
            app.catalog.location = std::env::temp_dir();
            let create = app.language.t("newvector.create").to_string();
            let cancel = app.language.t("dialog.cancel").to_string();
            app.open_new_vector_dialog();
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::pos2(0.0, 0.0),
                    egui::vec2(w, h),
                )),
                ..Default::default()
            };
            let output = ctx.run(input, |ctx| app.show_new_vector_dialog(ctx));
            let texts = painted_texts(&output.shapes);
            assert!(
                texts.iter().any(|t| *t == create),
                "{w}x{h}：「{create}」按钮必须渲染，实际：{texts:?}"
            );
            assert!(
                texts.iter().any(|t| *t == cancel),
                "{w}x{h}：「{cancel}」按钮必须渲染，实际：{texts:?}"
            );
        }
    }

    /// 等后台保存任务结束（主线程的非阻塞轮询，测试里同样能推进）。
    fn wait_saved(app: &mut RustGisApp, ctx: &Context) {
        for _ in 0..500 {
            if app.save_job.is_none() {
                return;
            }
            app.poll_save_job(ctx);
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("保存任务未在预期时间内结束");
    }
}
