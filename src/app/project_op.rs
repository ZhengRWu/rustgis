//! 工程文件（.rustgis）后台流水线：新建 / 打开 / 保存 / 另存为 / 未保存变更拦截。
//!
//! 与导入流水线（[`super::import`]）同款约定：文件对话框与序列化 / 反序列化
//! 都在后台线程，主线程每帧非阻塞轮询 + 50ms 定时唤醒，窗口永不卡死。
//! 磁盘格式本身在 `crate::project`，这里只负责「编排 + 应用状态变更」。

use std::path::PathBuf;
use std::sync::mpsc::{self, TryRecvError};

use egui::{Context, ViewportCommand};

use crate::dialog::Dialog;
use crate::geo::{LayerManager, LayerNode};
use crate::project;

use super::{RustGisApp, Status};

/// 系统"保存更改？"提示框中，保存成功后要执行的动作（见 [`ProjectStage::ConfirmSave`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AfterSave {
    /// 新建工程（保存后丢弃当前工程）。
    New,
    /// 打开工程（保存后弹出打开对话框）。
    Open,
    /// 退出程序（保存后退出）。
    Exit,
}

/// 用户在系统"保存更改？"提示框中的选择（由对话框后台线程回传）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SaveChoice {
    /// "保存"：先把当前工程保存，再继续后续动作。
    Save,
    /// "不保存"：放弃当前更改，直接执行后续动作。
    Discard,
    /// "取消"（或关闭对话框）：保留当前工程，什么都不做。
    Cancel,
}

/// 工程后台流水线：文件对话框 → 后台序列化 / 反序列化 → 应用到应用状态。
/// 与导入流水线（[`super::import::ImportStage`]）同款约定：每帧非阻塞轮询 + 50ms 唤醒。
pub(super) enum ProjectStage {
    /// 等待用户在系统"保存更改？"提示框中作出选择（保存 / 不保存 / 取消）。
    ConfirmSave {
        rx: mpsc::Receiver<SaveChoice>,
        /// 作出选择后的后续动作（"保存"时先保存再继续；"不保存"时直接执行）。
        after: AfterSave,
    },
    /// 等待用户在"打开工程"对话框中选定工程文件。
    PickOpen(mpsc::Receiver<Option<PathBuf>>),
    /// 等待用户在"另存为"对话框中选定保存路径。
    PickSaveAs {
        rx: mpsc::Receiver<Option<PathBuf>>,
        /// 保存成功后的后续动作（`None` = 独立的"保存 / 另存为"）。
        after: Option<AfterSave>,
    },
    /// 后台保存中（快照已移交后台线程）。
    Saving {
        rx: mpsc::Receiver<Result<(), String>>,
        target: PathBuf,
        after: Option<AfterSave>,
    },
    /// 后台打开中（解析 + 三角剖分）。
    Loading {
        rx: mpsc::Receiver<Result<project::ProjectData, String>>,
        path: PathBuf,
        name: String,
    },
}

impl RustGisApp {
    // ─────────────────────────── 工程：新建 / 打开 / 保存 ───────────────────────────

    /// 导入或工程任务是否都在空闲（文件对话框 / 后台 IO 进行中时禁止再发起新操作）。
    pub(super) fn project_idle(&self) -> bool {
        self.import.is_none() && self.project_op.is_none() && self.mask_job.is_none()
    }

