//! RustGIS — 纯 Rust 复刻 ArcGIS 的桌面 GIS 平台。
//!
//! v0.2.1：投影 / 坐标系识别与展示（GeoJSON 默认 EPSG:4326 / Shapefile
//!      `.prj` WKT 识别；首个导入图层定工程默认；状态栏动态显示）
//!      + v0.2 全部（导入 / 渲染 / 图层面板 / 工程文件）。

// Windows 下编译为 GUI 子系统程序：启动时不出现控制台窗口，直接显示主窗体。
#![windows_subsystem = "windows"]

mod app;
mod dialog;
mod fonts;
mod geo;
mod i18n;
mod map;
mod menus;
mod project;
mod style;

fn main() -> eframe::Result<()> {
    // 窗口标题与状态栏的版本号统一取 crate 版本（CARGO_PKG_VERSION），
    // 避免升版时两处硬编码不同步。
    let title = format!("RustGIS {}", env!("CARGO_PKG_VERSION"));
    eframe::run_native(
        &title,
        eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default()
                .with_inner_size([1280.0, 800.0])
                .with_min_inner_size([960.0, 600.0]),
            ..Default::default()
        },
        Box::new(|cc| Ok(Box::new(app::RustGisApp::new(&cc.egui_ctx)))),
    )
}
