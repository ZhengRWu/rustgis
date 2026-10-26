//! 图层导入后台流水线：GeoJSON / Shapefile → 新图层。
//!
//! **导入全程不占用 UI 线程**（与工程流水线同款约定，见 [`super::project_op`]）：
//! - 第一阶段：文件对话框在后台线程弹出并阻塞该线程，egui 主线程持续渲染；
//! - 第二阶段：解析文件、展开几何、三角剖分同样在后台线程完成；
//! - 主线程每帧非阻塞轮询（[`RustGisApp::poll_import`]），进行中用 50ms 定时唤醒。
//!
//! 旧实现只把文件对话框挪到了后台，解析与三角化仍留在 `update()` 里同步执行，
//! 于是稍有规模的多边形（数万顶点）就会让整个窗口卡死——这才是卡死的根因。

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, TryRecvError};

use egui::Context;

use crate::dialog::Dialog;
use crate::geo;

use super::{RustGisApp, Status};

/// 后台导入流水线：先弹出系统文件对话框取路径，再把解析 / 几何展开 / 三角剖分
/// 放到后台线程完成。两个阶段都不占用 UI 线程，因此多复杂的图层都不会锁住窗口。
pub(super) enum ImportStage {
    /// 等待用户在弹出的系统文件对话框中作出选择。
    Choosing(mpsc::Receiver<Option<PathBuf>>),
    /// 已选定文件，后台正在解析、展开几何、三角化。
    Loading {
        rx: mpsc::Receiver<Result<geo::Layer, String>>,
        name: String,
    },
}

impl RustGisApp {
    /// 系统文件选择器 → 后台导入为新图层 → 视图自适应。
    ///
    /// 「添加数据」是混合入口：矢量（GeoJSON / Shapefile）走本模块的解析流水线，
    /// **栅格（.tif / .tiff）按扩展名转交** [`super::raster_op`] 的栅格流水线，
    /// 这样在同一个对话框里选 TIF 不会报「不支持的文件格式」。
    pub(super) fn pick_and_add_layer(&mut self) {
        if !self.project_idle() {
            // 导入或工程任务（含系统"保存更改？"提示）进行中：忽略重复点击。
            // 否则第二个系统对话框会抢占焦点，且保存完成后因 project_op
            // 非空闲而 begin_save 被静默丢弃。
            return;
        }
        let title = self.t("dialog.addTitle").to_string();
        let (tx, rx) = mpsc::channel::<Option<PathBuf>>();
        std::thread::spawn(move || {
            let chosen = rfd::FileDialog::new()
                .set_title(&title)
                .add_filter("GeoJSON", &["geojson", "json"])
                .add_filter("Shapefile", &["shp"])
                .add_filter("GeoTIFF", &["tif", "tiff"])
                .pick_file();
            let _ = tx.send(chosen);
        });
        self.import = Some(ImportStage::Choosing(rx));
    }

