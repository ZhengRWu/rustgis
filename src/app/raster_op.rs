//! 栅格图层导入 / 统计 / 金字塔构建的后台流水线。
//!
//! 与 [`super::import`]、[`super::project_op`] 同一套约定：UI 线程只做非阻塞轮询，
//! 打开文件、统计计算、金字塔构建全部在后台线程完成，进行中用 50ms 定时唤醒。
//!
//! 流程：
//! 1. [`RustGisApp::pick_and_add_raster`]：后台线程弹出系统文件对话框；
//! 2. 选定后后台 [`RasterLayer::open`]（只读文件头与 GeoKey，不解码像素）；
//! 3. 加入图层树后，后台顺序计算显示波段的统计值（已有内嵌统计值时直接复用）；
//! 4. 影像较大且没有任何概览时，弹出**系统原生**询问框确认是否构建金字塔。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, TryRecvError};

use egui::Context;

use crate::dialog::{Confirm, Level};
use crate::raster::{RasterLayer, format_bytes};

use super::{RustGisApp, Status};

/// 低于该像素数（约 2048²）的影像即使没有概览也能流畅浏览，不主动提示建金字塔。
const PYRAMID_PROMPT_PIXELS: u64 = 2048 * 2048;

/// 扩展名是否属于「栅格」：混合文件选择器（添加数据）据此把 .tif/.tiff 与
/// Sentinel-2 `.SAFE` 产品包交给栅格流水线，其余交给矢量导入，
/// 避免出现「不支持的文件格式：.tif」。
pub(super) fn is_raster_path(path: &Path) -> bool {
    if crate::raster::is_safe_package(path) {
        return true;
    }
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("tif") || ext.eq_ignore_ascii_case("tiff"))
}

/// 栅格导入流水线。
pub(super) enum RasterStage {
    /// 等待用户在系统文件对话框中选定 TIF。
    Choosing(mpsc::Receiver<Option<PathBuf>>),
    /// 已选定文件，后台正在解析文件头 / 地理参考。
    Opening {
        rx: mpsc::Receiver<Result<RasterLayer, String>>,
        name: String,
    },
}

/// 后台栅格任务的类型。
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum RasterJobKind {
    /// 计算显示波段的统计值。
    Statistics,
    /// 构建概览金字塔缓存。
    Pyramid,
}

impl RasterJobKind {
    fn i18n_key(self) -> &'static str {
        match self {
            Self::Statistics => "status.rasterStats",
            Self::Pyramid => "status.rasterPyramid",
        }
    }
}

/// 后台任务回传的结果。
enum JobOutcome {
    /// 已计算完成的波段索引。
    Statistics(Vec<usize>),
    /// 已构建的概览层数。
    Pyramid(usize),
}

/// 进行中的后台栅格任务。
pub(super) struct RasterJob {
    /// 所属图层 id。
    layer: u64,
    kind: RasterJobKind,
    rx: mpsc::Receiver<Result<JobOutcome, String>>,
    /// 取消标志（关窗口 / 删除图层时置位）。
    cancel: Arc<AtomicBool>,
    /// 进度千分比（0..=1000），后台线程直接写入，避免大量消息。
    progress: Arc<AtomicU32>,
}

impl RasterJob {
    /// 进度百分比（0..=100）。
    fn percent(&self) -> u32 {
        self.progress.load(Ordering::Relaxed).min(1000) / 10
    }
}

/// 「是否构建概览」的系统原生询问状态。
///
/// 一批导入只弹一次：打开完成的大影像先进 [`RustGisApp::pyramid_candidates`]
/// 候选，等整批导入结束后统一询问；确认时对整批逐个排队构建。
pub(super) struct PyramidPrompt {
    body: String,
    /// 后台已弹出的原生询问框（`None` = 下一帧弹出）。
    job: Option<Confirm>,
}

