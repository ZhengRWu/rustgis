//! Searchable toolbox and asynchronous raster-mask extraction window.
use super::{RustGisApp, Status};
use crate::{
    geo::LayerNode,
    raster::{
        extract_mask::{self, MaskSource},
        mosaic::{self, MosaicOperator, MosaicRequest},
    },
    style,
};
use egui::{Context, RichText, Ui};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering},
        mpsc::{self, Receiver, TryRecvError},
    },
    time::Duration,
};

#[derive(Default)]
pub(super) struct Toolbox {
    pub open: bool,
    query: String,
    category: usize,
}

struct ToolEntry {
    name: &'static str,
    description: &'static str,
    category: usize,
    action: &'static str,
    keywords: &'static str,
}
const CATEGORIES: [&str; 5] = [
    "全部工具",
    "叠加分析",
    "数据管理",
    "栅格提取",
    "栅格数据管理",
];
const TOOLS: [ToolEntry; 8] = [
    ToolEntry {
        name: "裁剪",
        description: "以面要素为边界，保留输入要素在边界内的部分。",
        category: 1,
        action: "overlay.clip.menu",
        keywords: "clip 裁切",
    },
    ToolEntry {
        name: "相交",
        description: "提取输入图层共同覆盖的区域，并组合要素属性。",
        category: 1,
        action: "overlay.intersect.menu",
        keywords: "intersect intersection 交集",
    },
    ToolEntry {
        name: "联合",
        description: "叠加面图层，保留全部覆盖区域及相应属性。",
        category: 1,
        action: "overlay.union.menu",
        keywords: "union 并集",
    },
    ToolEntry {
        name: "合并",
        description: "将多个图层的要素汇集到一个新图层，不消除重叠边界。",
        category: 2,
        action: "overlay.merge.menu",
        keywords: "merge",
    },
    ToolEntry {
        name: "融合",
        description: "按字段分组，消除同组面要素之间的内部边界。",
        category: 2,
        action: "overlay.dissolve.menu",
        keywords: "dissolve",
    },
    ToolEntry {
        name: "缓冲区",
        description: "在输入要素周围按指定距离生成缓冲区域。",
        category: 1,
        action: "overlay.buffer",
        keywords: "buffer",
    },
    ToolEntry {
        name: "按掩膜提取",
        description: "用面要素或栅格的有效区域提取栅格，区域外设为 NoData。",
        category: 3,
        action: "raster.extractMask",
        keywords: "extract by mask raster nodata 掩膜 提取 栅格",
    },
    ToolEntry {
        name: "镶嵌至新栅格",
        description: "将多个栅格合并为一个新的 GeoTIFF，并可选择重叠像元的合成方式。",
        category: 4,
        action: "raster.mosaicToNewRaster",
        keywords: "mosaic raster merge combine 镶嵌 合并 栅格",
    },
];

fn matches(tool: &ToolEntry, category: usize, query: &str) -> bool {
    (category == 0 || tool.category == category)
        && query.split_whitespace().all(|word| {
            format!(
                "{} {} {} {}",
                tool.name, tool.description, tool.keywords, CATEGORIES[tool.category]
            )
            .to_lowercase()
            .contains(&word.to_lowercase())
        })
}

#[derive(Default)]
pub(super) struct MaskDialog {
    pub open: bool,
    input: Option<u64>,
    mask: Option<u64>,
    output: String,
    message: Option<String>,
    browse: Option<Receiver<Option<PathBuf>>>,
}
pub(super) struct MaskJob {
    rx: Receiver<Result<PathBuf, String>>,
    cancel: Arc<AtomicBool>,
    progress: Arc<AtomicU32>,
}

