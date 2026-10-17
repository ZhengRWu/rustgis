//! 主应用：窗口布局（菜单条 / 地图视图 / 图层面板 / 状态栏 / 模态对话框）。
//!
//! 模块按职责拆分（本文件只保留「应用状态 + 主循环 + 菜单分发」）：
//! - [`layers_panel`]：左侧「内容列表」（TOC）面板——图层树、拖拽重排、右键菜单；
//! - [`map_view`]：中央地图视图（平移 / 缩放交互 + 渲染）；
//! - [`widgets`]：矢量图标与通用小部件（刻意不引入图片资源依赖）；
//! - [`import`]：图层导入后台流水线（选文件 → 后台解析 / 三角化）；
//! - [`buffer_dialog`]：缓冲区分析对话框 + 后台缓冲流水线；
//! - [`geoprocess_dialog`]：叠加类地理处理工具（裁剪 / 相交 / 联合 / 合并 / 融合）；
//! - [`project_op`]：工程文件打开 / 保存后台流水线（含系统"保存更改？"提示）。
//!
//! UI 线程只做非阻塞轮询：导入 / 打开 / 保存的重活都在后台线程
//! （[`ImportStage`](import::ImportStage) / [`ProjectStage`](project_op::ProjectStage)），
//! 任何时刻都不锁住窗口。

pub mod buffer_dialog;
pub mod geoprocess_dialog;
pub mod import;
pub mod layers_panel;
pub mod map_view;
pub mod project_op;
pub mod widgets;

use std::path::PathBuf;

use egui::{CentralPanel, SidePanel, TopBottomPanel, ViewportCommand};

use crate::dialog::{self, Dialog};
use crate::fonts;
use crate::geo::overlay;
use crate::geo::{CrsId, LayerManager, ProjectionInfo, ProjectionModel};
use crate::i18n::Language;
use crate::map::{self, MapState, Tool};
use crate::menus;
use crate::style;

use self::buffer_dialog::{BufferJob, BufferWindow};
use self::geoprocess_dialog::{GeoJob, GeoWindow};
use self::import::ImportStage;
use self::project_op::{AfterSave, ProjectStage};

/// 投影模型 → i18n key（状态栏第三段用）。
fn model_i18n_key(model: ProjectionModel) -> &'static str {
    match model {
        ProjectionModel::Geographic => "projection.geographic",
        ProjectionModel::Projected => "projection.projected",
        ProjectionModel::Unknown => "projection.unknown",
    }
}

/// 状态栏状态（渲染时按当前语言翻译，语言切换即时生效）。
pub(crate) enum Status {
    Ready,
    /// 已翻译的动态消息（添加成功 / 加载失败等）。
    Message(String),
}

/// 菜单项点击后的动作。
pub enum MenuAction {
    /// 无需额外处理（对话框/导入/视图操作已在 [`RustGisApp::on_menu_item`] 内完成）。
    None,
    /// 请求关闭主窗口。
    Exit,
}

/// 主应用状态。
pub struct RustGisApp {
    status: Status,
    /// 当前模态对话框（None 表示无）。
    dialog: Option<Dialog>,
    /// 当前界面语言（启动时跟随系统，可在「帮助 → 语言」切换）。
    pub language: Language,
    /// “关于”对话框中展示的字体信息。
    font_info: String,
    /// 图层集合（v0.2 数据与图层管理）。
    layers: LayerManager,
    /// 地图视图状态。
    map: MapState,
    /// 跨帧复用的地图渲染器（视图不变时直接复用上一帧的形状）。
    map_renderer: map::render::Renderer,
    /// 后台导入流水线（等待文件选择 / 等待解析完成）。
    import: Option<ImportStage>,
    /// 「缓冲区」独立窗口（`Some` 时显示；用户点取消或确定后关闭）。
    buffer_dialog: Option<BufferWindow>,
    /// 后台缓冲流水线（确定之后、结果加入图层之前）。
    buffer_job: Option<BufferJob>,
    /// 「地理处理」独立窗口（裁剪 / 相交 / 联合 / 合并 / 融合共用）。
    geo_dialog: Option<GeoWindow>,
    /// 后台地理处理流水线（确定之后、结果加入图层之前）。
    geo_job: Option<GeoJob>,
    /// 上一帧中央地图区域（缓冲结果落点判断用：视图是否已包含结果范围）。
    map_rect: Option<egui::Rect>,
    /// 下一帧将视图适配到全部数据范围。
    needs_fit: bool,
    /// 鼠标在地图区域内的世界坐标（状态栏显示）。
    cursor_world: Option<[f64; 2]>,
    /// 正在被拖拽的图层（`Some` 时只画落点指示器；真正的移动在鼠标松开时才发生）。
    layer_drag: Option<u64>,
    /// 工程默认投影 / 坐标系（`None` = 未设置）。
    ///
    /// 来源优先级：打开工程时从文件恢复；导入**第一个**图层时（工程尚未
    /// 设置投影且无其他图层）取该图层的投影（产品需求：首个图层定工程默认）；
    /// 新建工程清回 `None`。状态栏实时展示。
    project_projection: Option<ProjectionInfo>,
    /// 当前打开的工程文件路径（`None` = 尚未保存过的新建工程）。
    project_path: Option<PathBuf>,
    /// 工程是否有未保存的更改（图层增删改 / 顺序 / 显隐 / 视图变化）。
    dirty: bool,
    /// 工程后台流水线（打开 / 另存为 / 保存 / 加载）。
    project_op: Option<ProjectStage>,
    /// 用户已确认退出（点了"不保存"或保存完成）：下一帧真正关闭窗口。
    exiting: bool,
    /// 为 `true` 时下一帧刷新标题栏（改变工程名的操作入口拿不到 `ctx` 时用）。
    title_dirty: bool,
}