impl RustGisApp {
    /// 系统文件选择器 → 后台打开 TIF/GeoTIFF 为栅格图层。
    pub(super) fn pick_and_add_raster(&mut self) {
        if !self.project_idle() || self.raster_stage.is_some() {
            return;
        }
        let title = self.t("dialog.addRasterTitle").to_string();
        let (tx, rx) = mpsc::channel::<Option<PathBuf>>();
        std::thread::spawn(move || {
            let chosen = rfd::FileDialog::new()
                .set_title(&title)
                .add_filter("GeoTIFF", &["tif", "tiff"])
                .add_filter("All files", &["*"])
                .pick_file();
            let _ = tx.send(chosen);
        });
        self.raster_stage = Some(RasterStage::Choosing(rx));
    }

    /// 系统文件选择器 → 后台打开 Sentinel-2 SAFE 产品包（选择 `.SAFE` 目录）。
    pub(super) fn pick_and_add_safe(&mut self) {
        if !self.project_idle() || self.raster_stage.is_some() {
            return;
        }
        let title = self.t("dialog.addRasterTitle").to_string();
        let (tx, rx) = mpsc::channel::<Option<PathBuf>>();
        std::thread::spawn(move || {
            let chosen = rfd::FileDialog::new().set_title(&title).pick_folder();
            let _ = tx.send(chosen);
        });
        self.raster_stage = Some(RasterStage::Choosing(rx));
    }

    /// 让指定路径进入栅格打开流水线（后台线程解析文件头 + GeoTIFF 标签）。
    ///
    /// 供两个入口共用：栅格专用选择器，以及「添加数据」这类混合选择器
    /// （按扩展名把 .tif/.tiff 转交给栅格读取，见 [`super::import`]）。
    pub(super) fn start_raster_open(&mut self, path: PathBuf) {
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        let (tx, open_rx) = mpsc::channel();
        std::thread::spawn(move || {
            // 只解析文件头 + GeoKey，像素留到显示时按窗口读取。
            let result = RasterLayer::open(&path, String::new())
                .map_err(|err| format!("[{}] {}", err.code(), err));
            let _ = tx.send(result);
        });
        self.status = Status::Message(self.tf("status.rasterOpening", &[&name]));
        self.raster_stage = Some(RasterStage::Opening { rx: open_rx, name });
    }