#[derive(Default)]
pub(super) struct MosaicDialog {
    pub open: bool,
    inputs: Vec<u64>,
    output: String,
    operator: usize,
    message: Option<String>,
    browse: Option<Receiver<Option<PathBuf>>>,
}
pub(super) struct MosaicJob {
    rx: Receiver<Result<PathBuf, String>>,
    cancel: Arc<AtomicBool>,
    progress: Arc<AtomicU32>,
}
impl Drop for MosaicJob {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}
impl Drop for MaskJob {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

#[derive(Clone)]
struct Choice {
    id: u64,
    name: String,
    raster: bool,
}
fn choices(nodes: &[LayerNode], out: &mut Vec<Choice>) {
    for node in nodes {
        match node {
            LayerNode::Group(g) => choices(&g.children, out),
            LayerNode::Raster(r) => out.push(Choice {
                id: r.id,
                name: r.name.clone(),
                raster: true,
            }),
            LayerNode::Layer(l) if !l.outer_rings.is_empty() => out.push(Choice {
                id: l.id,
                name: l.name.clone(),
                raster: false,
            }),
            _ => {}
        }
    }
}
fn choose(ui: &mut Ui, id: &str, selected: &mut Option<u64>, list: &[Choice], raster_only: bool) {
    let label = list
        .iter()
        .find(|c| Some(c.id) == *selected)
        .map(|c| c.name.as_str())
        .unwrap_or("请选择图层");
    egui::ComboBox::from_id_salt(id)
        .width(370.)
        .selected_text(label)
        .show_ui(ui, |ui| {
            for c in list.iter().filter(|c| !raster_only || c.raster) {
                ui.selectable_value(
                    selected,
                    Some(c.id),
                    format!("{} · {}", c.name, if c.raster { "栅格" } else { "面要素" }),
                );
            }
        });
}

impl RustGisApp {
    pub(super) fn show_toolbox(&mut self, ctx: &Context) {
        if !self.toolbox.open {
            return;
        }
        let mut state = std::mem::take(&mut self.toolbox);
        let mut action = None;
        let mut open = true;
        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("rustgis_toolbox"),
            egui::ViewportBuilder::default()
                .with_title("工具箱 — RustGIS")
                .with_inner_size([720., 530.])
                .with_min_inner_size([520., 340.]),
            |ctx, class| {
                let mut content = |ui: &mut Ui| {
                    ui.add_space(6.);
                    ui.horizontal(|ui| {
                        ui.heading("工具箱");
                        ui.label(RichText::new("地理处理与栅格分析").color(style::TEXT_WEAK));
                    });
                    ui.add_space(8.);
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut state.query)
                                .hint_text("搜索工具名称、简介或关键词…")
                                .desired_width((ui.available_width() - 56.).max(100.)),
                        );
                        if ui.button("清空").clicked() {
                            state.query.clear();
                        }
                    });
                    ui.add_space(8.);
                    ui.separator();
                    ui.horizontal_top(|ui| {
                        ui.vertical(|ui| {
                            ui.set_width(118.);
                            ui.add_space(6.);
                            for (i, name) in CATEGORIES.iter().enumerate() {
                                let count =
                                    TOOLS.iter().filter(|t| i == 0 || t.category == i).count();
                                ui.selectable_value(
                                    &mut state.category,
                                    i,
                                    format!("{name}  {count}"),
                                );
                                ui.add_space(5.);
                            }
                        });
                        ui.separator();
                        ui.vertical(|ui| {
                            let found: Vec<_> = TOOLS
                                .iter()
                                .filter(|t| matches(t, state.category, &state.query))
                                .collect();
                            ui.label(format!(
                                "{} · {} 个工具",
                                CATEGORIES[state.category],
                                found.len()
                            ));
                            egui::ScrollArea::vertical()
                                .id_salt("toolbox_results")
                                .show(ui, |ui| {
                                    if found.is_empty() {
                                        ui.add_space(30.);
                                        ui.label("没有找到匹配的工具，请尝试其他关键词或分类。");
                                    }
                                    for tool in found {
                                        ui.add_space(6.);
                                        egui::Frame::new()
                                            .fill(style::WHITE)
                                            .stroke(style::hairline())
                                            .corner_radius(3)
                                            .inner_margin(10)
                                            .show(ui, |ui| {
                                                ui.set_min_width(
                                                    (ui.available_width() - 4.).max(100.),
                                                );
                                                ui.horizontal(|ui| {
                                                    ui.label(
                                                        RichText::new(tool.name)
                                                            .strong()
                                                            .color(style::ACCENT),
                                                    );
                                                    ui.with_layout(
                                                        egui::Layout::right_to_left(
                                                            egui::Align::Center,
                                                        ),
                                                        |ui| {
                                                            if ui.button("打开").clicked() {
                                                                action = Some(tool.action);
                                                            }
                                                        },
                                                    );
                                                });
                                                ui.add_space(4.);
                                                ui.label(tool.description);
                                                ui.label(
                                                    RichText::new(CATEGORIES[tool.category])
                                                        .small()
                                                        .color(style::TEXT_WEAK),
                                                );
                                            });
                                    }
                                });
                        });
                    });
                };
                if class == egui::ViewportClass::Embedded {
                    egui::Window::new("工具箱")
                        .open(&mut open)
                        .default_size([720., 530.])
                        .show(ctx, &mut content);
                } else {
                    egui::CentralPanel::default().show(ctx, &mut content);
                    if ctx.input(|i| i.viewport().close_requested()) {
                        open = false;
                    }
                }
            },
        );
        state.open = open;
        self.toolbox = state;
        if let Some(key) = action {
            self.on_menu_item(key);
        }
    }

    pub(super) fn show_mask_dialog(&mut self, ctx: &Context) {
        if !self.mask_dialog.open {
            return;
        }
        let mut state = std::mem::take(&mut self.mask_dialog);
        let mut list = Vec::new();
        choices(self.layers.roots(), &mut list);
        if !list.iter().any(|c| Some(c.id) == state.input && c.raster) {
            state.input = None;
        }
        if !list.iter().any(|c| Some(c.id) == state.mask) {
            state.mask = None;
        }
        if let Some(rx) = &state.browse {
            match rx.try_recv() {
                Ok(path) => {
                    if let Some(path) = path {
                        state.output = path.display().to_string();
                    }
                    state.browse = None;
                }
                Err(TryRecvError::Disconnected) => state.browse = None,
                Err(TryRecvError::Empty) => ctx.request_repaint_after(Duration::from_millis(50)),
            }
        }
        let busy = self.mask_job.is_some();
        let ready = self.project_idle() && self.raster_stage.is_none();
        let mut run = false;
        let mut open = true;
        ctx.show_viewport_immediate(egui::ViewportId::from_hash_of("rustgis_extract_mask"),
            egui::ViewportBuilder::default().with_title("按掩膜提取 — RustGIS").with_inner_size([650.,530.]).with_min_inner_size([480.,400.]),|ctx,class| {
            let mut content=|ui:&mut Ui| {
                egui::ScrollArea::vertical().show(ui,|ui| {
                    ui.heading("按掩膜提取");ui.label("保留掩膜内的栅格像元，提取结果另存为 GeoTIFF。");
                    ui.add_space(10.);ui.separator();
                    ui.add_enabled_ui(!busy,|ui| {
                        ui.label("输入栅格");choose(ui,"mask_input",&mut state.input,&list,true);
                        ui.add_space(10.);ui.label("掩膜图层（面要素 / 栅格）");choose(ui,"mask_layer",&mut state.mask,&list,false);
                        ui.add_space(10.);ui.label("输出 GeoTIFF");
                        ui.horizontal(|ui| {
                            ui.add(egui::TextEdit::singleline(&mut state.output).hint_text("选择新的 .tif 文件路径").desired_width((ui.available_width()-65.).max(100.)));
                            if ui.add_enabled(state.browse.is_none(),egui::Button::new("浏览…")).clicked() {
                                let (tx,rx)=mpsc::channel();state.browse=Some(rx);
                                std::thread::spawn(move|| {let path=rfd::FileDialog::new().set_title("提取结果另存为").set_file_name("extract_mask.tif").add_filter("GeoTIFF",&["tif","tiff"]).save_file();let _=tx.send(path);});
                            }
                        });
                    });
                    ui.add_space(14.);
                    egui::Frame::new().fill(style::SOFT_BG).inner_margin(10).show(ui,|ui| {
                        ui.label(RichText::new("处理规则").strong());
                        ui.label("• 保留输入的全部波段、像元大小与网格对齐；输出范围为掩膜与输入的重叠范围。");
                        ui.label("• 面掩膜按像元中心判断，支持多面与孔洞。栅格掩膜使用第 1 波段，非 NoData 像元（包括 0）均有效。");
                        ui.label("• 输入和掩膜会自动转换到输入栅格坐标系（支持已识别的经纬度、Web Mercator、UTM 和高斯–克吕格坐标系）。输出为 Float32 GeoTIFF，掩膜外与原始无效值写为 NoData（NaN）；高精度数值可能舍入。");
                        ui.label("• 输出使用新文件名，不覆盖现有文件；完成后自动加入地图。");
                    });
                    ui.add_space(12.);
                    if let Some(message)=&state.message {ui.label(message);}
                    if let Some(job)=&self.mask_job {
                        ui.add(egui::ProgressBar::new(job.progress.load(Ordering::Relaxed) as f32/1000.).show_percentage());
                        if ui.add_enabled(!job.cancel.load(Ordering::Relaxed),egui::Button::new("取消处理")).clicked() {
                            job.cancel.store(true,Ordering::Relaxed);state.message=Some("正在取消…".into());
                        }
                    } else {
                        ui.horizontal(|ui| {
                            if ui.add_enabled(ready && state.input.is_some() && state.mask.is_some() && !state.output.trim().is_empty() && state.browse.is_none(),egui::Button::new("运行").fill(style::ACCENT_ACTIVE_BG)).clicked() {run=true;}
                            if !ready {ui.label("请等待当前导入或工程操作完成。");}
                        });
                    }
                });
            };
            if class==egui::ViewportClass::Embedded {
                egui::Window::new("按掩膜提取").open(&mut open).default_size([650.,530.]).show(ctx,&mut content);
            } else {
                egui::CentralPanel::default().show(ctx,&mut content);
                if ctx.input(|i|i.viewport().close_requested()) {open=false;}
            }
        });
        state.open = open;
        self.mask_dialog = state;
        if run {
            self.start_mask_job();
        }
    }

    fn start_mask_job(&mut self) {
        let request =
            (|| -> Result<_, String> {
                let input = self
                    .mask_dialog
                    .input
                    .and_then(|id| self.layers.node(id))
                    .and_then(LayerNode::raster)
                    .ok_or("请选择有效的输入栅格。")?
                    .path
                    .clone();
                let node = self
                    .mask_dialog
                    .mask
                    .and_then(|id| self.layers.node(id))
                    .ok_or("请选择有效的掩膜图层。")?;
                let mask = match node {
                    LayerNode::Layer(l) => MaskSource::Polygons(l.clone()),
                    LayerNode::Raster(r) => MaskSource::Raster(r.path.clone()),
                    _ => return Err("掩膜必须是面或栅格图层。".into()),
                };
                let path = PathBuf::from(self.mask_dialog.output.trim());
                if !path.extension().and_then(|s| s.to_str()).is_some_and(|s| {
                    s.eq_ignore_ascii_case("tif") || s.eq_ignore_ascii_case("tiff")
                }) {
                    return Err("输出文件扩展名应为 .tif 或 .tiff。".into());
                }
                if path.exists() {
                    return Err("输出文件已存在，请选择新的文件名。".into());
                }
                Ok((input, mask, path))
            })();
        match request {
            Err(e) => self.mask_dialog.message = Some(e),
            Ok((input, mask, path)) => {
                let (tx, rx) = mpsc::channel();
                let cancel = Arc::new(AtomicBool::new(false));
                let progress = Arc::new(AtomicU32::new(0));
                let c = cancel.clone();
                let p = progress.clone();
                std::thread::spawn(move || {
                    let result = extract_mask::extract(&input, mask, &path, &c, &p).map(|()| path);
                    let _ = tx.send(result);
                });
                self.mask_job = Some(MaskJob {
                    rx,
                    cancel,
                    progress,
                });
                self.mask_dialog.message =
                    Some("正在提取，可继续浏览地图；关闭此窗口不会中止处理。".into());
            }
        }
    }

    pub(super) fn poll_mask_job(&mut self, ctx: &Context) {
        // Do not replace an unrelated raster import started by a file drop.
        if self.mask_job.is_some() && self.raster_stage.is_some() {
            ctx.request_repaint_after(Duration::from_millis(50));
            return;
        }
        let Some(job) = self.mask_job.take() else {
            return;
        };
        match job.rx.try_recv() {
            Ok(Ok(path)) => {
                self.mask_dialog.message = Some(format!("提取完成：{}", path.display()));
                self.start_raster_open(path);
            }
            Ok(Err(e)) => {
                self.status = Status::Message(e.clone());
                self.mask_dialog.message = Some(e);
                self.mask_dialog.open = true;
            }
            Err(TryRecvError::Disconnected) => {
                self.mask_dialog.message = Some("处理线程意外结束，请重试。".into());
                self.mask_dialog.open = true;
            }
            Err(TryRecvError::Empty) => {
                self.mask_job = Some(job);
                ctx.request_repaint_after(Duration::from_millis(50));
            }
        }
    }

    pub(super) fn show_mosaic_dialog(&mut self, ctx: &Context) {
        if !self.mosaic_dialog.open {
            return;
        }
        let mut state = std::mem::take(&mut self.mosaic_dialog);
        let mut list = Vec::new();
        choices(self.layers.roots(), &mut list);
        list.retain(|c| c.raster);
        state.inputs.retain(|id| list.iter().any(|c| c.id == *id));
        if let Some(rx) = &state.browse {
            match rx.try_recv() {
                Ok(p) => {
                    if let Some(p) = p {
                        state.output = p.display().to_string();
                    }
                    state.browse = None;
                }
                Err(TryRecvError::Disconnected) => state.browse = None,
                Err(TryRecvError::Empty) => ctx.request_repaint_after(Duration::from_millis(50)),
            }
        }
        let busy = self.mosaic_job.is_some();
        let ready = self.project_idle() && self.raster_stage.is_none();
        let mut run = false;
        let mut open = true;
        ctx.show_viewport_immediate(egui::ViewportId::from_hash_of("rustgis_mosaic_to_new_raster"),egui::ViewportBuilder::default().with_title("镶嵌至新栅格 — RustGIS").with_inner_size([650.,560.]).with_min_inner_size([500.,440.]),|ctx,class|{
            let mut content=|ui:&mut Ui|{egui::ScrollArea::vertical().show(ui,|ui|{
                ui.heading("镶嵌至新栅格");ui.label("将多个栅格合成为一个新的 GeoTIFF。");ui.add_space(10.);ui.separator();
                ui.add_enabled_ui(!busy,|ui|{
                    ui.label("输入栅格（勾选多个，顺序用于 FIRST / LAST）");
                    egui::ScrollArea::vertical().max_height(150.).show(ui,|ui|{for c in &list{let mut checked=state.inputs.contains(&c.id);if ui.checkbox(&mut checked,&c.name).changed(){if checked{state.inputs.push(c.id)}else{state.inputs.retain(|id|*id!=c.id)}}}});
                    ui.add_space(8.);ui.label("镶嵌运算符");egui::ComboBox::from_id_salt("mosaic_operator").selected_text(MosaicOperator::ALL.get(state.operator).copied().unwrap_or(MosaicOperator::First).label()).show_ui(ui,|ui|for (i,op) in MosaicOperator::ALL.iter().enumerate(){ui.selectable_value(&mut state.operator,i,op.label());});
                    ui.add_space(8.);ui.label("输出 GeoTIFF");ui.horizontal(|ui|{ui.add(egui::TextEdit::singleline(&mut state.output).hint_text("选择新的 .tif 文件路径").desired_width((ui.available_width()-65.).max(100.)));if ui.add_enabled(state.browse.is_none(),egui::Button::new("浏览…")).clicked(){let (tx,rx)=mpsc::channel();state.browse=Some(rx);std::thread::spawn(move||{let p=rfd::FileDialog::new().set_title("镶嵌结果另存为").set_file_name("mosaic.tif").add_filter("GeoTIFF",&["tif","tiff"]).save_file();let _=tx.send(p);});}});
                });
                ui.add_space(12.);egui::Frame::new().fill(style::SOFT_BG).inner_margin(10).show(ui,|ui|{ui.label(RichText::new("处理规则").strong());ui.label("• 输出坐标系、像元大小和网格对齐采用第一个输入栅格；其他输入按最近邻方式采样并转换到该坐标系。");ui.label("• 输出波段数取所有输入中的最大值，输出类型为 Float32 GeoTIFF；缺失波段及 NoData 像元写为 NoData。");ui.label("• 支持 FIRST、LAST、BLEND、MINIMUM、MAXIMUM、MEAN 和 SUM；BLEND 按影像边缘距离加权。");ui.label("• 暂不支持调色板映射。输出使用新文件名，不覆盖现有文件；完成后自动加入地图。");});
                ui.add_space(10.);if let Some(m)=&state.message{ui.label(m);}if let Some(job)=&self.mosaic_job{ui.add(egui::ProgressBar::new(job.progress.load(Ordering::Relaxed) as f32/1000.).show_percentage());if ui.add_enabled(!job.cancel.load(Ordering::Relaxed),egui::Button::new("取消处理")).clicked(){job.cancel.store(true,Ordering::Relaxed);state.message=Some("正在取消…".into());}}else{ui.horizontal(|ui|{if ui.add_enabled(ready&&!state.inputs.is_empty()&&!state.output.trim().is_empty()&&state.browse.is_none(),egui::Button::new("运行").fill(style::ACCENT_ACTIVE_BG)).clicked(){run=true;}if !ready{ui.label("请等待当前导入或工程操作完成。");}});}
            });};
            if class==egui::ViewportClass::Embedded{egui::Window::new("镶嵌至新栅格").open(&mut open).default_size([650.,560.]).show(ctx,&mut content);}else{egui::CentralPanel::default().show(ctx,&mut content);if ctx.input(|i|i.viewport().close_requested()){open=false;}}
        });
        state.open = open;
        self.mosaic_dialog = state;
        if run {
            self.start_mosaic_job();
        }
    }

    fn start_mosaic_job(&mut self) {
        let prepared = (|| -> Result<_, String> {
            let inputs = self
                .mosaic_dialog
                .inputs
                .iter()
                .filter_map(|id| {
                    self.layers
                        .node(*id)
                        .and_then(LayerNode::raster)
                        .map(|r| r.path.clone())
                })
                .collect::<Vec<_>>();
            if inputs.is_empty() {
                return Err("请选择至少一个输入栅格。".into());
            }
            let output = PathBuf::from(self.mosaic_dialog.output.trim());
            if !output
                .extension()
                .and_then(|s| s.to_str())
                .is_some_and(|s| s.eq_ignore_ascii_case("tif") || s.eq_ignore_ascii_case("tiff"))
            {
                return Err("输出扩展名应为 .tif 或 .tiff。".into());
            }
            if output.exists() {
                return Err("输出文件已存在，请选择新的文件名。".into());
            }
            Ok((inputs, output))
        })();
        match prepared {
            Err(e) => self.mosaic_dialog.message = Some(e),
            Ok((inputs, output)) => {
                let (tx, rx) = mpsc::channel();
                let cancel = Arc::new(AtomicBool::new(false));
                let progress = Arc::new(AtomicU32::new(0));
                let c = cancel.clone();
                let p = progress.clone();
                let operator = MosaicOperator::ALL
                    .get(self.mosaic_dialog.operator)
                    .copied()
                    .unwrap_or(MosaicOperator::First);
                let out = output.clone();
                std::thread::spawn(move || {
                    let result = mosaic::mosaic(MosaicRequest { inputs, output }, operator, &c, &p)
                        .map(|()| out);
                    let _ = tx.send(result);
                });
                self.mosaic_job = Some(MosaicJob {
                    rx,
                    cancel,
                    progress,
                });
                self.mosaic_dialog.message =
                    Some("正在镶嵌，可继续浏览地图；关闭此窗口不会中止处理。".into());
            }
        }
    }

    pub(super) fn poll_mosaic_job(&mut self, ctx: &Context) {
        if self.mosaic_job.is_some() && self.raster_stage.is_some() {
            ctx.request_repaint_after(Duration::from_millis(50));
            return;
        }
        let Some(job) = self.mosaic_job.take() else {
            return;
        };
        match job.rx.try_recv() {
            Ok(Ok(path)) => {
                self.mosaic_dialog.message = Some(format!("镶嵌完成：{}", path.display()));
                self.start_raster_open(path);
            }
            Ok(Err(e)) => {
                self.status = Status::Message(e.clone());
                self.mosaic_dialog.message = Some(e);
                self.mosaic_dialog.open = true;
            }
            Err(TryRecvError::Disconnected) => {
                self.mosaic_dialog.message = Some("处理线程意外结束，请重试。".into());
                self.mosaic_dialog.open = true;
            }
            Err(TryRecvError::Empty) => {
                self.mosaic_job = Some(job);
                ctx.request_repaint_after(Duration::from_millis(50));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn toolbox_search_matches_names_descriptions_and_english_keywords() {
        assert_eq!(TOOLS.iter().filter(|t| matches(t, 0, "mask")).count(), 1);
        assert_eq!(TOOLS.iter().filter(|t| matches(t, 1, "mask")).count(), 0);
        // 「合并」同时命中矢量合并与「镶嵌至新栅格」（描述 / 关键词都含"合并"）。
        let merged: Vec<_> = TOOLS
            .iter()
            .filter(|t| matches(t, 0, "合并"))
            .map(|t| t.action)
            .collect();
        assert_eq!(merged.len(), 2);
        assert!(merged.contains(&"overlay.merge.menu"));
        assert!(merged.contains(&"raster.mosaicToNewRaster"));
        assert!(TOOLS.iter().any(|t| matches(t, 3, "栅格 NoData")));
        assert_eq!(
            TOOLS
                .iter()
                .filter(|t| matches(t, 0, "不存在的工具"))
                .count(),
            0
        );
        for op in crate::geo::overlay::OverlayOp::ALL {
            assert!(TOOLS.iter().any(|t| t.action == op.menu_key()));
        }
    }
}