impl RustGisApp {
    /// 创建应用状态。只依赖 `egui::Context`（不依赖 eframe 的 `CreationContext`），
    /// 因此测试里可以用 `Context::default()` 无头跑整块界面。
    pub fn new(ctx: &egui::Context) -> Self {
        // 整应用白底浅色主题：色板与 `Visuals` 统一收敛在 `style` 模块。
        ctx.set_visuals(style::visuals());

        let language = Language::from_system();

        // 内置字体不含 CJK 字形，必须加载系统中文字体，否则显示为方框。
        let font_info = match fonts::install_system_cjk_font(ctx) {
            Some(path) => format!("{}: {}", language.t("dialog.about.font"), path.display()),
            None => language.t("dialog.about.nofont").to_string(),
        };

        Self {
            status: Status::Ready,
            dialog: None,
            language,
            font_info,
            layers: LayerManager::default(),
            map: MapState::default(),
            map_renderer: map::render::Renderer::default(),
            import: None,
            buffer_dialog: None,
            buffer_job: None,
            geo_dialog: None,
            geo_job: None,
            map_rect: None,
            needs_fit: false,
            cursor_world: None,
            layer_drag: None,
            project_projection: None,
            project_path: None,
            dirty: false,
            project_op: None,
            exiting: false,
            title_dirty: false,
        }
    }

    /// 按键解析当前语言的 UI 字符串（未收录的 key 原样返回）。
    pub fn t<'a>(&self, key: &'a str) -> &'a str {
        self.language.t(key)
    }

    /// 解析含 `{0}` `{1}` ... 占位符的字符串。
    pub fn tf(&self, key: &str, args: &[&str]) -> String {
        self.language.tf(key, args)
    }

    /// 状态栏文本（按当前语言实时翻译）。
    fn status_text(&self) -> String {
        match &self.status {
            Status::Ready => self.t("status.ready").to_string(),
            Status::Message(m) => m.clone(),
        }
    }

    /// 状态栏第三段：`投影: … | 坐标系: … | RustGIS x.y.z`。
    ///
    /// 投影 / 坐标系取自工程默认值（[`RustGisApp::project_projection`]），
    /// 未设置时显示"未设置"。
    fn status_meta_text(&self) -> String {
        // 未设置（空工程 / 旧版本文件）：两段都显示"未设置"；
        // 已设置：投影模型按模型翻译，坐标系按识别结果
        //（`EPSG:<code>` / WKT 名称；未识别时显示"未识别"）。
        let (model, crs) = match &self.project_projection {
            Some(p) => (
                self.t(model_i18n_key(p.model)).to_string(),
                match &p.crs {
                    CrsId::None => self.t("crs.notIdentified").to_string(),
                    other => other.label(),
                },
            ),
            None => (
                self.t("projection.none").to_string(),
                self.t("crs.notSet").to_string(),
            ),
        };
        self.tf(
            "status.meta",
            &[model.as_str(), crs.as_str(), env!("CARGO_PKG_VERSION")],
        )
    }

    /// 切换界面语言（立即生效，含状态栏等已显示文案）。
    pub fn set_language(&mut self, lang: Language) {
        self.language = lang;
        if let Status::Message(_) = self.status {
            self.status = Status::Ready;
        }
    }

    /// 菜单项被点击时的统一入口（`key` 为 i18n key）。
    pub fn on_menu_item(&mut self, key: &str) -> MenuAction {
        match key {
            // ── 文件：工程生命周期（对标 ArcGIS 文件菜单） ──
            "file.new" => {
                self.start_new();
                MenuAction::None
            }
            "file.open" => {
                self.start_open();
                MenuAction::None
            }
            "file.save" => {
                self.start_save();
                MenuAction::None
            }
            "file.saveAs" => {
                self.start_save_as();
                MenuAction::None
            }
            "file.exit" => MenuAction::Exit,
            "help.about" => {
                self.dialog = Some(Dialog::About);
                MenuAction::None
            }
            "layer.add" | "data.import" => {
                self.pick_and_add_layer();
                MenuAction::None
            }
            // 地理处理 → 缓冲区：ArcGIS 风格参数对话框（确定后结果自动加入地图）。
            "overlay.buffer" => {
                self.open_buffer_dialog();
                MenuAction::None
            }
            // 地理处理 → 叠加类工具（裁剪 / 相交 / 联合 / 合并 / 融合）：
            // 与缓冲区同一个对话框骨架，只是参数不同。
            // 菜单 key 与工具的映射由 `OverlayOp::menu_key` 统一给出，
            // 这样菜单表（menus.rs）与这里不会再各写一份字符串。
            key if overlay::OverlayOp::from_menu_key(key).is_some() => {
                let op = overlay::OverlayOp::from_menu_key(key).expect("guard checked");
                self.open_geoprocess_dialog(op);
                MenuAction::None
            }
            "view.zoomIn" => {
                if let Some(v) = self.map.view.as_mut() {
                    v.zoom_center(1.6);
                    self.dirty = true;
                }
                MenuAction::None
            }
            "view.zoomOut" => {
                if let Some(v) = self.map.view.as_mut() {
                    v.zoom_center(1.0 / 1.6);
                    self.dirty = true;
                }
                MenuAction::None
            }
            "view.zoomFull" => {
                self.needs_fit = true;
                self.dirty = true;
                MenuAction::None
            }
            "view.pan" => {
                self.map.tool = Tool::Pan;
                MenuAction::None
            }
            other => {
                // 产品行为约定：未实现功能统一弹出“正在开发中”对话框。
                let feature = self.t(other).to_string();
                self.status = Status::Message(self.tf("status.dev", &[&feature]));
                self.dialog = Some(Dialog::Developing { feature });
                MenuAction::None
            }
        }
    }
}

