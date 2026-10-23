//! 设置窗口：svn.exe / Beyond Compare / 主题与自动刷新 / 提交记录 / 版本更新 / AI 日志。

use std::path::PathBuf;

use egui::{RichText, ScrollArea, TextEdit, Vec2};

use crate::{autostart, bcompare, config, fonts};
use crate::ui::WindowCentered;
use crate::jobs::Kind;
use crate::update;
use crate::{APP_VERSION, Level, RepoApp};
use crate::ui::{AlignedRow, field_margin};

impl RepoApp {
    // ------------------------------------------------------------ 设置窗口

    pub(crate) fn settings(&mut self, ctx: &egui::Context) {
        if !self.show_settings {
            return;
        }
        let mut open = self.show_settings;
        let candidates = self.candidates.clone();
        let version = self.version.clone().unwrap_or_else(|| "不可用".into());
        egui::Window::new("设置")
            .open(&mut open)
            .centered(ctx, "settings")
            .default_size(Vec2::new(660.0, 430.0))
            .show(ctx, |ui| {
                ScrollArea::vertical().id_salt("settings").show(ui, |ui| {
                    ui.label(RichText::new("svn.exe 路径").strong());
                    ui.aligned_row(|ui| {
                        ui.add(
                            TextEdit::singleline(&mut self.cfg.svn_exe)
                                .hint_text("留空则自动寻找")
                                .margin(field_margin(ui))
                                .desired_width((ui.available_width() - 150.0).max(180.0)),
                        );
                        if ui.button("浏览…").clicked() {
                            if let Some(file) = rfd::FileDialog::new()
                                .set_title("选择 svn.exe")
                                .add_filter("可执行文件", &["exe"])
                                .pick_file()
                            {
                                let text = file.to_string_lossy().into_owned();
                                self.apply_svn_exe(&text);
                                self.spawn_all_refresh();
                            }
                        }
                    });
                    ui.label(RichText::new("若提示“无法添加工作副本”，请重新安装 SVN，并勾选“command line client tools”").weak());
                    ui.aligned_row(|ui| {
                        if ui.button("应用此路径").clicked() {
                            let exe = self.cfg.svn_exe.clone();
                            self.apply_svn_exe(&exe);
                            self.hint(match self.version.clone() {
                                Some(v) => format!("svn.exe 可用，版本 {v}"),
                                None => "该路径无法执行，请确认是 svn.exe 命令行客户端".to_owned(),
                            });
                            self.spawn_all_refresh();
                        }
                        if ui.button("自动寻找").clicked() {
                            self.spawn_detect();
                        }
                        if ui.button("前往下载svn").clicked() {
                            ctx.open_url(egui::OpenUrl::same_tab(
                                "https://sourceforge.net/projects/tortoisesvn/",
                            ));
                            self.hint("已在默认浏览器打开 TortoiseSVN 下载页");
                        }
                        ui.label(RichText::new(format!("当前SVN版本：{version}")).weak());
                    });
                    if !candidates.is_empty() {
                        ui.collapsing(format!("检测到的候选（{} 个）", candidates.len()), |ui| {
                            for item in candidates {
                                let mark = if item == self.cfg.svn_exe { "✓" } else { " " };
                                let text = format!("{mark}  {item}");
                                if ui.add(egui::Label::new(RichText::new(text).size(12.0).monospace())).clicked() {
                                    self.apply_svn_exe(&item);
                                    self.spawn_all_refresh();
                                }
                            }
                        });
                    }
                    ui.separator();
                    ui.collapsing("git.exe 路径（.git 目录行用）", |ui| {
                        ui.aligned_row(|ui| {
                            ui.add(
                                TextEdit::singleline(&mut self.cfg.git_exe)
                                    .hint_text("留空则自动寻找")
                                    .margin(field_margin(ui))
                                    .desired_width((ui.available_width() - 150.0).max(180.0)),
                            );
                            if ui.button("浏览…").clicked() {
                                if let Some(file) = rfd::FileDialog::new()
                                    .set_title("选择 git.exe")
                                    .add_filter("可执行文件", &["exe"])
                                    .pick_file()
                                {
                                    let text = file.to_string_lossy().into_owned();
                                    self.apply_git_exe(&text);
                                }
                            }
                        });
                        ui.aligned_row(|ui| {
                            if ui.button("应用此路径").clicked() {
                                let exe = self.cfg.git_exe.clone();
                                self.apply_git_exe(&exe);
                                self.hint(match self.git_version.clone() {
                                    Some(v) => format!("git.exe 可用，版本 {v}"),
                                    None => "该路径无法执行，请确认是 git.exe 命令行客户端".to_owned(),
                                });
                            }
                            if ui.button("自动寻找").clicked() {
                                match crate::git::detect(crate::git::candidates()) {
                                    Some((found, version)) => {
                                        self.apply_git_exe(&found.to_string_lossy());
                                        self.hint(format!(
                                            "已找到 {}（版本 {version}）",
                                            found.display()
                                        ));
                                    }
                                    None => self.hint("未找到 git.exe，请手动填写路径"),
                                }
                            }
                            let note = if self.git.available() {
                                self.git.exe.display().to_string()
                            } else {
                                "未找到".to_owned()
                            };
                            ui.label(RichText::new(format!("当前：{note}")).weak());
                            if let Some(v) = &self.git_version {
                                ui.label(RichText::new(format!("版本 {v}")).weak());
                            }
                        });
                        ui.label(
                            RichText::new(
                                "列表里带 .git 的目录走这里配置的 git 命令行；Git 目录的「上传」= commit 后自动 push。",
                            )
                            .weak()
                            .size(11.0),
                        );
                    });
                    ui.separator();
                    ui.collapsing("Beyond Compare 设置", |ui| {
                        ui.aligned_row(|ui| {
                            ui.add(
                                TextEdit::singleline(&mut self.cfg.bc_exe)
                                    .hint_text("留空则自动寻找")
                                    .margin(field_margin(ui))
                                    .desired_width((ui.available_width() - 150.0).max(180.0)),
                            );
                            if ui.button("浏览…").clicked() {
                                if let Some(file) = rfd::FileDialog::new()
                                    .set_title("选择 BCompare.exe")
                                    .add_filter("可执行文件", &["exe"])
                                    .pick_file()
                                {
                                    self.bc = file.clone();
                                    self.cfg.bc_exe = file.to_string_lossy().into_owned();
                                    self.persist();
                                }
                            }
                        });
                        ui.aligned_row(|ui| {
                            if ui.button("应用此路径").clicked() {
                                self.bc = PathBuf::from(self.cfg.bc_exe.trim_matches('"'));
                                self.persist();
                            }
                            if ui.button("自动寻找").clicked() {
                                match bcompare::detect() {
                                    Some(found) => {
                                        self.bc = found.clone();
                                        self.cfg.bc_exe = found.to_string_lossy().into_owned();
                                        self.hint(format!("已找到 {}", found.display()));
                                    }
                                    None => self.hint("未找到 BCompare.exe，请手动填写路径"),
                                }
                                self.persist();
                            }
                            let note = if self.bc.is_file() {
                                self.bc.display().to_string()
                            } else {
                                "未找到".to_owned()
                            };
                            ui.label(RichText::new(format!("当前：{note}")).weak());
                        });
                        // 调用对比时的服务器端所在侧：文件对比的 BASE 版本、
                        // 文件夹对比里所选的工作副本目录都算服务器端
                        ui.aligned_row(|ui| {
                            ui.label("调用对比时，服务器端放在");
                            for (value, text) in [("left", "左侧"), ("right", "右侧")] {
                                if ui
                                    .selectable_label(self.cfg.bc_server_side == value, text)
                                    .on_hover_text(
                                        "服务器端指：文件对比时的 BASE 版本；文件夹对比时列表里选中的目录（本地端是「绑定对比对象」）。\n\
                                         没绑定对比对象、且记录两侧路径都认不出服务器端（按路径名含 server 判断）时，沿用记录原有的左右顺序。",
                                    )
                                    .clicked()
                                {
                                    self.cfg.bc_server_side = value.to_owned();
                                    self.persist();
                                    self.hint(format!(
                                        "已设置：调用 Beyond Compare 对比时，服务器端放在{text}侧"
                                    ));
                                }
                            }
                        });
                        ui.label(
                            RichText::new(
                                "服务器端 = 文件对比的 BASE 版本、文件夹对比里所选的目录；本地端 = 工作副本文件、绑定的对比对象",
                            )
                            .weak()
                            .size(11.0),
                        );
                        // BC 命令行只有 /filters= 一个开关能带设置，比较内容和规则传不过去，
                        // 只能靠 BC 自己的「所有文件夹比较视图」默认值兜底，这里把话说在前面
                        ui.label(
                            RichText::new(
                                "对比筛选条件按记录里的值用 /filters= 带过去；「比较内容」BC 没有命令行开关，\
                                 只能落到 BC 自己的会话默认值上——用下面那个开关，等价于在 BC 会话设置底部下拉里选「更新会话默认值」。",
                            )
                            .weak()
                            .size(11.0),
                        );
                        // 「比较内容」「比较文件名大小写」BC 没有命令行开关，只能写进它的会话默认值节点
                        match bcompare::default_rules() {
                            Some(saved) => {
                                let mut next = saved;
                                if ui
                                    .checkbox(&mut next.content, "新建文件夹比较默认开启「比较内容」")
                                    .on_hover_text(
                                        "写的是 BCSessions.xml 里「新建文件夹比较」那个默认值节点，\
                                         之后所有新建的文件夹比较都按它来（包括本程序打开的）。\n\
                                         Beyond Compare 退出时会整个覆盖会话存储，所以改之前要先全部关掉它；\
                                         写入前会自动备份成 BCSessions.xml.repomanager.bak。",
                                    )
                                    .clicked()
                                {
                                    match bcompare::set_default_rules(&next) {
                                        Ok(note) => self.push(Level::Info, note),
                                        Err(error) => self.push(Level::Error, format!("设置失败：{error}")),
                                    }
                                }
                                if ui
                                    .checkbox(&mut next.filename_case, "默认「比较文件名大小写」")
                                    .on_hover_text(
                                        "勾上之后 Foo.java 和 foo.java 不再算同一个文件，会各算一条只在单侧存在的记录。\n\
                                         同样写进 BC 的会话默认值，改之前要先全部关掉 Beyond Compare。",
                                    )
                                    .clicked()
                                {
                                    match bcompare::set_default_rules(&next) {
                                        Ok(note) => self.push(Level::Info, note),
                                        Err(error) => self.push(Level::Error, format!("设置失败：{error}")),
                                    }
                                }
                            }
                            None => {
                                ui.label(
                                    RichText::new("（未找到 BC 的会话默认值节点，请先在 Beyond Compare 中执行一次文件夹对比）")
                                        .weak()
                                        .size(11.0),
                                );
                            }
                        }                        ui.horizontal(|ui| {
                            if ui
                                .button("重置试用（删除注册表 CacheID）")
                                .on_hover_text(
                                    "reg delete \"HKEY_CURRENT_USER\\Software\\Scooter Software\\Beyond Compare 4\" /v CacheID /f\nBeyond Compare 4 / 5 两个版本的键都会处理，改完需重启 Beyond Compare。",
                                )
                                .clicked()
                            {
                                self.reset_bc();
                            }
                        });
                    });
                    ui.separator();
                    ui.collapsing("仓库账号（留空则使用 svn 已缓存的凭据）", |ui| {
                        ui.aligned_row(|ui| {
                            ui.label("用户名");
                            ui.add(
                                TextEdit::singleline(&mut self.cfg.auth_user)
                                    .margin(field_margin(ui))
                                    .desired_width(180.0),
                            );
                        });
                        ui.aligned_row(|ui| {
                            ui.label("密码    ");
                            ui.add(
                                TextEdit::singleline(&mut self.cfg.auth_pass)
                                    .password(true)
                                    .margin(field_margin(ui))
                                    .desired_width(180.0),
                            );
                        });
                        if ui.button("保存账号信息").clicked() {
                            self.svn.username = self.cfg.auth_user.clone();
                            self.svn.password = self.cfg.auth_pass.clone();
                            self.persist();
                            self.hint("账号设置已保存");
                            self.spawn_all_refresh();
                        }
                        ui.label(RichText::new("注意：密码以明文保存于配置文件中，仅在仓库没有缓存凭据时填写。").weak().size(11.5));
                    });
                    ui.collapsing("常规", |ui| {
                        ui.aligned_row(|ui| {
                            ui.label("提交记录读取条数");
                            ui.add(egui::DragValue::new(&mut self.cfg.log_limit).range(5..=500).speed(5));
                        });
                        ui.aligned_row(|ui| {
                            ui.label("自动刷新间隔（秒，0 = 关闭）");
                            ui.add(egui::DragValue::new(&mut self.cfg.auto_refresh).range(0..=3600).speed(10));
                        });
                        // 立即保存：这个开关只影响之后新打开的历史页，不用重新读取记录
                        let mut only_mine = self.cfg.history_only_mine;
                        if ui
                            .checkbox(&mut only_mine, "打开提交记录时默认只看本人记录")
                            .on_hover_text(
                                "开：进入「提交记录」页默认按本机 svn 登录人过滤，只列自己的提交。\n\
                                 关：默认列出所有人的提交。\n\
                                 只决定进入页面时的初始状态，页面里的「只看 xxx 的提交」随时可以切。",
                            )
                            .changed()
                        {
                            self.cfg.history_only_mine = only_mine;
                            self.persist();
                        }
                        // 立即保存：只影响之后的提交，不用重新检测
                        let mut update_after = self.cfg.update_after_commit;
                        if ui
                            .checkbox(&mut update_after, "提交成功后自动更新（svn update）")
                            .on_hover_text(
                                "开：提交完成后自动补跑一次 svn update，把整棵工作副本树的版本推到最新，\n\
                                 列表里的「本地 r」会立刻跟上新提交的版本（这一步不往输出区刷日志）。\n\
                                 关：只提交，不动版本号（根目录会一直停在提交前的版本）。\n\
                                 注意：update 会把别人已提交的改动一起拉到本地，可能带来合并甚至冲突。",
                            )
                            .changed()
                        {
                            self.cfg.update_after_commit = update_after;
                            self.persist();
                        }
                        // 开机自启：状态以注册表为准（用户手动删了 Run 值时这里如实显示），
                        // 配置里的字段只做记录；开关本身就持久化，不用再点「保存常规设置」
                        let mut auto_start = autostart::is_enabled();
                        if ui
                            .checkbox(&mut auto_start, "开机自动启动")
                            .on_hover_text(
                                "开：登录 Windows 后自动启动本程序（写入注册表 HKCU\\Software\\Microsoft\\\
                                 Windows\\CurrentVersion\\Run，指向当前 exe，不需要管理员权限）。\n\
                                 关：删除该注册表值。\n\
                                 注意：记录的是当前 exe 的完整路径，如果之后换了目录放新版，需要重新勾一次。",
                            )
                            .changed()
                        {
                            match autostart::set_enabled(auto_start) {
                                Ok(()) => {
                                    self.cfg.auto_start = auto_start;
                                    self.persist();
                                    self.hint(if auto_start {
                                        "已开启开机自启（下次登录 Windows 生效）"
                                    } else {
                                        "已关闭开机自启"
                                    });
                                }
                                Err(error) => {
                                    self.push(Level::Error, format!("设置开机自启失败：{error}"));
                                }
                            }
                        }
                        ui.aligned_row(|ui| {
                            ui.label("主题");
                            for (value, text) in
                                [("system", "跟随系统"), ("light", "浅色"), ("dark", "深色")]
                            {
                                if ui.selectable_label(self.cfg.theme == value, text).clicked() {
                                    self.cfg.theme = value.to_owned();
                                    self.persist();
                                }
                            }
                        });
                        ui.aligned_row(|ui| {
                            ui.label("主题色");
                            // 取色器读写的都是 "#RRGGBB"；留空时 accent_rgb 回落到默认靛蓝，
                            // 所以按钮一上来就停在当前真正生效的那支色上
                            let mut rgb = crate::ui::accent_rgb(&self.cfg.accent);
                            if crate::ui::color_swatch(ui, &mut rgb).changed() {
                                self.cfg.accent = crate::ui::accent_hex(rgb);
                                self.apply_accent(ctx);
                                self.persist();
                            }
                            if ui.button("恢复默认").clicked() {
                                self.cfg.accent.clear();
                                self.apply_accent(ctx);
                                self.persist();
                            }
                        });
                        ui.aligned_row(|ui| {
                            ui.label("字号");
                            let mut changed = false;
                            // 下拉框默认按 interact_size 撑高度，比整行的按钮档矮一截会错位：
                            // 先垫到 control_height 同档再画（与上面的取色按钮同一招）
                            let previous = ui.spacing().interact_size;
                            ui.spacing_mut().interact_size.y = crate::ui::control_height(ui);
                            egui::ComboBox::from_id_salt("ui_scale")
                                .selected_text(self.cfg.ui_scale.clone())
                                .show_ui(ui, |ui| {
                                    for (name, _) in fonts::SCALES {
                                        if ui
                                            .selectable_value(&mut self.cfg.ui_scale, name.to_string(), *name)
                                            .changed()
                                        {
                                            changed = true;
                                        }
                                    }
                                });
                            ui.spacing_mut().interact_size = previous;
                            if changed {
                                self.persist();
                            }
                        });
                        ui.aligned_row(|ui| {
                            ui.label("字体");
                            let mut picked: Option<String> = None;
                            let previous = ui.spacing().interact_size;
                            ui.spacing_mut().interact_size.y = crate::ui::control_height(ui);
                            egui::ComboBox::from_id_salt("ui_font")
                                .selected_text(self.cfg.ui_font.clone())
                                .show_ui(ui, |ui| {
                                    for name in fonts::available() {
                                        if ui
                                            .selectable_value(&mut self.cfg.ui_font, name.clone(), name)
                                            .changed()
                                        {
                                            picked = Some(self.cfg.ui_font.clone());
                                        }
                                    }
                                });
                            ui.spacing_mut().interact_size = previous;
                            if let Some(family) = picked {
                                self.font_note = fonts::note_for(&family, fonts::install(ctx, &family));
                                self.persist();
                            }
                        });
                        if ui.button("保存常规设置").clicked() {
                            self.persist();
                            self.hint("设置已保存");
                        }
                        ui.label(RichText::new(format!("配置文件：{}", config::config_file().display())).size(11.5).weak());
                        ui.label(RichText::new(self.font_note.clone()).size(11.5).weak());
                    });
                    ui.collapsing("简约样式卡片显示内容（作用于主页「简约」样式）", |ui| {
                        ui.label(
                            RichText::new(
                                "切换至「简约」样式后，卡片上显示以下内容项；目录名称始终显示。\
                                 更改即时生效，并自动写入配置文件。",
                            )
                            .weak()
                            .size(11.5),
                        );
                        let mut dirty = false;
                        ui.horizontal(|ui| {
                            let mut value = self.cfg.card_show.lamp;
                            if ui.checkbox(&mut value, "检测状态灯").changed() {
                                self.cfg.card_show.lamp = value;
                                dirty = true;
                            }
                            let mut value = self.cfg.card_show.tag;
                            if ui.checkbox(&mut value, "目录标记").changed() {
                                self.cfg.card_show.tag = value;
                                dirty = true;
                            }
                            let mut value = self.cfg.card_show.ver_local;
                            if ui.checkbox(&mut value, "本地版本").changed() {
                                self.cfg.card_show.ver_local = value;
                                dirty = true;
                            }
                            let mut value = self.cfg.card_show.ver_remote;
                            if ui.checkbox(&mut value, "远端版本").changed() {
                                self.cfg.card_show.ver_remote = value;
                                dirty = true;
                            }
                        });
                        ui.horizontal(|ui| {
                            let mut value = self.cfg.card_show.pending;
                            if ui.checkbox(&mut value, "可更新项数").changed() {
                                self.cfg.card_show.pending = value;
                                dirty = true;
                            }
                            let mut value = self.cfg.card_show.changed;
                            if ui.checkbox(&mut value, "待提交项数").changed() {
                                self.cfg.card_show.changed = value;
                                dirty = true;
                            }
                            let mut value = self.cfg.card_show.diff;
                            if ui.checkbox(&mut value, "本地差异项数").on_hover_text(
                                "仅对已通过「更多 → 绑定对比对象」绑定本地端的目录生效",
                            ).changed() {
                                self.cfg.card_show.diff = value;
                                dirty = true;
                            }
                        });
                        if dirty {
                            self.persist();
                        }
                    });
                    // 插件没装时这块配置一起收起，免得配了也没处用
                    if self.plugin_installed(crate::plugins::PluginId::AiLog) {
                        ui.collapsing("AI 日志（调用 AI 生成工作日志）", |ui| {
                            ui.label(
                                RichText::new(
                                    "在历史页勾选本人提交，把「文件名 + 修改内容」发给 AI 整理成工作日志。\n\
                                     接口需兼容 OpenAI /chat/completions 格式（DeepSeek、通义、Kimi 等均支持）；\
                                     只填 base 地址也行，程序会自动补全 /chat/completions。\
                                     密钥保存在本机配置文件，请求经系统 curl 发送。",
                                )
                                .size(11.5)
                                .weak(),
                            );
                            ui.aligned_row(|ui| {
                                ui.label("服务地址");
                                ui.add(
                                    TextEdit::singleline(&mut self.cfg.ai_url)
                                        .hint_text(
                                            "如 https://api.deepseek.com（通义：https://dashscope.aliyuncs.com/compatible-mode/v1）",
                                        )
                                        .margin(field_margin(ui))
                                        .desired_width((ui.available_width() - 90.0).max(180.0)),
                                );
                            });
                            ui.aligned_row(|ui| {
                                ui.label("API Key");
                                ui.add(
                                    TextEdit::singleline(&mut self.cfg.ai_key)
                                        .password(true)
                                        .hint_text("sk-…")
                                        .margin(field_margin(ui))
                                        .desired_width((ui.available_width() - 90.0).max(180.0)),
                                );
                            });
                            ui.aligned_row(|ui| {
                                ui.label("模型名  ");
                                ui.add(
                                    TextEdit::singleline(&mut self.cfg.ai_model)
                                        .hint_text("如 deepseek-chat / gpt-4o-mini / qwen-plus")
                                        .margin(field_margin(ui))
                                        .desired_width((ui.available_width() - 90.0).max(180.0)),
                                );
                            });
                            ui.label(RichText::new("口吻").size(11.5).weak());
                            ui.add(
                                TextEdit::multiline(&mut self.cfg.ai_tone)
                                    .desired_rows(2)
                                    .desired_width(f32::INFINITY)
                                    .hint_text("例：我是后端组的张三，日志写给部门周报，用第一人称、简洁正式"),
                            );
                            if ui.button("保存 AI 设置").clicked() {
                                self.persist();
                                self.hint("AI 日志设置已保存");
                            }
                            ui.label(
                                RichText::new("注意：密钥以明文保存于本机配置文件；生成时把勾选的提交说明与文件清单发给该服务，注意涉密内容。")
                                    .weak()
                                    .size(11.0),
                            );
                        });
                        ui.separator();
                    }

                    ui.collapsing("版本更新", |ui| {
                        ui.label(RichText::new("更新源").size(11.5).weak());
                        ui.aligned_row(|ui| {
                            // 三个都先求值再判断：`||` 短路会让没求值的那个 radio 某帧消失
                            let pick_official = ui.radio_value(
                                &mut self.cfg.update_source,
                                update::SOURCE_OFFICIAL.to_owned(),
                                "官方源（GitHub 最新发布）",
                            );
                            let pick_test = ui.radio_value(
                                &mut self.cfg.update_source,
                                update::SOURCE_TEST.to_owned(),
                                "测试分支（含预发布）",
                            );
                            let pick_custom = ui.radio_value(
                                &mut self.cfg.update_source,
                                update::SOURCE_CUSTOM.to_owned(),
                                "自定义源（服务端地址）",
                            );
                            // 仓库地址不单独占一行，收在悬停提示里
                            pick_official.clone().on_hover_text(format!(
                                "读取 {} 的最新 Release",
                                update::OFFICIAL_PAGE
                            ));
                            pick_test.clone().on_hover_text(format!(
                                "读取 {} 的 Release 列表，预发布也在候选里：\n\
                                 把某次 Release 勾上「pre-release」就是发测试版，\n\
                                 只有选了这个源的客户端会拿到它。",
                                update::OFFICIAL_PAGE
                            ));
                            if pick_official.changed() || pick_test.changed() || pick_custom.changed()
                            {
                                self.persist();
                                self.hint(if update::is_custom(&self.cfg.update_source) {
                                    "更新源已改为自建服务端，在下面填服务根目录地址".to_owned()
                                } else {
                                    format!(
                                        "更新源已改为{}",
                                        update::source_label(&self.cfg.update_source)
                                    )
                                });
                            }
                        });
                        if update::is_custom(&self.cfg.update_source) {
                            ui.label(
                                RichText::new(
                                    "在下方填写服务端托管地址",
                                )
                                .size(11.5)
                                .weak(),
                            );
                            ui.aligned_row(|ui| {
                                ui.add(
                                    TextEdit::singleline(&mut self.cfg.update_server)
                                        .hint_text("如 http://192.168.1.10:8666")
                                        .margin(field_margin(ui))
                                        .desired_width((ui.available_width() - 90.0).max(180.0)),
                                );
                                if ui.button("保存地址").clicked() {
                                    self.persist();
                                    self.hint("更新服务端地址已保存");
                                }
                            });
                        }
                        // 自动检查更新：开着程序时每 5 分钟查一次，只提醒不自动下载
                        let auto = ui
                            .checkbox(
                                &mut self.cfg.auto_update_check,
                                format!(
                                    "自动检查更新（每 {} 分钟）",
                                    update::AUTO_CHECK_EVERY_SECS / 60
                                ),
                            )
                            .on_hover_text(
                                "开着程序时每隔 5 分钟自动查一次更新：发现新版本只在顶部亮起「↑ 新版本」按钮，\n\
                                 不会自己下载或覆盖。关掉后仍然有启动时检查和手动「检查更新」。\n\
                                 官方源是匿名访问 GitHub 接口（按 IP 限流 60 次/小时），5 分钟一次用不掉额度。",
                            );
                        if auto.changed() {
                            self.persist();
                            // 刚勾上就重新掐表，不用等上一轮留下的旧时刻
                            self.next_update_check = std::time::Instant::now()
                                + std::time::Duration::from_secs(update::AUTO_CHECK_EVERY_SECS);
                            self.hint(if self.cfg.auto_update_check {
                                "已开启自动检查更新：每 5 分钟一次"
                            } else {
                                "已关闭自动检查更新"
                            });
                        }
                        ui.aligned_row(|ui| {
                            let checking = self.pool.has(Kind::CheckUpdate, usize::MAX);
                            if ui
                                .button(if checking { "检查中…" } else { "检查更新" })
                                .clicked()
                                && !checking
                            {
                                self.spawn_update_check(false);
                            }
                            // 判定在后台检查那趟就算好了（按文件 sha256，见 update::has_update）
                            let has_new = self.update_ready;
                            if ui.button("立即更新").clicked() {
                                if has_new {
                                    // 与顶部「↑ 新版本」按钮同一个确认对话框
                                    self.show_update_confirm = true;
                                } else {
                                    self.hint(if self.update_info.is_some() {
                                        "当前已是最新版本"
                                    } else {
                                        "请先「检查更新」"
                                    });
                                }
                            }
                        });
                        ui.label(RichText::new(format!("当前版本：V{APP_VERSION}")).size(11.5).weak());
                        if let Some(m) = &self.update_info {
                            let mut line = format!("最新版本：V{}", m.version.trim());
                            if m.prerelease {
                                line.push_str("（测试版）");
                            }
                            if !m.published_at.trim().is_empty() {
                                line.push_str(&format!("（发布于 {}）", m.published_at.trim()));
                            }
                            ui.label(RichText::new(line).size(11.5).weak());
                            if !m.notes.trim().is_empty() {
                                // 多行更新说明按原文渲染：标题一行，说明整块跟随
                                ui.label(RichText::new("更新说明").strong().size(11.5));
                                ui.label(RichText::new(m.notes.trim()).size(11.5).weak());
                            }
                        } else if update::is_custom(&self.cfg.update_source)
                            && self.cfg.update_server.trim().is_empty()
                        {
                            ui.label(RichText::new("未配置服务端地址，不会检查更新").size(11.5).weak());
                        }
                    });
                });
            });
        self.show_settings = open;
    }

}
