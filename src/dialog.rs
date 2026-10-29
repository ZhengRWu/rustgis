//! 模态提示框：统一走**系统原生对话框**（`rfd` 的 `MessageDialog`）。
//!
//! 产品约定：提示 / 错误 / 关于 / 询问类弹框一律使用系统默认样式
//! （Windows 为 ComCtl32 v6 的 TaskDialog、macOS 为 NSAlert、Linux 为 GTK 对话框），
//! 不再用 egui 自绘窗口。原生窗口自带系统图标、系统按钮文案、键盘快捷键与
//! 无障碍支持，观感与其他桌面软件一致；自定义按钮文案依赖 build.rs 嵌入的
//! 应用 manifest（`resources/`）。
//!
//! 弹出方式与「保存更改？」提示（[`crate::app::RustGisApp::ask_save_changes`]）
//! 同一套约定：对话框在后台线程弹出并阻塞该线程，UI 线程每帧非阻塞轮询结果
//! （[`NativeDialog::poll`] / [`Confirm::poll`]），主窗口不卡死、地图不冻结。
//!
//! 内容由 [`Dialog`] 描述（标题 / 正文 / 语义），文案经 i18n 解析
//! （[`crate::i18n::Language`]）；「是 / 否」询问用 [`Confirm`]。

use std::sync::mpsc::{self, Receiver, TryRecvError};

use rfd::{MessageButtons, MessageDialog, MessageDialogResult, MessageLevel};

use crate::i18n::Language;

pub use rfd::MessageLevel as Level;

/// 提示框内容（渲染为系统原生窗口）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dialog {
    /// 功能占位提示：`feature` 为被点击菜单项的（已翻译）名称。
    Developing { feature: String },
    /// 错误提示（如文件加载失败）。
    Error { body: String },
    /// 关于对话框（`font_info` 由调用方传入，放在正文末尾）。
    About,
}

impl Dialog {
    /// 窗口标题。
    fn title(&self, lang: Language) -> String {
        match self {
            Dialog::Developing { feature } => lang.tf("dialog.developTitle", &[feature]),
            Dialog::Error { .. } => lang.t("dialog.errorTitle").to_string(),
            Dialog::About => lang.t("dialog.aboutTitle").to_string(),
        }
    }

    /// 正文（原生对话框只有一个正文块，多段之间用空行分隔）。
    fn text(&self, lang: Language, font_info: &str) -> String {
        match self {
            Dialog::Developing { feature } => format!(
                "{}\n\n{}",
                lang.tf("dialog.developHeading", &[feature]),
                lang.t("dialog.developBody")
            ),
            Dialog::Error { body } => body.clone(),
            Dialog::About => format!(
                "RustGIS {}\n{}\n{}\n{}\n\n{}",
                env!("CARGO_PKG_VERSION"),
                lang.t("dialog.aboutSubtitle"),
                lang.t("dialog.aboutTagline"),
                lang.t("dialog.aboutLicense"),
                font_info
            ),
        }
    }

    /// 图标语义（信息 / 警告 / 错误）。
    fn level(&self) -> MessageLevel {
        match self {
            Dialog::Developing { .. } | Dialog::About => MessageLevel::Info,
            Dialog::Error { .. } => MessageLevel::Error,
        }
    }
}

/// 已在后台线程弹出的系统提示框（只有一个「确定」按钮）。
pub struct NativeDialog {
    /// 弹出时的内容：用于判断应用侧是否已换了一份新内容（换了就等这次关闭后重弹）。
    shown: Dialog,
    /// 用户点「确定」（或关闭窗口）时收到消息。
    rx: Receiver<()>,
}

impl NativeDialog {
    /// 在后台线程弹出系统原生提示框并立即返回（UI 线程不阻塞）。
    pub fn open(d: &Dialog, lang: Language, font_info: &str) -> Self {
        let title = d.title(lang);
        let text = d.text(lang, font_info);
        let level = d.level();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = MessageDialog::new()
                .set_title(title)
                .set_description(text)
                .set_level(level)
                .set_buttons(MessageButtons::Ok)
                .show();
            let _ = tx.send(());
        });
        Self {
            shown: d.clone(),
            rx,
        }
    }

    /// 非阻塞轮询：`true` = 用户已关闭对话框。
    ///
    /// 线程异常退出（`Disconnected`）按已关闭处理：宁可少一次提示，
    /// 也不要让界面永远以为"还有个框开着"。
    pub fn poll(&self) -> bool {
        match self.rx.try_recv() {
            Ok(()) | Err(TryRecvError::Disconnected) => true,
            Err(TryRecvError::Empty) => false,
        }
    }

    /// 仍在展示的内容是否就是 `d`（内容变了说明应重弹）。
    pub fn shows(&self, d: &Dialog) -> bool {
        &self.shown == d
    }
}

/// 系统原生「是 / 否」询问框（如「是否构建金字塔」）。
///
/// 按钮文案走 i18n（自定义按钮，与「保存更改？」提示一致），因此界面语言是
/// 中文时按钮就是「是 / 否」，而非系统的 OK / Cancel。
pub struct Confirm {
    rx: Receiver<bool>,
}

impl Confirm {
    /// 在后台线程弹出询问框并立即返回（UI 线程不阻塞）。
    pub fn open(title: String, text: String, yes: String, no: String, level: MessageLevel) -> Self {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            // 两个自定义按钮：点击分别回传 `Custom(按钮文案)`，按文案反查语义；
            // 关闭窗口 / Esc（`Cancel`）一律按"否"处理。
            let result = MessageDialog::new()
                .set_title(&title)
                .set_description(&text)
                .set_level(level)
                .set_buttons(MessageButtons::OkCancelCustom(yes.clone(), no.clone()))
                .show();
            let answer = matches!(&result, MessageDialogResult::Custom(label) if *label == yes);
            let _ = tx.send(answer);
        });
        Self { rx }
    }

    /// 非阻塞轮询：`Some(true)` = 是，`Some(false)` = 否 / 关闭，`None` = 还在等用户。
    pub fn poll(&self) -> Option<bool> {
        match self.rx.try_recv() {
            Ok(answer) => Some(answer),
            Err(TryRecvError::Disconnected) => Some(false),
            Err(TryRecvError::Empty) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn developing_dialog_carries_heading_and_body() {
        let d = Dialog::Developing {
            feature: "缓冲区分析...".to_string(),
        };
        assert_eq!(d.title(Language::Chinese), "功能开发中 — 缓冲区分析...");
        let text = d.text(Language::Chinese, "");
        assert!(text.starts_with("「缓冲区分析...」正在开发中\n\n"));
        assert!(text.contains("尚未实现"));
        assert!(matches!(d.level(), MessageLevel::Info));
    }

    #[test]
    fn error_dialog_body_is_verbatim() {
        let d = Dialog::Error {
            body: "无法解析 GeoJSON：第 3 行".to_string(),
        };
        assert_eq!(d.title(Language::English), "Error");
        assert_eq!(d.text(Language::English, ""), "无法解析 GeoJSON：第 3 行");
        assert!(matches!(d.level(), MessageLevel::Error));
    }

    #[test]
    fn about_dialog_lists_version_and_font() {
        let text = Dialog::About.text(Language::Chinese, "渲染字体: 阿里巴巴普惠体");
        assert!(text.starts_with(&format!("RustGIS {}", env!("CARGO_PKG_VERSION"))));
        assert!(text.contains("Apache-2.0"));
        assert!(text.ends_with("渲染字体: 阿里巴巴普惠体"));
    }
}
