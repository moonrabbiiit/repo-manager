//! 插件机制：功能装在 `plugin\*.dll` 里，**装没装看文件**，不看配置开关。
//!
//! 双向合并的算法（含 svn 读取、三路合并、备份还原）编译成 `plugin\merge-plugin.dll`，
//! 主程序只用 `plugin_host` 里的 JSON ABI 调它。所以：
//! - 卸载 = 删掉 dll，功能立刻不存在（不是界面上藏一藏）；
//! - 导入 = 把 dll 放回 `plugin\`，面板里点一下即可热加载；
//! - 版本对不上（ABI 不同）会被宿主拒绝，不会把主程序带崩。

use std::path::{Path, PathBuf};

use egui::{RichText, ScrollArea, Vec2};

use crate::ui::WindowCentered;
use crate::RepoApp;

/// 双向合并插件的文件名
pub const MERGE_DLL: &str = "merge-plugin.dll";

/// AI 工作日志插件的文件名
pub const AI_LOG_DLL: &str = "ai-log-plugin.dll";

/// 一个可安装的功能插件
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PluginId {
    /// 本地开发侧 ↔ 服务器端双向合并
    Merge,
    /// 调用 AI 生成工作日志
    AiLog,
}

impl PluginId {
    /// 全部插件，按面板里的展示顺序
    pub const ALL: &'static [PluginId] = &[PluginId::Merge, PluginId::AiLog];

    /// 配置 / 日志里的稳定标识
    pub fn key(self) -> &'static str {
        match self {
            Self::Merge => "merge",
            Self::AiLog => "ai-log",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Merge => "双向合并",
            Self::AiLog => "AI 工作日志",
        }
    }

    /// 插件自己的版本号，和主程序版本各走各的
    pub fn version(self) -> &'static str {
        match self {
            Self::Merge | Self::AiLog => "1.0.0",
        }
    }

    /// 面板里那行说明：讲清装上之后多了什么
    pub fn detail(self) -> &'static str {
        match self {
            Self::Merge => {
                "对本地开发侧与服务器端工作副本逐文件核对：可自动合并的按行合并，需覆盖的整体覆盖，\
                 服务器端删除的文件在本地同步删除。写入前对两侧分别完整备份，支持一键还原。\
                 卸载后合并入口、设置分区与合并窗口不再显示。"
            }
            Self::AiLog => {
                "将提交记录页中勾选的提交整理后提交给 AI，按指定口吻生成工作日志正文。\
                 服务地址、密钥与模型仍在设置中配置，插件仅负责发起请求与解析响应。\
                 卸载后日志模式、勾选栏、生成窗口与设置分区不再显示。"
            }
        }
    }

    /// 插件在 `plugin\` 里的文件名
    pub fn file_name(self) -> &'static str {
        match self {
            Self::Merge => MERGE_DLL,
            Self::AiLog => AI_LOG_DLL,
        }
    }
}

impl RepoApp {
    /// 这个插件装没装：**看 dll 在不在**
    pub(crate) fn plugin_installed(&self, id: PluginId) -> bool {
        self.plugin_present
            .unwrap_or_else(|| self.plugin_file(id).is_file())
    }

    /// 插件文件的实际路径
    pub(crate) fn plugin_file(&self, id: PluginId) -> PathBuf {
        crate::plugin_host::plugin_dir().join(id.file_name())
    }

