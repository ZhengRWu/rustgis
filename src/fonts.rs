//! 字体加载。
//!
//! eframe/egui 的内置字体不包含 CJK 字形，直接显示中文/日文会渲染为方框。
//! 本模块在编译期把阿里巴巴普惠体 3.0（55 Regular，`resources/fonts/`）
//! 通过 `include_bytes!` 嵌入二进制，并在启动时安装为 CJK 字体，
//! 任何平台都无需依赖系统字体即可正常渲染多语言文本。

use std::sync::Arc;

use egui::{FontData, FontDefinitions, FontFamily};

/// 字体注册名（`FontDefinitions::font_data` 的键与字体族成员名）。
const CJK_FONT_ID: &str = "rustgis_cjk";

/// 展示名（用于“关于”对话框）。
const CJK_FONT_NAME: &str = "AlibabaPuHuiTi-3-35-Thin（阿里巴巴普惠体 3.0）";

/// 内嵌的 TTF 字节（编译期读取，文件缺失会直接编译失败）。
static CJK_FONT_BYTES: &[u8] = include_bytes!("../resources/fonts/AlibabaPuHuiTi-3-35-Thin.ttf");

/// 在 `ctx` 上安装内嵌 CJK 字体。
///
/// 返回字体展示名（用于“关于”对话框展示）。
pub fn install_cjk_font(ctx: &egui::Context) -> String {
    let mut fonts = FontDefinitions::default();
    fonts.font_data.insert(
        CJK_FONT_ID.to_owned(),
        Arc::new(FontData::from_owned(CJK_FONT_BYTES.to_vec())),
    );
    // 追加到 Proportional / Monospace 字体族末尾：内置字体优先，缺字形时回退到 CJK 字体。
    for family in [&FontFamily::Proportional, &FontFamily::Monospace] {
        fonts
            .families
            .get_mut(family)
            .unwrap()
            .push(CJK_FONT_ID.to_owned());
    }
    ctx.set_fonts(fonts);
    CJK_FONT_NAME.to_owned()
}
