//! 系统字体加载。
//!
//! eframe/egui 的内置字体不包含 CJK 字形，直接显示中文/日文会渲染为方框。
//! 本模块在启动时按平台探测并加载系统中文字体（追加到等宽比例字体族，
//! 作为内置字体缺字形时的后备），保证多语言文本正常渲染。

use std::path::PathBuf;
use std::sync::Arc;

use egui::{FontData, FontDefinitions, FontFamily};

/// 在 `ctx` 上安装系统中文字体。
///
/// 返回成功加载的字体文件路径（用于“关于”对话框展示）；
/// 未找到可用字体时返回 `None`（界面仍可用，但 CJK 文本会显示为方框）。
pub fn install_system_cjk_font(ctx: &egui::Context) -> Option<PathBuf> {
    let (path, bytes) = find_cjk_font()?;
    let mut fonts = FontDefinitions::default();
    fonts.font_data.insert(
        "rustgis_cjk".to_owned(),
        Arc::new(FontData::from_owned(bytes)),
    );
    // 追加到 Proportional 字体族末尾：内置字体优先，缺字形时回退到中文字体。
    fonts
        .families
        .get_mut(&FontFamily::Proportional)
        .unwrap()
        .push("rustgis_cjk".to_owned());
    ctx.set_fonts(fonts);
    Some(path)
}

/// 按平台依次探测候选字体文件，返回第一个可读取的。
fn find_cjk_font() -> Option<(PathBuf, Vec<u8>)> {
    for path in candidates() {
        if let Ok(bytes) = std::fs::read(&path) && !bytes.is_empty() {
            return Some((path, bytes));
        }
    }
    None
}

/// 各平台的常见 CJK 字体候选（按优先级排序）。
fn candidates() -> Vec<PathBuf> {
    let paths = [
        // Windows
        "C:/Windows/Fonts/msyh.ttc",     // 微软雅黑
        "C:/Windows/Fonts/msyh.ttf",
        "C:/Windows/Fonts/simhei.ttf",   // 黑体
        "C:/Windows/Fonts/simkai.ttf",   // 楷体
        "C:/Windows/Fonts/msjh.ttc",     // 微软正黑（繁中）
        // macOS
        "/System/Library/Fonts/PingFang.ttc",            // 苹方
        "/System/Library/Fonts/STHeiti Light.ttc",       // 华文黑体
        "/System/Library/Fonts/Hiragino Sans GB.ttc",    // 冬青黑
        // Linux
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",
        "/usr/share/fonts/wqy-microhei/wqy-microhei.ttc",
        "/usr/share/fonts/truetype/droid/DroidSansFallbackFull.ttf",
        "/usr/share/fonts/google-noto-cjk/NotoSansCJK-Regular.ttc",
    ];
    paths.iter().map(PathBuf::from).collect()
}