    /// 导入插件：先就地读一遍源 dll 的自报身份，认得出是哪个插件才复制进 `plugin\`。
    /// 复制走 `.incoming` 中转，验过再换上去——验不过只是白复制一次，
    /// 不会把原来装着的那个好文件顶掉。
    pub(crate) fn import_plugin_file(&mut self, source: &Path) -> Result<PluginId, String> {
        let info = crate::plugin_host::info_at(source)?;
        let key = info.get("id").and_then(|v| v.as_str()).unwrap_or_default();
        let shown = info.get("name").and_then(|v| v.as_str()).unwrap_or_default();
        let id = PluginId::ALL
            .iter()
            .copied()
            .find(|id| id.key() == key)
            .ok_or_else(|| {
                format!(
                    "无法识别该插件（插件自报 id={key:?}{}）",
                    if shown.is_empty() { String::new() } else { format!("，名称“{shown}”") }
                )
            })?;

        let dest = self.plugin_file(id);
        if let Some(dir) = dest.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("无法创建插件目录：{e}"))?;
        }
        let staging = dest.with_extension("dll.incoming");
        let _ = std::fs::remove_file(&staging);
        std::fs::copy(source, &staging).map_err(|e| format!("复制插件文件失败：{e}"))?;

        // 先验中转文件：id 与 ABI 都对得上才换上去
        match crate::plugin_host::info_at(&staging) {
            Ok(info) if info.get("id").and_then(|v| v.as_str()) == Some(id.key()) => {}
            Ok(info) => {
                let _ = std::fs::remove_file(&staging);
                let got = info.get("id").and_then(|v| v.as_str()).unwrap_or_default();
                return Err(format!("该文件不是“{}”插件（插件自报 id={got}）", id.name()));
            }
            Err(e) => {
                let _ = std::fs::remove_file(&staging);
                return Err(e);
            }
        }

        // Windows 下文件被映射着就换不了，句柄必须先放掉
        crate::plugin_host::forget_one(id.file_name());
        if dest.is_file() {
            std::fs::remove_file(&dest)
                .map_err(|e| format!("无法替换旧插件 {}：{e}", dest.display()))?;
        }
        std::fs::rename(&staging, &dest).map_err(|e| {
            let _ = std::fs::remove_file(&staging);
            format!("安装失败（{}）：{e}", dest.display())
        })?;
        Ok(id)
    }

    /// 卸载插件：先把宿主里的句柄放掉，再删文件——文件不在，功能就不存在
    pub(crate) fn uninstall_plugin(&mut self, id: PluginId) -> Result<(), String> {
        let path = self.plugin_file(id);
        crate::plugin_host::forget_one(id.file_name());
        self.close_plugin_windows(id);
        if path.is_file() {
            std::fs::remove_file(&path).map_err(|e| format!("无法删除 {}：{e}", path.display()))?;
        }
        Ok(())
    }

    /// 卸载时把该插件留下的窗口 / 状态收干净，别让窗口还开着却没入口
    fn close_plugin_windows(&mut self, id: PluginId) {
        match id {
            PluginId::Merge => {
                self.merge = None;
            }
            PluginId::AiLog => {
                self.show_worklog = false;
                self.ai_mode = false;
                self.ai_picks.clear();
            }
        }
    }

    // ------------------------------------------------------------ 插件面板

    /// 「插件」独立面板：只列**已经装上的**插件。卸载即从清单消失，
    /// 如需重新启用，通过右上角的「导入插件…」重新载入 dll。
    pub(crate) fn plugins_window(&mut self, ctx: &egui::Context) {
        if !self.show_plugins {
            return;
        }
        let mut open = self.show_plugins;
        let installed: Vec<PluginId> = PluginId::ALL
            .iter()
            .copied()
            .filter(|id| self.plugin_installed(*id))
            .collect();
        let dir = crate::plugin_host::plugin_dir();
        let mut import_requested = false;
        let mut remove_requested: Option<PluginId> = None;
        egui::Window::new("插件")
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            // 两张卡也才 300 出头：窗口别再留一大片空
            .default_size(Vec2::new(620.0, 330.0))
            .max_height(ctx.content_rect().height() - 80.0)
            .min_width(520.0)
            .min_height(200.0)
            .centered(ctx, "plugins")
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("可选功能以插件形式提供，按需安装。").size(13.0));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("导入插件…").clicked() {
                            import_requested = true;
                        }
                        ui.label(
                            RichText::new(format!("已安装 {} 个", installed.len()))
                                .weak()
                                .size(11.5),
                        );
                    });
                });
                ui.label(
                    RichText::new(
                        "插件以独立 dll 文件形式存放于插件目录，文件存在即视为已安装。\
                         卸载将删除对应 dll，相关功能同时停用；如需重新启用，\
                         可通过「导入插件…」重新载入。ABI 版本不匹配的 dll 会被拒绝加载。",
                    )
                    .weak()
                    .size(11.5),
                );
                ui.add_space(4.0);
                ScrollArea::vertical()
                    .id_salt("plugins")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if installed.is_empty() {
                            ui.add_space(16.0);
                            ui.vertical_centered(|ui| {
                                ui.label(RichText::new("当前未安装任何插件").size(12.5));
                                ui.add_space(3.0);
                                ui.label(
                                    RichText::new(
                                        "可通过右上角「导入插件…」选择 dll 文件安装，安装完成后将显示在此处。",
                                    )
                                    .weak()
                                    .size(11.0),
                                );
                            });
                            ui.add_space(16.0);
                        }
                        for &id in &installed {
                            let path = self.plugin_file(id);
                            egui::Frame::new()
                                .fill(crate::ui::pane_fill(ui))
                                .stroke(egui::Stroke::new(1.0, crate::ui::pane_stroke(ui)))
                                .corner_radius(8.0)
                                .inner_margin(egui::Margin::symmetric(12, 9))
                                .show(ui, |ui| {
                                    ui.horizontal(|ui| {
                                        ui.label(RichText::new(id.name()).strong().size(13.0));
                                        ui.label(
                                            RichText::new(format!("v{}", id.version()))
                                                .weak()
                                                .size(10.5),
                                        );
                                        ui.label(
                                            RichText::new("● 已安装")
                                                .size(11.0)
                                                .color(crate::ink(ui, crate::OK)),
                                        );
                                        ui.with_layout(
                                            egui::Layout::right_to_left(egui::Align::Center),
                                            |ui| {
                                                if ui.button("卸载").clicked() {
                                                    remove_requested = Some(id);
                                                }
                                            },
                                        );
                                    });
                                    ui.add_space(2.0);
                                    ui.label(RichText::new(id.detail()).weak().size(11.0));
                                    ui.add_space(2.0);
                                    ui.label(
                                        RichText::new(format!("文件：{}", path.display()))
                                            .weak()
                                            .size(10.5),
                                    );
                                });
                            ui.add_space(6.0);
                        }
                    });
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(format!("插件目录：{}", dir.display()))
                            .weak()
                            .size(10.5),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("打开插件目录").clicked() {
                            let _ = std::fs::create_dir_all(&dir);
                            let _ = std::process::Command::new("explorer").arg(&dir).spawn();
                        }
                    });
                });
            });
        self.show_plugins = open;

        // 文件对话框与删文件都放到窗口画完之后：别在绘制回调里弹系统弹窗
        if let Some(id) = remove_requested {
            match self.uninstall_plugin(id) {
                Ok(()) => self.hint(format!(
                    "已卸载“{}”：对应 dll 文件已删除，相关功能已停用",
                    id.name()
                )),
                Err(e) => self.hint(e),
            }
        }
        if import_requested {
            if let Some(file) = rfd::FileDialog::new()
                .set_title("选择插件 dll")
                .add_filter("插件", &["dll"])
                .pick_file()
            {
                match self.import_plugin_file(&file) {
                    Ok(id) => self.hint(format!(
                        "已导入“{}”：{}",
                        id.name(),
                        self.plugin_file(id).display()
                    )),
                    Err(e) => self.hint(e),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 导入一个不是插件的 dll：拒绝，插件目录里不留任何残渣
    #[test]
    fn a_broken_dll_is_rejected_and_cleaned_up() {
        let _appdata = crate::testbed::lock_appdata();
        let dir = std::env::temp_dir().join("repo_manager_plugin_import_test");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        std::env::set_var("REPO_MANAGER_PLUGIN_DIR", &dir);
        crate::plugin_host::forget();
        let mut app = crate::testbed::stub_app(false);
        app.plugin_present = None;
        let bad = std::env::temp_dir().join("repo_manager_not_a_plugin.dll");
        std::fs::write(&bad, b"definitely not a dll").unwrap();
        let err = app.import_plugin_file(&bad).unwrap_err();
        assert!(err.contains("加载插件失败") || err.contains("不是"), "应说清为什么装不上：{err}");
        let leftovers: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(leftovers.is_empty(), "验不过的 dll 不能留在插件目录里：{leftovers:?}");
        assert!(!app.plugin_installed(PluginId::Merge));
    }

    /// 把插件面板画两帧（第一帧是 Window 的 sizing pass），收下所有文字
    fn panel_texts(app: &mut RepoApp) -> Vec<String> {
        let ctx = egui::Context::default();
        ctx.set_theme(egui::ThemePreference::Dark);
        crate::fonts::install(&ctx, crate::fonts::AUTO);
        app.show_plugins = true;
        let mut texts = Vec::new();
        for _ in 0..2 {
            let mut out =
                ctx.run_ui(crate::testbed::base_input(), |ui| app.plugins_window(ui.ctx()));
            out.textures_delta.clear();
            texts = out
                .shapes
                .iter()
                .filter_map(|item| match &item.shape {
                    egui::Shape::Text(text) => Some(text.galley.text().to_string()),
                    _ => None,
                })
                .collect();
        }
        texts
    }

    /// 装着的才在清单里；没装的不该以「未安装」的形式占位
    #[test]
    fn the_panel_lists_only_what_is_installed() {
        let _appdata = crate::testbed::lock_appdata();
        let mut app = crate::testbed::stub_app(false);
        app.plugin_present = Some(true);
        let texts = panel_texts(&mut app);
        assert!(texts.iter().any(|t| t == "双向合并"), "装了就该列出来：{texts:?}");
        assert!(texts.iter().any(|t| t == "AI 工作日志"));
        assert!(texts.iter().any(|t| t.contains("导入插件…")), "导入按钮要在");
        assert!(texts.iter().any(|t| t == "卸载"));
        assert!(!texts.iter().any(|t| t.contains("未安装")), "清单里不该有未安装项");
        assert!(!texts.iter().any(|t| t.contains("当前未安装任何插件")));
    }

    /// 卸载之后那张卡片当场从清单消失，只剩「导入插件…」这一个出口
    #[test]
    fn the_card_vanishes_once_it_is_removed() {
        let _appdata = crate::testbed::lock_appdata();
        let dir = std::env::temp_dir().join("repo_manager_plugin_list_test");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        std::env::set_var("REPO_MANAGER_PLUGIN_DIR", &dir);
        crate::plugin_host::forget();
        let mut app = crate::testbed::stub_app(false);
        app.plugin_present = None;
        std::fs::write(app.plugin_file(PluginId::Merge), b"stub").unwrap();

        let before = panel_texts(&mut app);
        assert!(before.iter().any(|t| t == "双向合并"), "装着的应出现：{before:?}");
        assert!(!before.iter().any(|t| t == "AI 工作日志"), "没装的不该出现");

        app.uninstall_plugin(PluginId::Merge).unwrap();
        let after = panel_texts(&mut app);
        assert!(!after.iter().any(|t| t == "双向合并"), "卸载后卡片要消失：{after:?}");
        assert!(!after.iter().any(|t| t == "卸载"), "没有插件可以卸载");
        assert!(after.iter().any(|t| t.contains("当前未安装任何插件")));
        assert!(after.iter().any(|t| t.contains("导入插件…")), "还得能导回来");
    }

    /// 卸载 = 真的删文件（连句柄一起放掉），不是把按钮藏起来
    #[test]
    fn removing_the_plugin_deletes_the_dll() {
        let _appdata = crate::testbed::lock_appdata();
        let dir = std::env::temp_dir().join("repo_manager_plugin_remove_test");
        let _ = std::fs::create_dir_all(&dir);
        std::env::set_var("REPO_MANAGER_PLUGIN_DIR", &dir);
        crate::plugin_host::forget();
        let mut app = crate::testbed::stub_app(false);
        app.plugin_present = None;
        let dll = app.plugin_file(PluginId::Merge);
        std::fs::write(&dll, b"stub").unwrap();
        assert!(app.plugin_installed(PluginId::Merge), "文件在 = 已装");

        app.merge = Some(crate::MergePreview { dir: 0, plan: None, ..Default::default() });
        app.uninstall_plugin(PluginId::Merge).unwrap();
        assert!(!dll.exists(), "卸载要把 dll 真的删掉");
        assert!(!app.plugin_installed(PluginId::Merge));
        assert!(app.merge.is_none(), "顺带把开着的合并窗口收掉");
    }

    /// 每个插件的 key / 文件名都要稳定：改一个就等于让老用户的插件「找不到了」
    #[test]
    fn every_key_and_file_name_is_stable() {
        let mut keys: Vec<&str> = PluginId::ALL.iter().map(|id| id.key()).collect();
        let total = keys.len();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), total, "插件键必须唯一");
        for id in PluginId::ALL {
            assert!(id.detail().len() > 20, "面板说明别写空话");
            assert!(id.file_name().ends_with(".dll"), "插件必须是 dll");
        }
        assert_eq!(PluginId::Merge.file_name(), MERGE_DLL);
        assert_eq!(PluginId::AiLog.file_name(), AI_LOG_DLL);
        assert_ne!(
            PluginId::Merge.file_name(),
            PluginId::AiLog.file_name(),
            "两个插件不能共用一个文件"
        );
    }

    /// 移除 AI 日志插件：勾选、日志模式、窗口都收掉，别留下一堆没有出口的状态
    #[test]
    fn removing_the_ai_log_plugin_clears_its_state() {
        let _appdata = crate::testbed::lock_appdata();
        let dir = std::env::temp_dir().join("repo_manager_plugin_ai_remove_test");
        let _ = std::fs::create_dir_all(&dir);
        std::env::set_var("REPO_MANAGER_PLUGIN_DIR", &dir);
        crate::plugin_host::forget();
        let mut app = crate::testbed::stub_app(false);
        app.plugin_present = None;
        let dll = app.plugin_file(PluginId::AiLog);
        std::fs::write(&dll, b"stub").unwrap();
        app.ai_mode = true;
        app.show_worklog = true;

        app.uninstall_plugin(PluginId::AiLog).unwrap();
        assert!(!dll.exists(), "卸载要把 dll 真的删掉");
        assert!(!app.plugin_installed(PluginId::AiLog));
        assert!(!app.ai_mode, "日志模式要跟着关掉");
        assert!(!app.show_worklog, "生成窗口要跟着收起");
        assert!(app.ai_picks.is_empty(), "勾选的提交也要清掉");
    }
}
