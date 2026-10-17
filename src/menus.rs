//! 菜单栏定义与渲染。
//!
//! 菜单结构对标 ArcGIS（见产品文档 §7）。「文件」（新建 / 打开 / 保存 / 另存为 /
//! 退出）、「视图」（放大 / 缩小 / 平移 / 缩放至全图）、「图层 / 数据」（添加图层）、
//! 「帮助」（语言 / 关于）为真实功能；其余菜单项点击后弹出“正在开发中”对话框。
//!
//! 菜单标签以 i18n key 存储，渲染时按当前语言解析（[`crate::i18n::Language::t`]）。

use egui::{menu, Ui};

use crate::app::{MenuAction, RustGisApp};
use crate::i18n::Language;

/// 单个菜单条目的静态描述。
#[derive(Clone, Copy)]
pub enum Entry {
    /// 可点击的菜单项（i18n key）。
    Item(&'static str),
    /// 分隔线。
    Separator,
    /// 「语言」子菜单（用于切换界面语言）。
    Language,
}

/// 一个下拉菜单（`label` 为 i18n key）。
#[derive(Clone, Copy)]
pub struct MenuDef {
    pub label: &'static str,
    pub entries: &'static [Entry],
}

/// 完整菜单栏（顺序即显示顺序）。
pub static MENUS: &[MenuDef] = &[
    MenuDef {
        label: "menu.file",
        entries: &[
            Entry::Item("file.new"),
            Entry::Item("file.open"),
            Entry::Item("file.save"),
            Entry::Item("file.saveAs"),
            Entry::Separator,
            Entry::Item("file.pageSetup"),
            Entry::Item("file.print"),
            Entry::Separator,
            Entry::Item("file.exit"),
        ],
    },
    MenuDef {
        label: "menu.edit",
        entries: &[
            Entry::Item("edit.undo"),
            Entry::Item("edit.redo"),
            Entry::Separator,
            Entry::Item("edit.cut"),
            Entry::Item("edit.copy"),
            Entry::Item("edit.paste"),
            Entry::Item("edit.selectAll"),
            Entry::Separator,
            Entry::Item("edit.properties"),
        ],
    },
    MenuDef {
        label: "menu.view",
        entries: &[
            Entry::Item("view.zoomIn"),
            Entry::Item("view.zoomOut"),
            Entry::Item("view.pan"),
            Entry::Item("view.zoomFull"),
            Entry::Separator,
            Entry::Item("view.dataFrame"),
            Entry::Item("view.layout"),
            Entry::Separator,
            Entry::Item("view.basemap"),
            Entry::Item("view.bookmarks"),
        ],
    },
    MenuDef {
        label: "menu.layer",
        entries: &[
            Entry::Item("layer.add"),
            Entry::Item("layer.addRaster"),
            Entry::Separator,
            Entry::Item("layer.order"),
            Entry::Item("layer.properties"),
            Entry::Item("layer.display"),
        ],
    },
    MenuDef {
        label: "menu.data",
        entries: &[
            Entry::Item("data.connect"),
            Entry::Item("data.online"),
            Entry::Separator,
            Entry::Item("data.import"),
            Entry::Item("data.export"),
            Entry::Separator,
            Entry::Item("data.table"),
            Entry::Item("data.manage"),
        ],
    },
    MenuDef {
        label: "menu.geoprocessing",
        entries: &[
            // 缓冲区（ArcGIS 的 Buffer）：独立参数对话框，先于其余工具落地。
            Entry::Item("overlay.buffer"),
            Entry::Separator,
            // 叠加分析工具箱（ArcGIS 的 Overlay toolset）：五个工具共用同一个
            // 窗口骨架（`app::geoprocess_dialog`），菜单项只是决定打开时选中哪个。
            Entry::Item("overlay.clip.menu"),
            Entry::Item("overlay.intersect.menu"),
            Entry::Item("overlay.union.menu"),
            Entry::Item("overlay.merge.menu"),
            Entry::Item("overlay.dissolve.menu"),
            Entry::Separator,
            // 尚未实现的空间分析工具（占位：统一走「功能开发中」提示）。
            Entry::Item("analysis.siting"),
            Entry::Item("analysis.network"),
            Entry::Item("analysis.raster"),
            Entry::Item("analysis.stats"),
            Entry::Item("analysis.3d"),
        ],
    },
    MenuDef {
        label: "menu.map",
        entries: &[
            Entry::Item("map.symbology"),
            Entry::Item("map.labels"),
            Entry::Item("map.legend"),
            Entry::Item("map.scale"),
            Entry::Separator,
            Entry::Item("map.properties"),
        ],
    },
    MenuDef {
        label: "menu.tools",
        entries: &[
            Entry::Item("tools.geoprocessing"),
            Entry::Item("tools.modelBuilder"),
            Entry::Item("tools.toolbox"),
            Entry::Separator,
            Entry::Item("tools.options"),
        ],
    },
    MenuDef {
        label: "menu.help",
        entries: &[
            Entry::Item("help.help"),
            Entry::Item("help.docs"),
            Entry::Separator,
            Entry::Language,
            Entry::Item("help.about"),
        ],
    },
];

/// 在 `ui` 中渲染整条菜单栏，并把点击事件派发给 `app`。
pub fn show_menu_bar(ui: &mut Ui, app: &mut RustGisApp) {
    menu::bar(ui, |ui| {
        for menu in MENUS {
            // 先取出拥有权的标签，避免与闭包对 app 的可变借用冲突。
            let label = app.t(menu.label).to_string();
            ui.menu_button(label, |ui| {
                for entry in menu.entries {
                    match entry {
                        Entry::Separator => {
                            ui.separator();
                        }
                        Entry::Item(key) => {
                            if ui.button(app.t(key)).clicked() {
                                match app.on_menu_item(key) {
                                    MenuAction::None => {}
                                    // 退出前检查未保存更改（可能弹出保存提示）。
                                    MenuAction::Exit => app.request_exit(),
                                }
                            }
                        }
                        Entry::Language => {
                            let lang_label = app.t("help.language").to_string();
                            ui.menu_button(lang_label, |ui| {
                                for lang in Language::ALL {
                                    let mut checked = app.language == lang;
                                    if ui
                                        .checkbox(&mut checked, lang.native_name())
                                        .changed()
                                    {
                                        app.set_language(lang);
                                    }
                                }
                            });
                        }
                    }
                }
            });
        }
    });
}