    /// 推进后台导入流水线：选择文件 → 后台解析 → 加入图层并适配视图。
    /// 每帧调用一次，非阻塞。
    pub(super) fn poll_import(&mut self, ctx: &Context) {
        let Some(stage) = self.import.take() else {
            return;
        };
        match stage {
            ImportStage::Choosing(rx) => match rx.try_recv() {
                Ok(None) => {
                    // 用户在对话框中点了取消。
                }
                Ok(Some(path)) => {
                    // 混合入口：栅格文件交给栅格流水线（本帧不进入矢量解析）。
                    if super::raster_op::is_raster_path(&path) {
                        self.start_raster_open(path);
                    } else {
                        self.start_vector_import(path);
                    }
                }
                Err(TryRecvError::Empty) => {
                    self.import = Some(ImportStage::Choosing(rx)); // 用户仍在对话框中操作
                }
                Err(TryRecvError::Disconnected) => {
                    // 对话框线程异常退出（无结果可回收），静默放弃本次导入。
                }
            },
            ImportStage::Loading { rx, name } => match rx.try_recv() {
                Ok(Ok(layer)) => {
                    let count = layer.feature_count.to_string();
                    // 产品需求：工程未设置投影且尚无图层时，**第一个**导入
                    // 图层的投影 / 坐标系成为工程默认值（判定必须在 `add`
                    // **之前**——加入后 `is_empty` 不再为真）。首个图层无法
                    // 识别投影时，工程默认保持"未设置"（严格"首图层定默认"）。
                    self.adopt_project_default(&layer);
                    self.layers.add(layer);
                    self.needs_fit = true;
                    self.dirty = true; // 图层结构变化 → 工程有未保存更改
                    // 完成提示走右下角通知（状态栏只留"正在加载…"这类进行中文案）。
                    let body = self.tf("status.added", &[&name, &count]);
                    let title = self.t("toast.layerAdded").to_string();
                    let now = ctx.input(|input| input.time);
                    self.toasts.push(now, title, body);
                    // 结束态回到"就绪"：「正在加载数据…」不能一直挂在状态栏上。
                    self.status = Status::Ready;
                }
                Ok(Err(e)) => {
                    self.status = Status::Message(self.tf("status.loadError", &[&e]));
                    self.dialog = Some(Dialog::Error { body: e });
                }
                Err(TryRecvError::Empty) => {
                    self.import = Some(ImportStage::Loading { rx, name }); // 仍在解析
                }
                Err(TryRecvError::Disconnected) => {
                    let msg = self.t("status.loadAborted").to_string();
                    self.status = Status::Message(msg.clone());
                    self.dialog = Some(Dialog::Error { body: msg });
                }
            },
        }

        // egui 默认按需重绘（无输入就不调用 update）。导入进行中必须持续唤醒，
        // 否则后台线程回传的结果要等到下一次用户操作才被取回，窗口看起来会像卡住。
        // 用 20Hz 定时唤醒而非每帧狂转：既保证及时取回结果，也不空转 CPU。
        if self.import.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        }
    }

    /// 让指定路径进入矢量导入流水线：后台线程解析文件 → 展开几何 → 三角剖分。
    ///
    /// 供两个入口共用：「添加数据」混合对话框（见 [`Self::poll_import`]），
    /// 以及拖进窗口的文件（见 [`Self::handle_dropped_files`]）。
    pub(super) fn start_vector_import(&mut self, path: PathBuf) {
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        let (tx, load_rx) = mpsc::channel();
        std::thread::spawn(move || {
            // 解析 + 展开 + 三角剖分：全部留在后台线程。
            let _ = tx.send(geo::load_file(&path));
        });
        self.status = Status::Message(self.tf("status.loading", &[&name]));
        self.import = Some(ImportStage::Loading { rx: load_rx, name });
    }

    /// 收集拖进窗口的文件（egui 的 `raw.dropped_files`），入队后由
    /// [`Self::poll_drop_queue`] 逐个启动。
    ///
    /// 拖到窗口任何位置都算（地图区、图层面板都行，等同于工业软件里把数据
    /// 甩进主窗口）。不支持的扩展名只弹一条右下角通知，不打断已入队的导入。
    pub(super) fn handle_dropped_files(&mut self, ctx: &Context) {
        let dropped = ctx.input(|i| i.raw.dropped_files.clone());
        if dropped.is_empty() {
            return;
        }
        let mut rejected: Vec<String> = Vec::new();
        for file in dropped {
            // 只有 `path` 可用的拖放才支持（从浏览器等拖来的纯字节流没有本地路径）。
            let Some(path) = file.path else {
                rejected.push(file.name);
                continue;
            };
            if is_supported_drop(&path) {
                self.drop_queue.push_back(path);
            } else {
                rejected.push(file_display_name(&path));
            }
        }
        if !rejected.is_empty() {
            let body = self.tf("toast.dropUnsupported", &[&rejected.join("、")]);
            let title = self.t("toast.dropTitle").to_string();
            let now = ctx.input(|i| i.time);
            self.toasts.push(now, title, body);
        }
        // 入队后立即尝试启动，避免要等下一次用户输入（egui 按需重绘）。
        self.poll_drop_queue();
    }

    /// 每帧：队列非空且导入流水线空闲时，启动下一个拖放文件。
    ///
    /// 同一时刻只跑一条流水线（矢量导入或栅格打开），否则两条后台任务会
    /// 同时改写图层面板状态、完成通知也会交错。
    pub(super) fn poll_drop_queue(&mut self) {
        if self.drop_queue.is_empty() || self.import.is_some() || self.raster_stage.is_some() {
            return;
        }
        let Some(path) = self.drop_queue.pop_front() else {
            return;
        };
        if super::raster_op::is_raster_path(&path) {
            self.start_raster_open(path);
        } else {
            self.start_vector_import(path);
        }
    }

    /// 导入进行中时面板上的提示 key（空闲时为 `None`）。
    pub(super) fn import_hint(&self) -> Option<&'static str> {
        match self.import {
            Some(ImportStage::Choosing(_)) => Some("layers.loading"),
            Some(ImportStage::Loading { .. }) => Some("layers.parsing"),
            None => None,
        }
    }

    /// 若该图层是"工程首个可识别投影的导入图层"，则把其投影 / 坐标系设为
    /// 工程默认值（返回是否已设置），供 [`Self::poll_import`] 在
    /// `layers.add` **之前**调用（加入后 `is_empty` 不再为真）。
    ///
    /// 对应产品需求（m00001）：工程未设置投影 / 坐标系、且**尚无任何图层**时，
    /// **第一个**导入图层的投影 / 坐标系成为工程默认值。严格"首图层定默认"——
    /// 一旦加入首个图层，`is_empty` 即为假，后续图层不会再触发默认值设置。
    ///
    /// 成立条件（三者缺一不可）：
    /// 1. 工程尚未设置投影（`None`；`Some(none())` 视同未设置，仅防御旧文件）；
    /// 2. 工程里还没有任何图层（含空组——`is_empty` 对空组也算空）⇒ 本图层
    ///    即"第一个导入的图层"；
    /// 3. 该图层自身识别出了投影。首个图层无法识别（如缺 `.prj` 的 Shapefile）
    ///    时**不占用**默认值——工程默认保持"未设置"，而非存入无意义的 `none()`。
    pub(super) fn adopt_project_default(&mut self, layer: &geo::Layer) -> bool {
        if self
            .project_projection
            .as_ref()
            .is_none_or(|p| *p == geo::ProjectionInfo::none())
            && self.layers.is_empty()
            && layer.projection != geo::ProjectionInfo::none()
        {
            self.project_projection = Some(layer.projection.clone());
            true
        } else {
            false
        }
    }
}