    /// 弹出系统原生"保存更改？"提示框（按钮：保存 / 不保存 / 取消）。
    ///
    /// 与文件对话框同款约定：对话框在后台线程弹出并阻塞该线程，UI 线程
    /// 每帧非阻塞轮询结果（[`ProjectStage::ConfirmSave`]），窗口不卡死。
    /// Windows 上是 ComCtl32 v6 原生 TaskDialog（自定义按钮文案依赖
    /// build.rs 嵌入的 manifest），macOS 为 NSAlert，Linux 为 GTK 对话框。
    pub(super) fn ask_save_changes(&mut self, after: AfterSave) {
        if !self.project_idle() {
            return;
        }
        let title = self.t("project.savePromptTitle").to_string();
        let body = self.t("project.savePromptBody").to_string();
        let save = self.t("dialog.save").to_string();
        let dont_save = self.t("dialog.dontSave").to_string();
        let cancel = self.t("dialog.cancel").to_string();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            // 三平台的自定义按钮点击都回传 `Custom(按钮文案)`，
            // 这里按文案反查语义；任何意外结果一律按"取消"处理。
            let result = rfd::MessageDialog::new()
                .set_title(&title)
                .set_description(&body)
                .set_level(rfd::MessageLevel::Warning)
                .set_buttons(rfd::MessageButtons::YesNoCancelCustom(
                    save.clone(),
                    dont_save.clone(),
                    cancel.clone(),
                ))
                .show();
            let choice = match result {
                rfd::MessageDialogResult::Custom(label) if label == save => SaveChoice::Save,
                rfd::MessageDialogResult::Custom(label) if label == dont_save => {
                    SaveChoice::Discard
                }
                _ => SaveChoice::Cancel,
            };
            let _ = tx.send(choice);
        });
        self.project_op = Some(ProjectStage::ConfirmSave { rx, after });
    }

    /// 刷新主窗口标题栏：「工程名 — RustGIS 版本」（对标 ArcGIS Pro 标题栏显示工程名）。
    pub(super) fn refresh_window_title(&self, ctx: &Context) {
        let project = self
            .project_path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.t("window.untitled").to_string());
        let title = format!("{project} — RustGIS {}", env!("CARGO_PKG_VERSION"));
        ctx.send_viewport_cmd(ViewportCommand::Title(title));
    }

    /// 新建工程（文件菜单）。有未保存更改时先弹系统"保存更改？"提示。
    pub(super) fn start_new(&mut self) {
        if !self.project_idle() {
            return;
        }
        if self.dirty {
            self.ask_save_changes(AfterSave::New);
        } else {
            self.do_new();
        }
    }

    /// 真正执行"新建"：清空全部图层、重置视图与工程名（"未命名"）。
    fn do_new(&mut self) {
        self.layers = LayerManager::default();
        self.map.view = None; // 下一帧按默认范围自动适配
        self.zoom_extent = None; // 丢弃旧工程排队的「缩放至图层」
        self.attr_table = None; // 旧工程的属性表指向已不存在的图层，一并关闭
        self.project_projection = None; // 新工程从"未设置投影"开始
        self.project_path = None;
        self.dirty = false;
        self.title_dirty = true;
        self.status = Status::Message(self.t("status.newProject").to_string());
    }

    /// 打开工程（文件菜单）。有未保存更改时先弹系统"保存更改？"提示。
    pub(super) fn start_open(&mut self) {
        if !self.project_idle() {
            return;
        }
        if self.dirty {
            self.ask_save_changes(AfterSave::Open);
        } else {
            self.pick_open();
        }
    }

    /// 保存（文件菜单）：已有工程路径 → 直接保存；否则走"另存为"。
    pub(super) fn start_save(&mut self) {
        if !self.project_idle() {
            return;
        }
        match self.project_path.clone() {
            Some(path) => self.begin_save(path, None),
            None => self.pick_save_as(None),
        }
    }

    /// 另存为（文件菜单）：先选保存路径，再后台序列化。
    pub(super) fn start_save_as(&mut self) {
        if !self.project_idle() {
            return;
        }
        self.pick_save_as(None);
    }

    /// 弹出"打开工程"文件选择器（对话框线程与 UI 线程分离，不阻塞窗口）。
    fn pick_open(&mut self) {
        if !self.project_idle() {
            return;
        }
        let title = self.t("project.openTitle").to_string();
        let filter = self.t("project.fileFilter").to_string();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let chosen = rfd::FileDialog::new()
                .set_title(&title)
                .add_filter(&filter, &[project::PROJECT_EXT])
                .pick_file();
            let _ = tx.send(chosen);
        });
        self.project_op = Some(ProjectStage::PickOpen(rx));
    }

    /// 弹出"另存为"文件选择器。
    fn pick_save_as(&mut self, after: Option<AfterSave>) {
        if !self.project_idle() {
            return;
        }
        let title = self.t("project.saveAsTitle").to_string();
        let filter = self.t("project.fileFilter").to_string();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let chosen = rfd::FileDialog::new()
                .set_title(&title)
                .set_file_name(project::DEFAULT_FILE_NAME)
                .add_filter(&filter, &[project::PROJECT_EXT])
                .save_file();
            let _ = tx.send(chosen);
        });
        self.project_op = Some(ProjectStage::PickSaveAs { rx, after });
    }

    /// 把当前工程状态快照移交后台线程序列化写入 `path`。
    fn begin_save(&mut self, path: PathBuf, after: Option<AfterSave>) {
        if !self.project_idle() {
            return;
        }
        // 快照当前图层树、视图与工程默认投影：大工程 JSON 可达 MB 级，
        // 序列化不得占用 UI 线程。
        let nodes: Vec<LayerNode> = self.layers.roots_cloned();
        let view = self.map.view;
        let projection = self.project_projection.clone();
        let save_path = path.clone();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(project::save(&save_path, &nodes, view, projection));
        });
        self.status = Status::Message(self.t("status.saving").to_string());
        self.project_op = Some(ProjectStage::Saving {
            rx,
            target: path,
            after,
        });
    }

    /// 把选定的工程文件移交后台线程解析（含几何展开与三角剖分）。
    fn begin_load(&mut self, path: PathBuf) {
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        let load_path = path.clone();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(project::load(&load_path));
        });
        self.status = Status::Message(self.tf("status.opening", &[&name]));
        self.project_op = Some(ProjectStage::Loading { rx, path, name });
    }

    /// 推进工程后台流水线：选文件 → 后台序列化 / 解析 → 应用结果。每帧调用，非阻塞。
    pub(super) fn poll_project_op(&mut self, ctx: &Context) {
        let Some(stage) = self.project_op.take() else {
            return;
        };
        match stage {
            ProjectStage::ConfirmSave { rx, after } => match rx.try_recv() {
                Ok(choice) => self.on_save_choice(choice, after),
                Err(TryRecvError::Empty) => {
                    self.project_op = Some(ProjectStage::ConfirmSave { rx, after });
                }
                Err(TryRecvError::Disconnected) => {
                    // 对话框线程异常退出（无结果可回收）：按"取消"处理，保留当前工程。
                }
            },
            ProjectStage::PickOpen(rx) => match rx.try_recv() {
                Ok(Some(path)) => self.begin_load(path),
                Ok(None) => {} // 用户取消
                Err(TryRecvError::Empty) => self.project_op = Some(ProjectStage::PickOpen(rx)),
                Err(TryRecvError::Disconnected) => {}
            },
            ProjectStage::PickSaveAs { rx, after } => match rx.try_recv() {
                Ok(Some(mut path)) => {
                    // 对话框里没带扩展名（或未选类型）时补上工程扩展名。
                    if path
                        .extension()
                        .map(|e| e != project::PROJECT_EXT)
                        .unwrap_or(true)
                    {
                        path.set_extension(project::PROJECT_EXT);
                    }
                    self.begin_save(path, after);
                }
                Ok(None) => {
                    // 用户取消"另存为"：若是"先保存再继续"的链式操作，则整链中止。
                }
                Err(TryRecvError::Empty) => {
                    self.project_op = Some(ProjectStage::PickSaveAs { rx, after });
                }
                Err(TryRecvError::Disconnected) => {}
            },
            ProjectStage::Saving { rx, target, after } => match rx.try_recv() {
                Ok(Ok(())) => {
                    let name = target
                        .file_name()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_else(|| target.display().to_string());
                    self.project_path = Some(target);
                    self.dirty = false;
                    self.title_dirty = true;
                    self.status = Status::Message(self.tf("status.saved", &[&name]));
                    self.finish_after_save(after);
                }
                Ok(Err(e)) => {
                    self.status = Status::Message(self.tf("status.saveError", &[&e]));
                    self.dialog = Some(Dialog::Error { body: e });
                }
                Err(TryRecvError::Empty) => {
                    self.project_op = Some(ProjectStage::Saving { rx, target, after });
                }
                Err(TryRecvError::Disconnected) => {
                    let msg = self.t("status.saveAborted").to_string();
                    self.status = Status::Message(msg.clone());
                    self.dialog = Some(Dialog::Error { body: msg });
                }
            },
            ProjectStage::Loading { rx, path, name } => match rx.try_recv() {
                Ok(Ok(data)) => {
                    self.layers.replace_with(data.nodes);
                    self.map.view = data.view; // None 时下一帧自动适配
                    self.zoom_extent = None; // 丢弃上一个工程排队的「缩放至图层」
                    self.attr_table = None; // 属性表关到旧工程的图层上，重新打开前先收起
                    self.project_projection = data.projection; // 旧版本文件 → None（未设置）
                    self.project_path = Some(path);
                    self.dirty = false;
                    self.title_dirty = true;
                    // 栅格源文件缺失等非致命问题：只提示，不影响工程其余部分。
                    if data.warnings.is_empty() {
                        self.status = Status::Message(self.tf("status.opened", &[&name]));
                    } else {
                        let body = data.warnings.join("\n");
                        self.status =
                            Status::Message(self.tf("status.openedWithWarnings", &[&name]));
                        self.dialog = Some(Dialog::Error { body });
                    }
                }
                Ok(Err(e)) => {
                    self.status = Status::Message(self.tf("status.loadError", &[&e]));
                    self.dialog = Some(Dialog::Error { body: e });
                }
                Err(TryRecvError::Empty) => {
                    self.project_op = Some(ProjectStage::Loading { rx, path, name });
                }
                Err(TryRecvError::Disconnected) => {
                    let msg = self.t("status.loadAborted").to_string();
                    self.status = Status::Message(msg.clone());
                    self.dialog = Some(Dialog::Error { body: msg });
                }
            },
        }

        // 与导入流水线同理：任务进行中用 50ms 定时唤醒，及时取回后台结果。
        if self.project_op.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        }
    }

    /// 保存成功后执行"先保存再继续"链的后续动作。
    fn finish_after_save(&mut self, after: Option<AfterSave>) {
        match after {
            Some(AfterSave::New) => self.do_new(),
            Some(AfterSave::Open) => self.pick_open(),
            Some(AfterSave::Exit) => self.exiting = true,
            None => {}
        }
    }

    /// 统一退出入口（菜单「退出」/ 标题栏 X）：有未保存更改先弹系统保存提示，
    /// 否则标记退出（下一帧真正关闭窗口）。
    pub fn request_exit(&mut self) {
        if self.exiting {
            return;
        }
        if self.dirty {
            self.ask_save_changes(AfterSave::Exit);
        } else {
            self.exiting = true;
        }
    }

    /// 系统"保存更改？"提示框的选择结果（[`ProjectStage::ConfirmSave`] 回传）。
    fn on_save_choice(&mut self, choice: SaveChoice, after: AfterSave) {
        match choice {
            SaveChoice::Save => match self.project_path.clone() {
                Some(path) => self.begin_save(path, Some(after)),
                None => self.pick_save_as(Some(after)),
            },
            SaveChoice::Discard => match after {
                AfterSave::New => self.do_new(),
                AfterSave::Open => self.pick_open(),
                AfterSave::Exit => self.exiting = true,
            },
            // "取消"只关闭提示框，保留当前工程；OS 关闭事件（标题栏 X）在事件帧
            // 已被 update() 的 close_requested 分支拦截，此处无需再处理。
            SaveChoice::Cancel => {}
        }
    }
}