    /// 推进栅格导入流水线（每帧调用，非阻塞）。
    pub(super) fn poll_raster_stage(&mut self, ctx: &Context) {
        let Some(stage) = self.raster_stage.take() else {
            return;
        };
        match stage {
            RasterStage::Choosing(rx) => match rx.try_recv() {
                Ok(None) => {}
                Ok(Some(path)) => self.start_raster_open(path),
                Err(TryRecvError::Empty) => self.raster_stage = Some(RasterStage::Choosing(rx)),
                Err(TryRecvError::Disconnected) => {}
            },
            RasterStage::Opening { rx, name } => match rx.try_recv() {
                Ok(Ok(mut layer)) => {
                    // 图层名用文件名（去掉扩展名），并保证不与已有图层重名。
                    let base = name
                        .rsplit_once('.')
                        .map_or(name.as_str(), |(stem, _)| stem);
                    let unique = self.layers.unique_name(base);
                    layer.name = unique.clone();
                    // 与矢量导入一致：工程未设置投影且尚无图层时，首个栅格图层的
                    // 坐标系成为工程默认值（判定必须在加入之前）。
                    let first_layer = self.layers.is_empty()
                        && self
                            .project_projection
                            .as_ref()
                            .is_none_or(|p| *p == crate::geo::ProjectionInfo::none());
                    if first_layer && layer.projection != crate::geo::ProjectionInfo::none() {
                        self.project_projection = Some(layer.projection.clone());
                    }
                    let (width, height, bands, bytes, overviews, missing_georef) = {
                        let dataset = layer.dataset();
                        let meta = dataset.metadata().clone();
                        (
                            meta.width,
                            meta.height,
                            meta.band_count,
                            meta.file_size,
                            dataset.has_overviews(),
                            dataset.georeference().is_missing(),
                        )
                    };
                    let id = self.layers.add_raster(layer);
                    self.needs_fit = true;
                    self.dirty = true;
                    // 完成提示走右下角通知（状态栏只留"正在打开…"这类进行中文案）。
                    let body = self.tf(
                        "status.rasterAdded",
                        &[
                            &unique,
                            &format!("{width}×{height}"),
                            &bands.to_string(),
                            &format_bytes(bytes),
                        ],
                    );
                    let title = self.t("toast.rasterAdded").to_string();
                    let now = ctx.input(|input| input.time);
                    self.toasts.push(now, title, body);
                    // 结束态回到"就绪"：「正在打开栅格文件…」不能一直挂在状态栏上。
                    // 若下面随即开始计算统计，会写入新的进行中文案。
                    self.status = Status::Ready;
                    if missing_georef {
                        // 缺失地理参考不是错误：按像素坐标显示，并明确告知用户。
                        self.dialog = Some(crate::dialog::Dialog::Error {
                            body: self.t("raster.noGeoReference").to_string(),
                        });
                    }
                    // 已有内嵌统计值时不再重复计算。
                    self.start_statistics_for_display(id);
                    // 大影像且无概览 → 记入候选；等本批导入全部结束后统一
                    // 询问一次，而不是每张都弹框打断批量拖放。
                    if !overviews && u64::from(width) * u64::from(height) > PYRAMID_PROMPT_PIXELS {
                        self.pyramid_candidates.push(id);
                    }
                }
                Ok(Err(e)) => {
                    self.status = Status::Message(self.tf("status.loadError", &[&e]));
                    self.dialog = Some(crate::dialog::Dialog::Error { body: e });
                }
                Err(TryRecvError::Empty) => {
                    self.raster_stage = Some(RasterStage::Opening { rx, name })
                }
                Err(TryRecvError::Disconnected) => {
                    let msg = self.t("status.loadAborted").to_string();
                    self.status = Status::Message(msg.clone());
                    self.dialog = Some(crate::dialog::Dialog::Error { body: msg });
                }
            },
        }
        if self.raster_stage.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        }
    }

    /// 导入进行中时面板上的提示 key（空闲时为 `None`）。
    pub(super) fn raster_import_hint(&self) -> Option<&'static str> {
        match self.raster_stage {
            Some(RasterStage::Choosing(_)) => Some("layers.loading"),
            Some(RasterStage::Opening { .. }) => Some("layers.rasterOpening"),
            None => None,
        }
    }

    /// 为当前显示用到的波段启动统计任务（三者都已有时不启动）。
    ///
    /// 同一时刻只跑一个栅格任务：若已有任务在跑（典型场景是拖入多幅 TIF，
    /// 前一幅的统计还没算完，后一幅已打开），把本图层排入
    /// [`RustGisApp::pending_statistics`]，当前任务结束后由
    /// [`Self::poll_raster_job`] 依序接续，保证每幅图都会被统计。
    pub(super) fn start_statistics_for_display(&mut self, layer_id: u64) {
        let Some((handle, bands)) = self.raster_handle_and_bands(layer_id) else {
            return;
        };
        let needed: Vec<usize> = {
            let dataset = handle.lock().unwrap_or_else(|e| e.into_inner());
            bands
                .into_iter()
                .filter(|band| !dataset.has_statistics(*band))
                .collect()
        };
        if needed.is_empty() {
            return;
        }
        if self.raster_job.is_some() {
            self.pending_statistics.push_back(layer_id);
            return;
        }
        self.start_raster_job(layer_id, RasterJobKind::Statistics, handle, Some(needed));
    }

    /// 启动后台任务：`bands` 仅统计任务使用。
    fn start_raster_job(
        &mut self,
        layer_id: u64,
        kind: RasterJobKind,
        handle: Arc<std::sync::Mutex<crate::raster::RasterDataset>>,
        bands: Option<Vec<usize>>,
    ) {
        if self.raster_job.is_some() {
            return; // 同一时刻只跑一个栅格任务
        }
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(AtomicU32::new(0));
        let job_cancel = Arc::clone(&cancel);
        let job_progress = Arc::clone(&progress);
        std::thread::spawn(move || {
            let result = {
                let mut dataset = handle.lock().unwrap_or_else(|e| e.into_inner());
                match kind {
                    RasterJobKind::Statistics => {
                        let bands = bands.unwrap_or_default();
                        let count = bands.len().max(1) as f32;
                        let mut done = Vec::new();
                        let mut failure = None;
                        for (index, band) in bands.iter().enumerate() {
                            let base = index as f32 / count;
                            let mut report = |ratio: f32| {
                                let value = (base + ratio / count) * 1000.0;
                                job_progress
                                    .store(value.clamp(0.0, 1000.0) as u32, Ordering::Relaxed);
                            };
                            match dataset.compute_statistics(*band, &job_cancel, &mut report) {
                                Ok(_) => done.push(*band),
                                Err(err) => {
                                    failure = Some(err.to_string());
                                    break;
                                }
                            }
                        }
                        match failure {
                            Some(err) => Err(err),
                            None => Ok(JobOutcome::Statistics(done)),
                        }
                    }
                    RasterJobKind::Pyramid => {
                        let mut report = |ratio: f32| {
                            job_progress.store(
                                (ratio * 1000.0).clamp(0.0, 1000.0) as u32,
                                Ordering::Relaxed,
                            );
                        };
                        dataset
                            .build_overviews(&job_cancel, &mut report)
                            .map(JobOutcome::Pyramid)
                            .map_err(|err| err.to_string())
                    }
                }
            };
            job_progress.store(1000, Ordering::Relaxed);
            let _ = tx.send(result);
        });
        self.raster_job = Some(RasterJob {
            layer: layer_id,
            kind,
            rx,
            cancel,
            progress,
        });
    }

    /// 取栅格图层的共享句柄与当前显示波段。
    fn raster_handle_and_bands(
        &self,
        layer_id: u64,
    ) -> Option<(
        Arc<std::sync::Mutex<crate::raster::RasterDataset>>,
        Vec<usize>,
    )> {
        let layer = self.layers.node(layer_id)?.raster()?;
        let bands = layer.display.bands.to_vec();
        Some((layer.handle(), bands))
    }

    /// 推进后台栅格任务（每帧调用，非阻塞）。
    pub(super) fn poll_raster_job(&mut self, ctx: &Context) {
        let Some(job) = self.raster_job.as_ref() else {
            return;
        };
        let layer_id = job.layer;
        match job.rx.try_recv() {
            Ok(Ok(outcome)) => {
                self.raster_job = None;
                let message = match outcome {
                    JobOutcome::Statistics(bands) => {
                        let count = bands.len().to_string();
                        self.tf("status.rasterStatsDone", &[&count])
                    }
                    JobOutcome::Pyramid(levels) => {
                        let count = levels.to_string();
                        self.tf("status.rasterPyramidDone", &[&count])
                    }
                };
                // 结果不占用状态栏（否则会一直挂着这句话）：在右下角弹一条通知，
                // 标题是图层名，正文是结果文案；图层属性对话框每帧读数据集，
                // 统计值会自动刷新，无需额外通知。
                let title = self
                    .layers
                    .node(layer_id)
                    .map(|node| node.name().to_string())
                    .unwrap_or_default();
                let now = ctx.input(|input| input.time);
                self.toasts.push(now, title, message);
                // 结束态回到"就绪"：进行中的文案（正在计算统计…）不能一直挂着。
                // 若还有排队中的统计 / 金字塔任务，下面会立刻写入新的进行中文案。
                self.status = Status::Ready;
                // 任务结束后先接续排队中的统计（拖入多幅 TIF 时后打开的图层
                // 都排在这里），直到某次启动真正占住了任务槽或队列取空。
                while let Some(pending) = self.pending_statistics.pop_front() {
                    self.start_statistics_for_display(pending);
                    if self.raster_job.is_some() {
                        break;
                    }
                }
                // 再执行排队中的金字塔构建（若统计任务已占住任务槽，这里会重新排队）。
                while let Some(pending) = self.pending_pyramid.pop_front() {
                    self.begin_pyramid(pending);
                    if self.raster_job.is_some() {
                        break;
                    }
                }
                ctx.request_repaint();
            }
            Ok(Err(e)) => {
                self.raster_job = None;
                self.pending_pyramid.clear();
                self.pending_statistics.clear();
                self.status = Status::Message(self.tf("status.rasterJobError", &[&e]));
                self.dialog = Some(crate::dialog::Dialog::Error { body: e });
            }
            Err(TryRecvError::Empty) => {
                ctx.request_repaint_after(std::time::Duration::from_millis(50));
            }
            Err(TryRecvError::Disconnected) => {
                self.raster_job = None;
                self.pending_pyramid.clear();
                self.pending_statistics.clear();
                let msg = self.t("status.loadAborted").to_string();
                self.status = Status::Message(msg.clone());
                self.dialog = Some(crate::dialog::Dialog::Error { body: msg });
            }
        }
    }

    /// 后台任务的状态栏进度文案（`None` = 无任务）。
    pub(super) fn raster_job_status(&self) -> Option<String> {
        let job = self.raster_job.as_ref()?;
        let name = self
            .layers
            .node(job.layer)
            .map(|node| node.name().to_string())
            .unwrap_or_default();
        Some(self.tf(
            job.kind.i18n_key(),
            &[&name, &format!("{}%", job.percent())],
        ))
    }

    /// 用户确认构建金字塔：立刻开始，或在一项任务之后再开始。
    pub(super) fn begin_pyramid(&mut self, layer_id: u64) {
        let Some((handle, _)) = self.raster_handle_and_bands(layer_id) else {
            return;
        };
        if self.raster_job.is_some() {
            self.pending_pyramid.push_back(layer_id);
            return;
        }
        self.start_raster_job(layer_id, RasterJobKind::Pyramid, handle, None);
    }

    /// 根据候选图层生成询问正文：单幅沿用原文案，多幅列出图层名（最多 5 个）。
    fn pyramid_batch_body(&self) -> String {
        let names: Vec<String> = self
            .pyramid_candidates
            .iter()
            .filter_map(|id| self.layers.node(*id).map(|n| n.name().to_string()))
            .collect();
        if names.len() == 1 {
            let dims = self
                .layers
                .node(self.pyramid_candidates[0])
                .and_then(|n| n.raster())
                .map(|layer| {
                    let dataset = layer.dataset();
                    let meta = dataset.metadata();
                    format!("{}×{}", meta.width, meta.height)
                })
                .unwrap_or_default();
            return self.tf("dialog.pyramidBody", &[&names[0], &dims]);
        }
        let count = names.len().to_string();
        let mut list = names.iter().take(5).cloned().collect::<Vec<_>>().join("\n");
        if names.len() > 5 {
            list.push_str("\n…");
        }
        self.tf("dialog.pyramidBatchBody", &[&count, &list])
    }

    /// 本批导入已全部结束（拖放队列 / 矢量导入 / 栅格打开都空闲）。
    fn pyramid_batch_idle(&self) -> bool {
        self.drop_queue.is_empty()
            && self.import.is_none()
            && self.raster_stage.is_none()
    }

    /// 候选非空且本批导入已结束时，创建"是否构建概览"询问（只创建一次）。
    ///
    /// 拆成独立步骤供无头测试复用：测试不能真的弹出系统原生对话框。
    fn maybe_create_pyramid_prompt(&mut self) {
        if self.raster_prompt.is_some()
            || self.pyramid_candidates.is_empty()
            || !self.pyramid_batch_idle()
        {
            return;
        }
        let body = self.pyramid_batch_body();
        self.raster_prompt = Some(PyramidPrompt { body, job: None });
    }

    /// 「是否构建概览」：**系统原生**询问框（后台线程弹出、这里非阻塞轮询）。
    ///
    /// 与提示框（[`crate::dialog::NativeDialog`]）同一套约定：弹框开着时 UI
    /// 循环照常运行；用户点「是」才开始构建金字塔，「否」/ 关闭窗口则跳过
    /// （整批候选一并清空，后续新导入的批次会重新询问）。
    pub(super) fn show_pyramid_prompt(&mut self, ctx: &Context) {
        self.maybe_create_pyramid_prompt();

        if self.raster_prompt.as_ref().is_some_and(|p| p.job.is_none()) {
            let body = self
                .raster_prompt
                .as_ref()
                .map(|p| p.body.clone())
                .unwrap_or_default();
            let job = Confirm::open(
                self.t("dialog.pyramidTitle").to_string(),
                format!("{body}\n\n{}", self.t("dialog.pyramidHint")),
                self.t("dialog.yes").to_string(),
                self.t("dialog.no").to_string(),
                Level::Info,
            );
            if let Some(prompt) = self.raster_prompt.as_mut() {
                prompt.job = Some(job);
            }
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
            return;
        }

        let Some(job) = self.raster_prompt.as_ref().and_then(|p| p.job.as_ref()) else {
            return;
        };
        let Some(confirmed) = job.poll() else {
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
            return;
        };
        self.raster_prompt = None;
        let candidates = std::mem::take(&mut self.pyramid_candidates);
        if confirmed {
            for layer in candidates {
                self.begin_pyramid(layer);
            }
        }
        ctx.request_repaint();
    }

    /// 取消某个图层正在进行的后台任务（删除图层 / 关闭工程时调用）。
    pub(super) fn cancel_raster_job(&mut self, layer_id: u64) {
        if let Some(job) = self.raster_job.as_ref()
            && job.layer == layer_id
        {
            job.cancel.store(true, Ordering::Relaxed);
        }
        self.pending_pyramid.retain(|id| *id != layer_id);
        self.pyramid_candidates.retain(|id| *id != layer_id);
        self.pending_statistics.retain(|id| *id != layer_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use egui::{Pos2, Rect, Vec2};

    use crate::raster::RasterDisplay;
    use crate::raster::fixture_tests::sample_geotiff;

    fn headless_input() -> egui::RawInput {
        egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1024.0, 768.0))),
            ..Default::default()
        }
    }

    /// 无头跑一帧地图画布（栅格绘制挂在这一步里）。
    fn run_map_frame(ctx: &egui::Context, app: &mut RustGisApp) {
        let _ = ctx.run(headless_input(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| app.map_panel(ui));
        });
    }

    /// 走一遍真实导入流水线：后台打开结果 → 图层入树 → 地图帧里栅格纹理完成上传，
    /// 再打开属性弹框、修改显示设置，确认纹理随显示设置更新而不泄漏。
    #[test]
    fn raster_import_pipeline_paints_texture() {
        let path = sample_geotiff("app-e2e-geo.tif");
        let ctx = egui::Context::default();
        let mut app = RustGisApp::new(&ctx);

        // 模拟「后台已打开完成」的那一刻。
        let layer = RasterLayer::open(&path, String::new()).unwrap();
        let (tx, rx) = mpsc::channel();
        tx.send(Ok(layer)).unwrap();
        app.raster_stage = Some(RasterStage::Opening {
            rx,
            name: "app-e2e-geo".to_owned(),
        });
        app.poll_raster_stage(&ctx);
        assert!(app.raster_stage.is_none(), "导入阶段应结束");

        let id = app
            .layers
            .iter_visible_rasters()
            .next()
            .expect("栅格应已加入图层树")
            .id;

        // 后台统计任务结束前不断轮询（主线程只做非阻塞检查）。
        for _ in 0..400 {
            app.poll_raster_job(&ctx);
            if app.raster_job.is_none() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(app.raster_job.is_none(), "统计任务应已结束");

        run_map_frame(&ctx, &mut app);
        assert!(
            app.raster_painter.cached_textures() >= 1,
            "应上传至少一块纹理"
        );
        let windows = app.raster_painter.cached_windows(id);
        assert!(!windows.is_empty(), "应有纹理");
        assert!(
            windows.iter().all(|window| window[2] > 0 && window[3] > 0),
            "像素窗口不应为空"
        );

        // 双击图层打开「图层属性」：默认落在「符号系统」页。
        app.open_raster_props(id);
        let _ = ctx.run(headless_input(), |ctx| app.show_raster_props(ctx));
        assert!(app.raster_props.is_some(), "属性弹框应保持打开");
        app.raster_props = None;

        // 改显示设置后重新出图：纹理按新的显示参数重建。
        app.layers
            .set_raster_display(id, RasterDisplay::for_band_count(1, false));
        run_map_frame(&ctx, &mut app);
        assert!(app.raster_painter.cached_textures() >= 1);
    }

    /// SAFE 产品包走同一条导入 → 绘制链路：JPEG 2000 按窗口解码后上传纹理。
    #[test]
    fn safe_package_imports_and_paints_texture() {
        let Some(root) = crate::raster::fixture_tests::safe_fixture(
            "app-e2e-safe.SAFE",
            &[("B04", "10m"), ("B03", "10m"), ("B02", "10m")],
        ) else {
            eprintln!(
                "跳过：没有可用的 JPEG 2000 样例（设置 RUSTGIS_JP2_SAMPLE 或放到 $TEMP/jp2sample）"
            );
            return;
        };

        let ctx = egui::Context::default();
        let mut app = RustGisApp::new(&ctx);
        let layer = RasterLayer::open(&root, String::new()).unwrap();
        let (tx, rx) = mpsc::channel();
        tx.send(Ok(layer)).unwrap();
        app.raster_stage = Some(RasterStage::Opening {
            rx,
            name: "app-e2e-safe".to_owned(),
        });
        app.poll_raster_stage(&ctx);
        assert!(app.raster_stage.is_none(), "导入阶段应结束");

        let id = app
            .layers
            .iter_visible_rasters()
            .next()
            .expect("SAFE 栅格应已加入图层树")
            .id;

        // 统计按波段抽样解码，等后台任务结束。
        for _ in 0..400 {
            app.poll_raster_job(&ctx);
            if app.raster_job.is_none() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(app.raster_job.is_none(), "统计任务应已结束");

        run_map_frame(&ctx, &mut app);
        assert!(
            app.raster_painter.cached_textures() >= 1,
            "SAFE 波段应上传至少一块纹理"
        );
        let windows = app.raster_painter.cached_windows(id);
        assert!(!windows.is_empty(), "应有纹理");
        assert!(
            windows.iter().all(|window| window[2] > 0 && window[3] > 0),
            "像素窗口不应为空"
        );

        // 属性弹框里的「源」页应能展示 JPEG 2000 相关信息。
        app.open_raster_props(id);
        let _ = ctx.run(headless_input(), |ctx| app.show_raster_props(ctx));
        assert!(app.raster_props.is_some(), "属性弹框应保持打开");
        app.raster_props = None;

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 拖入多幅 TIF：后打开的图层统计不能被正在运行的任务静默吞掉，
    /// 而应排队，当前任务结束后依序接续，保证每幅图都有统计值。
    #[test]
    fn multiple_rasters_queue_statistics_while_busy() {
        let path = sample_geotiff("stats-queue.tif");
        let ctx = egui::Context::default();
        let mut app = RustGisApp::new(&ctx);

        // 第一幅：走真实打开流水线，导入完成后统计任务立即启动。
        let layer = RasterLayer::open(&path, String::new()).unwrap();
        let (tx, rx) = mpsc::channel();
        tx.send(Ok(layer)).unwrap();
        app.raster_stage = Some(RasterStage::Opening {
            rx,
            name: "stats-queue-a".to_owned(),
        });
        app.poll_raster_stage(&ctx);
        assert!(app.raster_job.is_some(), "第一幅导入后应立即开始统计");

        // 第二幅在第一幅统计仍在运行时打开完成：统计应排队而不是被丢弃。
        let layer = RasterLayer::open(&path, String::new()).unwrap();
        let (tx, rx) = mpsc::channel();
        tx.send(Ok(layer)).unwrap();
        app.raster_stage = Some(RasterStage::Opening {
            rx,
            name: "stats-queue-b".to_owned(),
        });
        app.poll_raster_stage(&ctx);
        assert_eq!(app.pending_statistics.len(), 1, "忙碌时第二幅的统计应排队");

        let ids: Vec<u64> = app
            .layers
            .iter_visible_rasters()
            .map(|l| l.id)
            .collect();
        assert_eq!(ids.len(), 2, "两幅图都应已加入图层树");

        // 依次等两个统计任务都跑完。
        for _ in 0..800 {
            app.poll_raster_job(&ctx);
            if app.raster_job.is_none() && app.pending_statistics.is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(app.raster_job.is_none(), "所有统计任务应已结束");
        assert!(app.pending_statistics.is_empty(), "统计队列应取空");
        for id in ids {
            let layer = app.layers.node(id).and_then(|n| n.raster()).expect("图层存在");
            let handle = layer.handle();
            let dataset = handle.lock().unwrap_or_else(|e| e.into_inner());
            assert!(dataset.has_statistics(0), "每幅图的显示波段都应有统计值");
        }
    }

    /// 批量导入多幅大影像（无概览）：打开过程中不逐张弹框，整批结束后
    /// 只创建一次"是否构建概览"询问。
    #[test]
    fn batch_import_prompts_pyramids_once() {
        let path =
            crate::raster::fixture_tests::pyramid_prompt_geotiff("pyramid-batch.tif");
        let ctx = egui::Context::default();
        let mut app = RustGisApp::new(&ctx);

        // 两幅大影像依次打开：都进候选，不弹框。
        for name in ["pyramid-batch-a", "pyramid-batch-b"] {
            let layer = RasterLayer::open(&path, String::new()).unwrap();
            let (tx, rx) = mpsc::channel();
            tx.send(Ok(layer)).unwrap();
            app.raster_stage = Some(RasterStage::Opening {
                rx,
                name: name.to_owned(),
            });
            app.poll_raster_stage(&ctx);
        }
        assert_eq!(app.pyramid_candidates.len(), 2, "两幅都应进入候选");
        assert!(app.raster_prompt.is_none(), "打开过程中不应弹框");

        // 拖放队列里还有待导入文件时：整批未结束，不弹框。
        app.drop_queue.push_back(path.clone());
        app.show_pyramid_prompt(&ctx);
        assert!(app.raster_prompt.is_none(), "批量导入未结束不应弹框");
        app.drop_queue.clear();

        // 本批结束后：只创建一次询问；候选保留到用户作答，不会重复弹。
        app.maybe_create_pyramid_prompt();
        assert!(app.raster_prompt.is_some(), "批量结束后应弹一次询问");
        let first = app.raster_prompt.as_ref().unwrap();
        assert!(first.job.is_none(), "测试中不应真正弹出系统对话框");
        app.maybe_create_pyramid_prompt();
        assert!(app.raster_prompt.is_some(), "询问已存在时不应重复创建");
    }
}