impl eframe::App for RustGisApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // 后台导入流水线推进（不阻塞主线程）
        self.poll_import(ctx);
        // 后台工程流水线推进（打开 / 保存，不阻塞主线程）
        self.poll_project_op(ctx);
        // 后台缓冲流水线推进（缓冲区分析，不阻塞主线程）
        self.poll_buffer_job(ctx);
        // 后台地理处理流水线推进（裁剪 / 相交 / 联合 / 合并 / 融合）
        self.poll_geo_job(ctx);

        // 退出流程：用户已确认退出（"不保存"或保存完成）→ 真正关闭窗口。
        if self.exiting {
            ctx.send_viewport_cmd(ViewportCommand::Close);
            return;
        }

        // 标题栏 X：有未保存更改时拦截关闭并弹系统"保存更改？"提示；
        // 无更改则不拦截，应用在本帧后正常退出。
        // 提示框已在等待用户选择（project_op 占用）时不再重复弹出。
        if ctx.input(|i| i.viewport().close_requested()) && self.dirty {
            ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            if self.dialog.is_none() && self.project_op.is_none() {
                self.ask_save_changes(AfterSave::Exit);
            }
        }

        // 标题栏刷新（改变工程名的操作入口拿不到 ctx，延迟到下一帧）。
        if self.title_dirty {
            self.title_dirty = false;
            self.refresh_window_title(ctx);
        }

        // 面板骨架：填充色取整应用白底（`style::visuals` 的 `panel_fill`），
        // 另加 1px 发丝线描边做面板分隔（白色界面上不用色块、只用细线）。
        // 注意 `Frame::side_top_panel` 接收的是 `&Style`（不是 `&Visuals`）。
        let panel_style = ctx.style();
        let panel_frame = || {
            egui::Frame::side_top_panel(&panel_style).stroke(style::hairline())
        };

        // 顶部菜单栏
        TopBottomPanel::top("menu_bar")
            .frame(panel_frame())
            .show(ctx, |ui| {
                menus::show_menu_bar(ui, self);
            });

        // 左侧图层管理面板（对标 ArcGIS 图层窗口位置）
        SidePanel::left("layers_panel")
            .min_width(200.0)
            .default_width(260.0)
            .resizable(true)
            .frame(panel_frame())
            .show(ctx, |ui| self.layers_panel(ui));

        // 底部状态栏
        TopBottomPanel::bottom("status_bar")
            .frame(panel_frame())
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(self.status_text());
                    ui.separator();
                    if let Some(w) = self.cursor_world {
                        ui.weak(self.tf(
                            "status.coords",
                            &[&format!("{:.5}", w[0]), &format!("{:.5}", w[1])],
                        ));
                        ui.separator();
                    }
                    ui.weak(self.status_meta_text());
                });
            });

        // 中央地图视图
        CentralPanel::default().show(ctx, |ui| self.map_panel(ui));

        // 模态对话框（位于最上层）；用户点"确定"后关闭。
        // （"保存更改？"提示已改为系统原生对话框，见 ask_save_changes。）
        if let Some(d) = self.dialog.as_ref()
            && dialog::show(ctx, d, self.language, &self.font_info)
        {
            self.dialog = None;
        }

        // 「缓冲区」对话框（非模态窗口：可以一边看地图一边调参数）。
        self.show_buffer_dialog(ctx);

        // 「地理处理」对话框（裁剪 / 相交 / 联合 / 合并 / 融合，同一个窗口）。
        self.show_geoprocess_dialog(ctx);
    }
}
