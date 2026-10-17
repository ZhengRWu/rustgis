//! 模态对话框："正在开发中" / 错误 / "关于 RustGIS"。
//!
//! 所有文案经 i18n 解析（[`crate::i18n::Language`]）。
//! [`show`] 返回用户是否点击了"确定"；未点击时返回 `false`，调用方应保持
//! 对话框打开。
//!
//! 注意："保存更改？"提示不在这里实现，而是走系统原生对话框
//! （见 `app.rs` 的 [`ask_save_changes`]）。
//!
//! [`ask_save_changes`]: crate::app::RustGisApp::ask_save_changes

use egui::{Align2, Context, RichText, Window};

use crate::i18n::Language;

pub enum Dialog {
    /// 功能占位提示：`feature` 为被点击菜单项的（已翻译）名称。
    Developing { feature: String },
    /// 错误提示（如文件加载失败）。
    Error { body: String },
    /// 关于对话框。
    About,
}

/// 在屏幕中央渲染对话框；返回用户是否点击了"确定"。
pub fn show(ctx: &Context, d: &Dialog, lang: Language, font_info: &str) -> bool {
    let title = match d {
        Dialog::Developing { feature } => lang.tf("dialog.developTitle", &[feature]),
        Dialog::Error { .. } => lang.t("dialog.errorTitle").to_string(),
        Dialog::About => lang.t("dialog.aboutTitle").to_string(),
    };

    let mut ok_clicked = false;

    let window = Window::new(title)
        .collapsible(false)
        .resizable(false)
        .min_width(380.0)
        .anchor(Align2::CENTER_CENTER, [0.0, 0.0]);

    let _ = window.show(ctx, |ui| {
        ui.vertical_centered(|ui| {
            match d {
                Dialog::Developing { feature } => {
                    ui.add_space(6.0);
                    ui.strong(lang.tf("dialog.developHeading", &[feature]));
                    ui.add_space(6.0);
                    ui.label(RichText::new(lang.t("dialog.developBody")).weak());
                }
                Dialog::Error { body } => {
                    ui.add_space(6.0);
                    ui.strong(lang.t("dialog.errorTitle"));
                    ui.add_space(6.0);
                    ui.label(RichText::new(body).weak());
                }
                Dialog::About => {
                    ui.add_space(6.0);
                    ui.strong(format!("RustGIS {}", env!("CARGO_PKG_VERSION")));
                    ui.add_space(4.0);
                    ui.label(lang.t("dialog.aboutSubtitle"));
                    ui.add_space(4.0);
                    ui.label(RichText::new(lang.t("dialog.aboutTagline")).weak());
                    ui.label(RichText::new(lang.t("dialog.aboutLicense")).weak());
                    ui.add_space(8.0);
                    ui.label(RichText::new(font_info).weak().small());
                }
            }
            ui.add_space(16.0);
            if ui.button(lang.t("dialog.ok")).clicked() {
                ok_clicked = true;
            }
        });
    });

    ok_clicked
}