/// 拖进窗口的文件是否受支持：栅格（.tif/.tiff/SAFE 目录）或矢量
/// （.geojson/.json/.shp），与两条导入流水线接受的格式保持一致。
pub(super) fn is_supported_drop(path: &Path) -> bool {
    if super::raster_op::is_raster_path(path) {
        return true;
    }
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| matches!(ext.to_ascii_lowercase().as_str(), "shp" | "geojson" | "json"))
}

/// 通知里展示的文件名：优先文件名，退化到完整路径（末尾没有文件名时）。
fn file_display_name(path: &Path) -> String {
    path.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn square_layer(projection: geo::ProjectionInfo) -> geo::Layer {
        let mut l = geo::Layer::from_geometry(
            "t".to_string(),
            Vec::new(),
            Vec::new(),
            vec![geo::PolygonData {
                outer: vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0], [0.0, 0.0]],
                holes: Vec::new(),
            }],
        );
        l.projection = projection;
        l
    }

    fn epsg(code: u32) -> geo::ProjectionInfo {
        geo::ProjectionInfo {
            model: geo::ProjectionModel::Projected,
            crs: geo::CrsId::Epsg(code),
        }
    }

    /// 产品需求（m00001）：空工程 + 未设置投影 → 首个图层定默认。
    #[test]
    fn first_layer_sets_project_default_on_empty_project() {
        let ctx = egui::Context::default();
        let mut app = RustGisApp::new(&ctx);
        let layer = square_layer(epsg(4549));
        assert!(app.adopt_project_default(&layer), "空工程应采纳首图层投影");
        assert_eq!(app.project_projection, Some(epsg(4549)));
        app.layers.add(layer);
    }

    /// 首个图层无法识别投影 → 不占用默认值（保持 `None`，而非存入 none()）。
    #[test]
    fn unidentifiable_first_layer_keeps_default_unset() {
        let ctx = egui::Context::default();
        let mut app = RustGisApp::new(&ctx);
        let layer = square_layer(geo::ProjectionInfo::none());
        assert!(
            !app.adopt_project_default(&layer),
            "无法识别投影的图层不应定默认"
        );
        app.layers.add(layer);
        assert!(app.project_projection.is_none());
    }

    /// 已有图层 → 后续导入不再触发默认值（严格"首图层定默认"）。
    #[test]
    fn second_layer_never_adopts_default() {
        let ctx = egui::Context::default();
        let mut app = RustGisApp::new(&ctx);
        app.layers.add(square_layer(epsg(4326)));
        app.project_projection = Some(epsg(4326));
        let second = square_layer(epsg(3857));
        assert!(
            !app.adopt_project_default(&second),
            "工程已有图层时不再改默认"
        );
        app.layers.add(second);
        assert_eq!(app.project_projection, Some(epsg(4326)));
    }

    /// 混合入口：在「添加数据」对话框里选 .tif 必须转交栅格流水线，
    /// 而不是落到矢量解析器报「不支持的文件格式：.tif」。
    #[test]
    fn add_data_routes_tiff_to_raster_pipeline() {
        let ctx = egui::Context::default();
        let mut app = RustGisApp::new(&ctx);
        let (tx, rx) = mpsc::channel();
        tx.send(Some(std::path::PathBuf::from("demo.tif"))).unwrap();
        app.import = Some(ImportStage::Choosing(rx));
        app.poll_import(&ctx);
        assert!(app.import.is_none(), "矢量流水线应已退出");
        assert!(
            matches!(
                app.raster_stage,
                Some(super::super::raster_op::RasterStage::Opening { .. })
            ),
            "应进入栅格打开流水线"
        );
        // 清理后台线程结果，避免测试结束时仍有未回收的消息。
        app.raster_stage = None;
    }

    /// 扩展名判断（大小写不敏感，仅 tif/tiff）。
    #[test]
    fn raster_extension_detection() {        use super::super::raster_op::is_raster_path;
        assert!(is_raster_path(std::path::Path::new("a.tif")));
        assert!(is_raster_path(std::path::Path::new("a.TIFF")));
        assert!(!is_raster_path(std::path::Path::new("a.geojson")));
        assert!(!is_raster_path(std::path::Path::new("a.shp")));
        assert!(!is_raster_path(std::path::Path::new("tif")));
    }

    /// 拖放接受的文件类型与两条导入流水线一致（栅格 + 矢量）。
    #[test]
    fn drop_accepts_raster_and_vector_extensions() {
        assert!(is_supported_drop(std::path::Path::new("a.tif")));
        assert!(is_supported_drop(std::path::Path::new("a.TIFF")));
        assert!(is_supported_drop(std::path::Path::new("a.SHP")));
        assert!(is_supported_drop(std::path::Path::new("a.geojson")));
        assert!(is_supported_drop(std::path::Path::new("a.json")));
        assert!(!is_supported_drop(std::path::Path::new("a.docx")));
        assert!(!is_supported_drop(std::path::Path::new("readme")));
    }

    /// 拖放队列：同一时刻只跑一条流水线，前一个文件结束后才启动下一个。
    #[test]
    fn drop_queue_feeds_one_pipeline_at_a_time() {
        let ctx = egui::Context::default();
        let mut app = RustGisApp::new(&ctx);
        app.drop_queue
            .push_back(std::path::PathBuf::from("demo.tif"));
        app.drop_queue
            .push_back(std::path::PathBuf::from("demo.geojson"));

        app.poll_drop_queue();
        assert!(
            matches!(
                app.raster_stage,
                Some(super::super::raster_op::RasterStage::Opening { .. })
            ),
            "第一个文件（栅格）应进入栅格打开流水线"
        );
        assert_eq!(app.drop_queue.len(), 1, "同一时刻只启动一条流水线");

        // 栅格流水线未结束前不启动下一个文件。
        app.poll_drop_queue();
        assert_eq!(app.drop_queue.len(), 1);

        // 栅格流水线结束后，队列里的矢量文件接着启动。
        app.raster_stage = None;
        app.poll_drop_queue();
        assert!(matches!(app.import, Some(ImportStage::Loading { .. })));
        assert!(app.drop_queue.is_empty());
        // 清理后台线程结果，避免测试结束时仍有未回收的消息。
        app.import = None;
    }
}
