//! 构建脚本（仅 Windows 生效）：把应用 manifest 嵌入最终 exe。
//!
//! manifest 声明依赖 Common Controls 6.0（ComCtl32.dll v6）。没有它，进程默认
//! 加载 ComCtl32 v5，`TaskDialogIndirect` 不存在，rfd 启用 `common-controls-v6`
//! 后连程序都无法启动。嵌入 v6 manifest 后，"保存更改？"提示即可用系统原生
//! TaskDialog 显示自定义按钮文案（保存 / 不保存 / 取消）。
//!
//! 非 Windows 平台无此步骤（embed-resource 也只声明为 Windows 构建依赖）。
fn main() {
    #[cfg(windows)]
    embed_resource::compile("resources/rustgis.manifest.rc", embed_resource::NONE)
        .manifest_required()
        .expect("嵌入应用 manifest 失败");
}
