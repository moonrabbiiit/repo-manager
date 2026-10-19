//! 各个独立窗口：修改仓库地址、全部上传（含单目录待提交明细）、发现新版本时的升级确认。


use egui::{Align, Color32, Frame, Key, Layout, RichText, ScrollArea, TextEdit, Vec2};

use crate::jobs::Kind;
use crate::repo::{fmt_rev, RepoKind};
use crate::ui::WindowCentered;
use crate::{pane_hover, update, APP_VERSION, Level, RepoApp, UploadAll, ink};
use crate::ui::{AlignedRow, field_margin};

/// 清单行：勾选中的行淡染主色、没勾的行悬停给一档灰底，长列表里指到哪行看得清哪行。
/// 行内容照旧由调用方摆；悬停读的是上一帧的命中结果——逐帧跟着指针走，滞后一帧看不出来。
fn checked_row(ui: &mut egui::Ui, id: egui::Id, checked: bool, add_contents: impl FnOnce(&mut egui::Ui)) {
    let hovered = ui
        .ctx()
        .read_response(id)
        .is_some_and(|resp| resp.hovered());
    let fill = if checked {
        ui.visuals().selection.bg_fill
    } else if hovered {
        pane_hover(ui)
    } else {
        Color32::TRANSPARENT
    };
    let row = Frame::new()
        .inner_margin(egui::Margin::symmetric(6, 2))
        .corner_radius(5.0)
        .fill(fill)
        .show(ui, |ui| {
            ui.horizontal(add_contents);
        });
    ui.interact(row.response.rect, id, egui::Sense::hover());
}

/// 规则窗口四格共用的定尺寸富文本格。egui 的 `TextEdit` 只认 `min_size.x`，纵向一律按内容撑开，
/// 两格写多写少就一高一矮、排在下面的「确认」还跟着被顶跑。这里把尺寸钉死：外层 `Frame` 画边框，
/// 固定视口的 `ScrollArea` 兜住高宽，里面放一个无边框 `TextEdit`，内容写满就地滚动，
/// 整窗几何不再随字数变。
fn rules_field(
    ui: &mut egui::Ui,
    salt: &'static str,
    buf: &mut String,
    hint: &'static str,
    size: Vec2,
) -> egui::Response {
    let margin = field_margin(ui);
    let inner = egui::vec2(
        size.x - 2.0 * margin.left as f32,
        size.y - 2.0 * margin.top as f32,
    );
    // 边框描哪一档取决于这一格有没有焦点，所以 id 得先算出来才问得到
    let id = ui.make_persistent_id(salt);
    let stroke = if ui.memory(|mem| mem.has_focus(id)) {
        ui.visuals().selection.stroke
    } else {
        ui.visuals().widgets.inactive.bg_stroke
    };
    Frame::new()
        .fill(ui.visuals().text_edit_bg_color())
        .stroke(stroke)
        .corner_radius(ui.visuals().widgets.inactive.corner_radius)
        .inner_margin(margin)
        .show(ui, |ui| {
            ScrollArea::vertical()
                .id_salt(salt)
                .auto_shrink([false, false])
                .max_width(inner.x)
                .max_height(inner.y)
                .show(ui, |ui| {
                    ui.add(
                        TextEdit::multiline(buf)
                            .id(id)
                            .hint_text(hint)
                            .min_size(egui::vec2(inner.x, 0.0))
                            .frame(Frame::new()),
                    )
                })
                .inner
        })
        .inner
}

/// 「解决冲突」窗口里那条策略下拉：四个选项排下来和 `svn resolve --accept` 的取值一一对应，
/// 选中项的短名当按钮文字。每个选项挂一句后果说明——「用我的版本」和「用服务器版本」
/// 各会丢掉一边，不写出来最容易点错。
fn accept_choice(ui: &mut egui::Ui, salt: impl egui::AsIdSalt, selected: &mut crate::svn::Accept) {
    egui::ComboBox::from_id_salt(salt)
        .selected_text(selected.label())
        .show_ui(ui, |ui| {
            for accept in crate::svn::Accept::ALL {
                ui.selectable_value(selected, accept, accept.label())
                    .on_hover_text(accept.tip());
            }
        });
}

/// 地址前缀改写：把 `current` 开头的 `from` 换成 `to`。两边都先掐掉尾部斜杠；
/// from 是 current 的开头且 to 非空才改写，否则给 None（没有可显示的新地址）。
/// 单目录与批量两个改地址窗口共用这一份口径
pub(crate) fn rewrite_url(current: &str, from: &str, to: &str) -> Option<String> {
    let from = from.trim().trim_end_matches('/');
    let to = to.trim().trim_end_matches('/');
    if from.is_empty() || to.is_empty() || !current.starts_with(from) {
        return None;
    }
    Some(current.replacen(from, to, 1))
}

impl RepoApp {
    // ------------------------------------------------------------ 「修改仓库地址」窗口

    pub(crate) fn relocate_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut dialog) = self.relocate.clone() else {
            return;
        };
        let dir = dialog.dir;
        let label = self.dir_label(dir);
        let current = self
            .dirs
            .get(dir)
            .and_then(|view| view.info.clone())
            .map(|info| info.url)
            .unwrap_or_default();
        let running = self.pool.has(Kind::Relocate, dir);
        // 关闭用标题栏的 ×，工具栏里不再放「关闭」按钮
        let mut open = true;
        egui::Window::new(format!("修改仓库地址 · {label}"))
            .open(&mut open)
            .centered(ctx, "relocate")
            .show(ctx, |ui| {
                ui.label(
                    RichText::new("只改写工作副本记录的仓库地址，不会更新、也不会改动任何本地文件。")
                        .weak()
                        .size(11.5),
                );
                ui.label(RichText::new(format!("当前地址：{current}")).size(12.0).monospace());
                ui.add_space(5.0);
                ui.label(RichText::new("原地址前缀").strong().size(12.5));
                ui.add(
                    TextEdit::singleline(&mut dialog.from)
                        .margin(field_margin(ui))
                        .desired_width(470.0),
                );
                ui.label(RichText::new("新地址前缀").strong().size(12.5));
                let field = ui.add(
                    TextEdit::singleline(&mut dialog.to)
                        .hint_text("例如 https://ypcloud/svn/java")
                        .margin(field_margin(ui))
                        .desired_width(470.0),
                );
                if dialog.focus {
                    field.request_focus();
                    dialog.focus = false;
                }
                let from = dialog.from.trim().trim_end_matches('/');
                let to = dialog.to.trim().trim_end_matches('/');
                let new_url = rewrite_url(&current, &dialog.from, &dialog.to).unwrap_or_default();
                ui.add_space(5.0);
                if new_url.is_empty() {
                    let tip = if to.is_empty() {
                        "填好新地址前缀后，这里会显示改写后的地址".to_owned()
                    } else {
                        "原地址前缀必须是当前地址的开头，请核对".to_owned()
                    };
                    ui.label(RichText::new(tip).weak().size(11.5));
                } else {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("改写后：").weak().size(12.0));
                        ui.label(
                            RichText::new(&new_url)
                                .strong()
                                .monospace()
                                .size(12.0)
                                .color(ink(ui, Color32::from_rgb(80, 200, 120))),
                        );
                    });
                }
                if !dialog.error.is_empty() {
                    ui.label(
                        RichText::new(dialog.error.clone())
                            .size(12.0)
                            .color(ink(ui, Color32::from_rgb(240, 100, 100))),
                    );
                }
                ui.aligned_row(|ui| {
                    let ready = !running && !new_url.is_empty() && new_url != current;
                    if ui.add_enabled_ui(ready, |ui| ui.button("✓ 执行 relocate")).inner.clicked() {
                        dialog.error.clear();
                        self.spawn_relocate(dir, from.to_owned(), to.to_owned(), new_url.clone());
                    }
                    if running {
                        ui.spinner();
                        ui.label(RichText::new("正在修改 …").weak().size(11.5));
                    } else if !new_url.is_empty() && new_url == current {
                        ui.label(RichText::new("新地址与原地址相同").weak().size(11.5));
                    }
                });
                if field.has_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                    if new_url.is_empty() || new_url == current {
                        dialog.error = "请填写与当前地址不同的新地址前缀".to_owned();
                    } else if !running {
                        dialog.error.clear();
                        self.spawn_relocate(dir, from.to_owned(), to.to_owned(), new_url.clone());
                    }
                }
                ui.label(
                    RichText::new("提示：svn 将连接新地址校验仓库标识；地址有误仅会导致执行失败，不会损坏工作副本。")
                        .weak()
                        .size(11.5),
                );
            });
        if open {
            self.relocate = Some(dialog);
        } else {
            self.relocate = None;
        }
    }

    // ------------------------------------------------------------ 「批量修改仓库」窗口

    /// 服务器搬家时一次给多个目录改地址：共用一对地址前缀，每个目录各起一个
    /// relocate 任务（同目录的并发约束由 spawn_relocate 把关，有任务在跑的跳过并计数）。
    /// 会改写工作副本记录，所以第一下只把按钮换成确认
    pub(crate) fn batch_relocate_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut dialog) = self.batch_relocate.clone() else {
            return;
        };
        // 窗口开着时目录列表可能增删：勾选状态跟着对齐，新出现的目录默认不勾
        dialog.checked.resize(self.cfg.dirs.len(), false);
        dialog.checked.truncate(self.cfg.dirs.len());
        // from 变了就按新前缀重算一遍默认勾选：地址能对上的才勾
        let from_now = dialog.from.trim().trim_end_matches('/').to_owned();
        if from_now != dialog.last_from {
            for (index, checked) in dialog.checked.iter_mut().enumerate() {
                let url = self
                    .dirs
                    .get(index)
                    .and_then(|view| view.info.as_ref())
                    .map(|info| info.url.clone())
                    .unwrap_or_default();
                *checked = !from_now.is_empty() && url.starts_with(&from_now);
            }
            dialog.last_from = from_now;
        }
        // 每行一份快照：目录名、当前地址、改写结果（None = 改不动），Git 目录不参与
        let rows: Vec<(String, String, Option<String>, bool)> = (0..self.cfg.dirs.len())
            .map(|index| {
                let url = self
                    .dirs
                    .get(index)
                    .and_then(|view| view.info.clone())
                    .map(|info| info.url)
                    .unwrap_or_default();
                let git = self.dir_kind(index) == crate::repo::RepoKind::Git;
                let new_url = rewrite_url(&url, &dialog.from, &dialog.to);
                (
                    self.dir_label(index),
                    url,
                    new_url,
                    git,
                )
            })
            .collect();
        let picked = rows
            .iter()
            .zip(dialog.checked.iter())
            .filter(|((_, _, new_url, git), checked)| !*git && new_url.is_some() && **checked)
            .count();
        let mut open = true;
        let mut close = false;
        egui::Window::new("批量修改仓库")
            .open(&mut open)
            .default_width(760.0)
            .centered(ctx, "batch_relocate")
            .show(ctx, |ui| {
                ui.label(
                    RichText::new(
                        "给勾选的目录逐个执行「修改仓库地址」：只改写工作副本记录的仓库地址，不会更新、也不会改动任何本地文件。",
                    )
                    .weak()
                    .size(11.5),
                );
                ui.add_space(5.0);
                ui.label(RichText::new("原地址前缀").strong().size(12.5));
                let from_field = ui.add(
                    TextEdit::singleline(&mut dialog.from)
                        .hint_text("要替换掉的地址开头，例如 http://192.168.1.251:8080/svn")
                        .margin(field_margin(ui))
                        .desired_width(560.0),
                );
                if dialog.focus {
                    from_field.request_focus();
                    dialog.focus = false;
                }
                ui.label(RichText::new("新地址前缀").strong().size(12.5));
                ui.add(
                    TextEdit::singleline(&mut dialog.to)
                        .hint_text("新前缀，例如 https://svn.example.com/repo")
                        .margin(field_margin(ui))
                        .desired_width(560.0),
                );
                ui.add_space(5.0);
                ui.aligned_row(|ui| {
                    ui.label(RichText::new("参与改地址的目录").strong().size(12.5));
                    if ui.button("全选").clicked() {
                        for (index, checked) in dialog.checked.iter_mut().enumerate() {
                            *checked = rows
                                .get(index)
                                .is_some_and(|(_, _, new_url, git)| new_url.is_some() && !*git);
                        }
                    }
                    if ui.button("全不选").clicked() {
                        dialog.checked.iter_mut().for_each(|checked| *checked = false);
                    }
                    if picked == 0 {
                        ui.label(RichText::new("没有可改写的目录").weak().size(11.5));
                    }
                });
                ScrollArea::vertical()
                    .id_salt("batch_relocate_dirs")
                    .max_height(220.0)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for (index, (label, url, new_url, git)) in rows.iter().enumerate() {
                            let picked_row = dialog.checked.get(index).copied().unwrap_or(false);
                            checked_row(
                                ui,
                                egui::Id::new(("relocate_row", index)),
                                picked_row,
                                |ui| {
                                    let can = !git && new_url.is_some();
                                    let mut checked =
                                        dialog.checked.get(index).copied().unwrap_or(false);
                                    if ui
                                        .add_enabled(can, egui::Checkbox::new(&mut checked, ""))
                                        .on_hover_text("勾选参与本次批量修改仓库")
                                        .changed()
                                    {
                                        dialog.checked[index] = checked;
                                    }
                                    ui.label(RichText::new(label).size(12.5).strong());
                                    match (*git, new_url) {
                                        (true, _) => {
                                            ui.label(
                                                RichText::new("Git 目录不支持")
                                                    .size(11.5)
                                                    .color(ink(ui, Color32::from_gray(130))),
                                            );
                                        }
                                        (false, Some(new)) => {
                                            ui.label(
                                                RichText::new(url).size(11.5).monospace().weak(),
                                            );
                                            ui.label(RichText::new("→").size(11.5).weak());
                                            ui.label(
                                                RichText::new(new)
                                                    .size(11.5)
                                                    .monospace()
                                                    .color(ink(ui, Color32::from_rgb(
                                                        80, 200, 120,
                                                    ))),
                                            );
                                        }
                                        (false, None) if url.is_empty() => {
                                            ui.label(
                                                RichText::new("未检测，没有地址")
                                                    .size(11.5)
                                                    .color(ink(ui, Color32::from_gray(130))),
                                            );
                                        }
                                        (false, None) => {
                                            ui.label(
                                                RichText::new(url).size(11.5).monospace().weak(),
                                            );
                                            ui.label(
                                                RichText::new("前缀不匹配")
                                                    .size(11.5)
                                                    .color(ink(ui, Color32::from_rgb(
                                                        240, 100, 100,
                                                    ))),
                                            );
                                        }
                                    }
                                    if self.pool.is_busy(index) {
                                        ui.spinner();
                                        ui.label(
                                            RichText::new("有任务在跑，本次跳过")
                                                .weak()
                                                .size(11.5),
                                        );
                                    }
                                },
                            );
                        }
                    });
                ui.separator();
                ui.aligned_row(|ui| {
                    let ready = picked > 0;
                    if dialog.confirm {
                        let button = egui::Button::new(
                            RichText::new(format!("确认改地址？{picked} 个目录"))
                                .strong()
                                .color(crate::DANGER_TEXT),
                        )
                        .fill(crate::DANGER_FILL);
                        if ui.add_enabled(ready, button).clicked() {
                            let from = dialog.from.trim().trim_end_matches('/').to_owned();
                            let to = dialog.to.trim().trim_end_matches('/').to_owned();
                            let mut done = 0;
                            let mut skipped = 0;
                            for (index, (_, _, new_url, git)) in rows.iter().enumerate() {
                                let Some(new_url) = new_url else {
                                    continue;
                                };
                                if *git || !dialog.checked.get(index).copied().unwrap_or(false) {
                                    continue;
                                }
                                if self.pool.is_busy(index) {
                                    skipped += 1;
                                    continue;
                                }
                                self.spawn_relocate(index, from.clone(), to.clone(), new_url.clone());
                                done += 1;
                            }
                            let mut text = format!("批量修改仓库：已为 {done} 个目录发起改地址");
                            if skipped > 0 {
                                text.push_str(&format!("，{skipped} 个因有任务在跑被跳过"));
                            }
                            self.push(Level::Info, text);
                            close = true;
                        }
                        if ui.button("先不改").clicked() {
                            dialog.confirm = false;
                        }
                    } else if ui
                        .add_enabled(
                            ready,
                            egui::Button::new(RichText::new(format!("改地址（{picked} 个目录）")).strong()),
                        )
                        .clicked()
                    {
                        dialog.confirm = true;
                        self.hint("每个目录将单独执行一次改地址：核对上方预览后，再次点击「确认改地址」");
                    }
                    if picked == 0 && !rows.is_empty() {
                        ui.label(
                            RichText::new("填写一对前缀，勾选地址以「原地址前缀」开头的目录")
                                .weak()
                                .size(11.5),
                        );
                    }
                });
            });
        if open && !close {
            self.batch_relocate = Some(dialog);
        } else {
            self.batch_relocate = None;
        }
    }

    // ------------------------------------------------------------ 「批量分组 / 标记」窗口

    /// 给勾选的目录统一写分组：空串即清除。只动本地配置，persist 一次，返回真正改动的个数
    pub(crate) fn apply_batch_group(&mut self, checked: &[bool], group: &str) -> usize {
        let group = group.trim();
        let mut done = 0;
        for (index, dir) in self.cfg.dirs.iter_mut().enumerate() {
            if !checked.get(index).copied().unwrap_or(false) || dir.group == group {
                continue;
            }
            dir.group = group.to_owned();
            done += 1;
        }
        if done > 0 {
            self.persist();
        }
        done
    }

    /// 给勾选的目录统一写标记：tag 非空才存颜色（与单目录编辑器同一口径），tag 为空连颜色一起清。
    /// 只动本地配置，persist 一次，返回真正改动的个数
    pub(crate) fn apply_batch_tag(&mut self, checked: &[bool], tag: &str, hex: &str) -> usize {
        let tag = tag.trim();
        let hex = crate::home::tag_color_hex(crate::home::tag_color_rgb(hex));
        let mut done = 0;
        for (index, dir) in self.cfg.dirs.iter_mut().enumerate() {
            if !checked.get(index).copied().unwrap_or(false) {
                continue;
            }
            if tag.is_empty() {
                if dir.tag.is_empty() && dir.tag_color.is_empty() {
                    continue;
                }
                dir.tag.clear();
                dir.tag_color.clear();
            } else {
                if dir.tag == tag && dir.tag_color == hex {
                    continue;
                }
                dir.tag = tag.to_owned();
                dir.tag_color = hex.clone();
            }
            done += 1;
        }
        if done > 0 {
            self.persist();
        }
        done
    }

    /// 批量分组 / 标记：勾选若干目录，统一设置（或清除）分组、标记。改的是本地配置、
    /// 随时能逐个改回来，所以不做两步确认；分组与标记两个按钮互不牵连，各写各的字段。
    pub(crate) fn batch_label_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut dialog) = self.batch_label.clone() else {
            return;
        };
        // 窗口开着时目录列表可能增删：勾选状态跟着对齐，新出现的目录默认不勾
        dialog.checked.resize(self.cfg.dirs.len(), false);
        dialog.checked.truncate(self.cfg.dirs.len());
        // 每行一份快照：目录名、当前分组、当前标记——勾了才知道会覆盖谁
        let rows: Vec<(String, String, String)> = (0..self.cfg.dirs.len())
            .map(|index| {
                let dir = &self.cfg.dirs[index];
                (self.dir_label(index), dir.group.clone(), dir.tag.clone())
            })
            .collect();
        let picked = dialog.checked.iter().filter(|checked| **checked).count();
        let mut open = true;
        let mut close = false;
        egui::Window::new("批量分组 / 标记")
            .open(&mut open)
            .default_width(560.0)
            .centered(ctx, "batch_tag")
            .show(ctx, |ui| {
                ui.label(
                    RichText::new(
                        "给勾选的目录统一设置分组、标记：只改本地配置，随时能逐个改回来。两个按钮互不影响，各写各的字段。",
                    )
                    .weak()
                    .size(11.5),
                );
                ui.add_space(5.0);
                ui.label(RichText::new("分组").strong().size(12.5));
                let group_field = ui.add(
                    TextEdit::singleline(&mut dialog.group)
                        .hint_text("留空 = 清除所选目录的分组")
                        .margin(field_margin(ui))
                        .desired_width(520.0),
                );
                if dialog.focus {
                    group_field.request_focus();
                    dialog.focus = false;
                }
                // 在用分组一键取用：点一下填进输入框，勾选范围仍由下面的列表决定
                let groups = self.existing_groups("");
                if !groups.is_empty() {
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing.x = 4.0;
                        ui.label(RichText::new("在用分组：").weak().size(11.0));
                        for name in groups {
                            if ui
                                .add(egui::Button::new(RichText::new(name.clone())).small())
                                .clicked()
                            {
                                dialog.group = name;
                            }
                        }
                    });
                }
                ui.add_space(3.0);
                ui.label(RichText::new("标记").strong().size(12.5));
                ui.aligned_row(|ui| {
                    ui.add(
                        TextEdit::singleline(&mut dialog.tag)
                            .hint_text("留空 = 清除所选目录的标记")
                            .margin(field_margin(ui))
                            .desired_width(490.0),
                    );
                    let mut rgb = crate::home::tag_color_rgb(&dialog.tag_hex);
                    if crate::ui::color_swatch(ui, &mut rgb)
                        .on_hover_text("标记颜色")
                        .changed()
                    {
                        dialog.tag_hex = crate::home::tag_color_hex(rgb);
                    }
                });
                // 在用标记一键取用：点一下名字与颜色一起填进输入框
                let tags = self.existing_tags("");
                if !tags.is_empty() {
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing.x = 4.0;
                        ui.label(RichText::new("在用标记：").weak().size(11.0));
                        for (name, color) in tags {
                            let [r, g, b] = crate::home::tag_color_rgb(&color);
                            let dot = RichText::new("● ").color(Color32::from_rgb(r, g, b));
                            let label = RichText::new(name.clone());
                            if ui
                                .add(egui::Button::new((dot, label)).small())
                                .clicked()
                            {
                                dialog.tag = name;
                                if !color.is_empty() {
                                    dialog.tag_hex = color;
                                }
                            }
                        }
                    });
                }
                ui.add_space(5.0);
                ui.aligned_row(|ui| {
                    ui.label(RichText::new("要修改的目录").strong().size(12.5));
                    if ui.button("全选").clicked() {
                        dialog.checked.iter_mut().for_each(|checked| *checked = true);
                    }
                    if ui.button("全不选").clicked() {
                        dialog.checked.iter_mut().for_each(|checked| *checked = false);
                    }
                    if picked == 0 {
                        ui.label(RichText::new("请先勾选要修改的目录").weak().size(11.5));
                    }
                });
                ScrollArea::vertical()
                    .id_salt("batch_label_dirs")
                    .max_height(220.0)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for (index, (label, group, tag)) in rows.iter().enumerate() {
                            let picked_row = dialog.checked.get(index).copied().unwrap_or(false);
                            checked_row(
                                ui,
                                egui::Id::new(("label_row", index)),
                                picked_row,
                                |ui| {
                                    let mut checked =
                                        dialog.checked.get(index).copied().unwrap_or(false);
                                    if ui
                                        .checkbox(&mut checked, "")
                                        .on_hover_text("勾选参与本次批量修改")
                                        .changed()
                                    {
                                        dialog.checked[index] = checked;
                                    }
                                    ui.label(RichText::new(label).size(12.5).strong());
                                    if !group.trim().is_empty() {
                                        ui.label(
                                            RichText::new(format!("分组：{group}"))
                                                .size(11.5)
                                                .weak(),
                                        );
                                    }
                                    if !tag.is_empty() {
                                        ui.label(
                                            RichText::new(format!("标记：{tag}")).size(11.5).weak(),
                                        );
                                    }
                                    if group.trim().is_empty() && tag.is_empty() {
                                        ui.label(
                                            RichText::new("未分组、无标记")
                                                .size(11.5)
                                                .color(ink(ui, Color32::from_gray(130))),
                                        );
                                    }
                                },
                            );
                        }
                    });
                ui.separator();
                ui.aligned_row(|ui| {
                    let ready = picked > 0;
                    if ui
                        .add_enabled(
                            ready,
                            egui::Button::new(RichText::new(format!("设置分组（{picked}）")).strong()),
                        )
                        .clicked()
                    {
                        let n = self.apply_batch_group(&dialog.checked, &dialog.group);
                        self.push(Level::Info, format!("批量分组：已修改 {n} 个目录的分组"));
                        close = true;
                    }
                    if ui
                        .add_enabled(
                            ready,
                            egui::Button::new(RichText::new(format!("设置标记（{picked}）")).strong()),
                        )
                        .clicked()
                    {
                        let n = self.apply_batch_tag(&dialog.checked, &dialog.tag, &dialog.tag_hex);
                        self.push(Level::Info, format!("批量标记：已修改 {n} 个目录的标记"));
                        close = true;
                    }
                });
            });
        if open && !close {
            self.batch_label = Some(dialog);
        } else {
            self.batch_label = None;
        }
    }

    // ------------------------------------------------------------ 「回退到某个版本」窗口

    /// 回退确认窗口：这一步会改本地工作副本里的文件，所以第一下只把按钮换成「确认回退」，
    /// 再点一次才真的执行。窗口里不写操作手册，只问一句从哪版回退到哪版、目标是哪个路径。
    pub(crate) fn rollback_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut dialog) = self.rollback.clone() else {
            return;
        };
        let dir = dialog.dir;
        let kind = self.dir_kind(dir);
        let revision = dialog.revision.clone();
        let name = dialog.name.clone();
        let target_path = dialog.target.clone();
        let is_dir = dialog.is_dir;
        let shown = target_path.display().to_string();
        let running = self.pool.has(Kind::Rollback, dir);
        let label = self.dir_label(dir);
        let wc_rev = self
            .dirs
            .get(dir)
            .and_then(|view| view.info.clone())
            .map(|info| info.revision)
            .unwrap_or_default();
        // 回退是在现有本地改动之上做合并，先报个已有的待提交数，别让人以为回退会把它们冲掉
        let pending = self.dirs.get(dir).and_then(|view| view.changed).unwrap_or(0);
        let scope = if is_dir { "整个目录" } else { "这个文件" };
        let mut open = true;
        egui::Window::new(format!("回退到 {} · {label}", fmt_rev(kind, &revision)))
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(460.0)
            .centered(ctx, "rollback")
            .show(ctx, |ui| {
                ui.label(
                    RichText::new(format!(
                        "确定要把{scope}从 {} 回退到 {}？",
                        fmt_rev(kind, &wc_rev),
                        fmt_rev(kind, &revision)
                    ))
                        .strong()
                        .size(13.0),
                );
                ui.label(RichText::new(shown.as_str()).size(12.0).monospace());
                if pending > 0 {
                    ui.label(
                        RichText::new(format!("该目录已有 {pending} 项待提交改动，回退会和它们合并"))
                            .size(11.5)
                            .color(ink(ui, Color32::from_rgb(240, 190, 70))),
                    );
                }
                if dialog.conflicts > 0 {
                    // 实测口径：svn 不肯把回退落在正冲突的节点上，拒绝时一个文件都不改；
                    // 整目录回退只要沾上一个冲突文件就整趟作废，单文件只看它自己
                    let here = self
                        .dirs
                        .get(dir)
                        .and_then(|view| view.blocked.iter().find(|e| {
                            e.path.to_ascii_lowercase() == shown.to_ascii_lowercase()
                        }))
                        .is_some();
                    let note = match (kind, is_dir) {
                        (RepoKind::Git, _) => format!(
                            "该目录还有 {} 项冲突没解决，回退撞上同一处改动会中途停下",
                            dialog.conflicts
                        ),
                        (_, true) => format!(
                            "该目录还有 {} 项冲突没解决，svn 会拒绝整趟回退、一个文件都不改\n想先回退别的文件，就只选那个文件",
                            dialog.conflicts
                        ),
                        (_, false) if here =>
                            "这个文件自己正有冲突，svn 会拒绝这趟回退、不会改动它\n先在目录行「更多 → 解决冲突」把它处理掉".to_string(),
                        (_, false) => format!(
                            "该目录还有 {} 项冲突没解决，不在这个文件上，回退它不受影响",
                            dialog.conflicts
                        ),
                    };
                    ui.label(RichText::new(note).size(11.5).color(ink(ui, Color32::from_rgb(240, 190, 70))));
                }
                if !dialog.error.is_empty() {
                    ui.label(
                        RichText::new(dialog.error.clone())
                            .size(12.0)
                            .color(ink(ui, Color32::from_rgb(240, 100, 100))),
                    );
                }
                ui.add_space(4.0);
                ui.aligned_row(|ui| {
                    if dialog.confirm {
                        let button = egui::Button::new(
                            RichText::new(format!("确认回退到 {}", fmt_rev(kind, &revision)))
                                .strong()
                                .color(crate::DANGER_TEXT),
                        )
                        .fill(crate::DANGER_FILL);
                        if ui.add_enabled(!running, button).clicked() {
                            self.spawn_rollback(
                                dir,
                                revision.clone(),
                                target_path.clone(),
                                name.clone(),
                                is_dir,
                            );
                        }
                        if ui.button("先不回退").clicked() {
                            dialog.confirm = false;
                        }
                    } else if ui
                        .add_enabled(
                            !running,
                            egui::Button::new(
                                RichText::new(format!("回退到 {}", fmt_rev(kind, &revision)))
                                    .strong()
                                    .color(crate::DANGER_TEXT),
                            )
                            .fill(crate::DANGER_FILL),
                        )
                        .clicked()
                    {
                        dialog.confirm = true;
                        self.hint(format!(
                            "回退将改动{scope}的本地文件：核对上方路径后，再次点击「确认回退」"
                        ));
                    }
                    if running {
                        ui.spinner();
                        ui.label(RichText::new("正在回退 …").weak().size(11.5));
                    }
                });
            });
        if open {
            self.rollback = Some(dialog);
        } else {
            self.rollback = None;
        }
    }

    /// 「解决冲突」窗口：该目录里必须先人工处理的条目逐行列出来，每行自己挑一个收场策略，
    /// 勾上的那批一次执行。策略都只动本地工作副本，不碰服务器；「处理」要先确认一次。
    pub(crate) fn resolve_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut panel) = self.resolve.clone() else { return };
        let dir = panel.dir;
        let label = self.dir_label(dir);
        let running = self.pool.has(Kind::Resolve, dir);
        let mut open = true;
        egui::Window::new(format!("解决冲突 · {label}"))
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(520.0)
            .centered(ctx, "resolve")
            .show(ctx, |ui| {
                if panel.rows.is_empty() {
                    ui.label(
                        RichText::new(if panel.incomplete > 0 {
                            "没有文本冲突要处理，但有工作副本没读完的条目（不完整）"
                        } else {
                            "这个目录现在没有必须人工处理的条目"
                        })
                        .size(12.5),
                    );
                    if panel.incomplete > 0 {
                        ui.label(
                            RichText::new(format!(
                                "{} 项不完整（~）：resolve 动不了它们，先「更新」这个目录。",
                                panel.incomplete
                            ))
                            .size(11.5)
                            .color(ink(ui, Color32::from_rgb(240, 190, 70))),
                        );
                    }
                } else {
                    ui.label(
                        RichText::new(
                            "每条选一个收场策略，勾上的那些一次处理完。\n\
                             只改本地工作副本，不碰服务器；之后照样能提交或再撤销。",
                        )
                        .size(11.5)
                        .weak(),
                    );
                    ui.add_space(4.0);
                    // 一行套全部：大多数情况整目录的冲突都是同一种收场，逐行点太累
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("全部取用").size(12.0));
                        accept_choice(ui, "resolve_bulk", &mut panel.bulk);
                        if ui.button("套到所有行").clicked() {
                            for row in panel.rows.iter_mut() {
                                row.accept = panel.bulk;
                            }
                        }
                    });
                    ui.add_space(2.0);
                    ScrollArea::vertical()
                        .id_salt("resolve_rows")
                        .max_height(220.0)
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            for (index, row) in panel.rows.iter_mut().enumerate() {
                                let checked = row.checked;
                                let name = row.name.clone();
                                let target = row.target.clone();
                                checked_row(
                                    ui,
                                    egui::Id::new(("resolve_row", index)),
                                    checked,
                                    |ui| {
                                        ui.checkbox(&mut row.checked, "");
                                        ui.label(RichText::new(name).monospace().size(12.0))
                                            .on_hover_text(target.display().to_string());
                                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                            accept_choice(ui, ("resolve_row", index), &mut row.accept)
                                        });
                                    },
                                );
                            }
                        });
                }
                if !panel.error.is_empty() {
                    ui.label(
                        RichText::new(panel.error.clone())
                            .size(12.0)
                            .color(ink(ui, Color32::from_rgb(240, 100, 100))),
                    );
                }
                let picked: Vec<crate::ResolveRow> = panel
                    .rows
                    .iter()
                    .filter(|row| row.checked)
                    .cloned()
                    .collect();
                if picked.is_empty() && panel.rows.is_empty() {
                    return;
                }
                ui.add_space(6.0);
                ui.aligned_row(|ui| {
                    if panel.confirm {
                        let button = egui::Button::new(
                            RichText::new(format!("确认处理 {} 项", picked.len()))
                                .strong()
                                .color(crate::DANGER_TEXT),
                        )
                        .fill(crate::DANGER_FILL);
                        if ui.add_enabled(!running, button).clicked() {
                            panel.confirm = false;
                            self.spawn_resolve(dir, picked);
                        }
                        if ui.button("先不处理").clicked() {
                            panel.confirm = false;
                        }
                    } else if ui
                        .add_enabled(
                            !running,
                            egui::Button::new(
                                RichText::new(format!("处理勾选的 {} 项", picked.len())).strong(),
                            ),
                        )
                        .clicked()
                    {
                        panel.confirm = true;
                        self.hint("这会改动本地文件，核对每行的策略后再点「确认处理」");
                    }
                    if running {
                        ui.spinner();
                        ui.label(RichText::new("正在处理 …").weak().size(11.5));
                    }
                });
            });
        if open {
            self.resolve = Some(panel);
        } else {
            self.resolve = None;
        }
    }

    // ------------------------------------------------------------ 「双向合并」预览窗口

    /// 本地开发侧 ↔ 服务器端的双向合并预览。清单里只有「写入」那几行动文件，落盘前两侧都会先整份备份。
    /// `Plan` 里带着每份文件算好的字节，逐帧 clone 等于把它们复制一遍，所以这扇窗 take 出来用完再放回去。
    pub(crate) fn merge_dialog(&mut self, ctx: &egui::Context) {
        use crate::merge::Action;
        // 插件被卸载后这扇窗不再出现（卸载时会清掉，这里再兜一层）
        if !self.plugin_installed(crate::plugins::PluginId::Merge) {
            self.merge = None;
            return;
        }
        let Some(mut dialog) = self.merge.take() else { return };
        let dir = dialog.dir;
        // 窗口开着时目录可能已被移除：这扇窗没有归属，不收回来
        if self.cfg.dirs.get(dir).is_none() {
            return;
        }
        let label = self.dir_label(dir);
        let scanning = self.pool.has(Kind::Merge, dir);
        let running = self.pool.has(Kind::ApplyMerge, dir);
        let cleaning = self.pool.has(Kind::CleanBackup, dir);
        let backup_root = crate::config::config_dir();
        // 上一次合并停在半路：备份还留着，两侧却已不一致，先还原再开始新的写入
        let interrupted = dialog
            .backups
            .iter()
            .find(|item| item.unfinished)
            .map(|item| (item.name.clone(), item.dir.clone()));
        let plan = dialog.plan.take();
        let id = egui::Id::new("merge");
        // Esc：最大化时先还原，再按一次才关窗，和两个明细窗口同一个规矩
        let close = dialog.zoom.escape(ctx, id);
        let mut open = true;
        let window = egui::Window::new(format!("双向合并 · {label}"))
            .id(id)
            .open(&mut open)
            .collapsible(false)
            .resizable(true);
        // 默认矩形摆正即可，最大化与还原那一帧的矩形由 Zoom 记账；清单吃满窗口剩下的地方，
        // 所以窗口多大都不会在底下留出一片空白
        let normal_rect = crate::ui::centered_rect(ctx, Vec2::new(620.0, 520.0));
        let shown = dialog
            .zoom
            .place(ctx, window, normal_rect)
            .show(ctx, |ui| {
                if !dialog.error.is_empty() {
                    ui.label(
                        RichText::new(dialog.error.clone())
                            .size(12.0)
                            .color(ink(ui, crate::ERR)),
                    );
                }
                let Some(preview) = plan.as_ref() else {
                    if scanning {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(
                                RichText::new("正在读取两侧内容并推导基线 …")
                                    .weak()
                                    .size(11.5),
                            );
                        });
                    } else {
                        ui.label(
                            RichText::new("尚无清单，可从目录行「更多 → 双向合并」重新预览。")
                                .size(12.0),
                        );
                    }
                    return;
                };
                let rows: Vec<&crate::merge::Entry> = preview
                    .entries
                    .iter()
                    .filter(|entry| entry.action != Action::SkipUnchanged)
                    .collect();
                // 两条写入路径共用同一套前置条件，卡住的原因原样写进悬停里
                let blocked = if running {
                    Some("上一次写入还在跑")
                } else if interrupted.is_some() {
                    Some("上一次合并尚未还原，先处理完那一次")
                } else if preview.writing() == 0 {
                    Some("清单里没有要写入的条目")
                } else {
                    None
                };
                let can_write = blocked.is_none();

                // 左上：功能按钮一行排开（应用并备份 / 直接合并 / 导出报告 / 历次备份 / 最大化）
                ui.horizontal_wrapped(|ui| {
                    if ui
                        .add_enabled(
                            can_write,
                            egui::Button::new(RichText::new("应用并备份").strong()),
                        )
                        .on_hover_text(blocked.unwrap_or(
                            "先整份备份两侧，再按清单写入；这一次之后能一键还原",
                        ))
                        .clicked()
                    {
                        dialog.confirm = true;
                        self.hint(
                            "写入会同时改动本地开发侧与工作副本，两侧原内容先整份备份；核对清单后再次点击「确认写入」",
                        );
                    }
                    // 这条路把确认写在字面上：一次点击就落盘，不留还原点
                    if ui
                        .add_enabled(can_write, egui::Button::new("我已确认无误，直接合并"))
                        .on_hover_text(blocked.unwrap_or("不备份、不留还原点，按清单一次写完"))
                        .clicked()
                    {
                        self.spawn_merge_apply(dir, Box::new(preview.clone()), false);
                    }
                    if ui
                        .button("导出合并报告")
                        .on_hover_text("把本清单存成文字文件，含每项处置与未核对条数")
                        .clicked()
                    {
                        let now = chrono::Local::now();
                        let text = format!(
                            "生成时间：{}\n目录：{label}\n\n{}",
                            now.format("%Y-%m-%d %H:%M:%S"),
                            crate::merge::report(preview)
                        );
                        if let Some(path) = rfd::FileDialog::new()
                            .set_title("保存双向合并预览清单")
                            .set_file_name(format!(
                                "合并预览_{}.txt",
                                now.format("%Y%m%d_%H%M%S")
                            ))
                            .save_file()
                        {
                            match std::fs::write(&path, text) {
                                Ok(()) => self.push(
                                    Level::Info,
                                    format!("合并预览清单已写入：{}", path.display()),
                                ),
                                Err(e) => {
                                    self.push(Level::Error, format!("清单写入失败：{e}"))
                                }
                            }
                        }
                    }
                    let toggle = if dialog.show_backups { "收起备份记录" } else { "历次备份" };
                    if ui.button(toggle).clicked() {
                        dialog.show_backups = !dialog.show_backups;
                    }
                    if ui
                        .button(if dialog.zoom.maximized { "还原大小" } else { "最大化" })
                        .on_hover_text(if dialog.zoom.maximized {
                            "把窗口缩回最大化之前的大小（按 Esc 同样先还原）"
                        } else {
                            "把窗口铺满整个工作区，清单能多看几十行"
                        })
                        .clicked()
                    {
                        dialog.zoom.toggle();
                    }
                    if running {
                        ui.spinner();
                        ui.label(RichText::new("正在写入 …").weak().size(11.5));
                    }
                });
                // 确认写入那一栏：第一下只把按钮换成确认，再点一次才真的动两边
                if dialog.confirm {
                    ui.horizontal(|ui| {
                        let button = egui::Button::new(
                            RichText::new(format!("确认写入 {} 项", preview.writing()))
                                .strong()
                                .color(crate::DANGER_TEXT),
                        )
                        .fill(crate::DANGER_FILL);
                        if ui.add_enabled(can_write, button).clicked() {
                            dialog.confirm = false;
                            self.spawn_merge_apply(dir, Box::new(preview.clone()), true);
                        }
                        if ui.button("先不写入").clicked() {
                            dialog.confirm = false;
                        }
                    });
                }
                // 上一次合并中断未完成：先还原，再开始新的写入
                if let Some((name, path)) = interrupted.as_ref() {
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(format!(
                                "上一次合并（{name}）中断未完成，两侧内容不一致：先还原，再开始新的写入。"
                            ))
                            .size(11.5)
                            .color(ink(ui, crate::AMBER)),
                        );
                        if ui
                            .add_enabled(
                                !running,
                                egui::Button::new(RichText::new("还原上一次").strong()),
                            )
                            .clicked()
                        {
                            self.spawn_merge_restore(dir, path.clone());
                        }
                    });
                }
                ui.add_space(6.0);
                ui.label(
                    RichText::new(
                        "单方改动的区域合入对面，两侧收敛为同一份内容；同一处两边都改过的文件保持原样，仅列入本清单。",
                    )
                    .weak()
                    .size(11.5),
                );
                ui.add_space(6.0);
                // 主体两栏：左侧卡片装文件清单，右侧面板是处理结果的汇总表
                // 非最大化时按固定设计宽（620）切分，不读 available_width —— 否则窗口首帧
                // 用过大的可用宽算出过大的左右栏、又把窗口顶大，下一帧更大，循环涨到满屏。
                // 最大化时窗口已被 Zoom 拉到工作区尺寸，available_width 才是真满宽，照用即可。
                let body_h = ui.available_height().max(120.0);
                let total_w = if dialog.zoom.maximized {
                    ui.available_width()
                } else {
                    normal_rect.width()
                };
                // 两栏都按固定百分比切：左文件清单占大头、右处理结果只占一小部分。
                // 这样无论内容多少，右栏永远有自己那一份宽度、不会被挤没（之前就吃过这个亏）。
                let left_w = total_w * 0.74;
                let right_w = total_w * 0.26 - ui.spacing().item_spacing.x;
                ui.horizontal(|ui| {
                    // 左：文件清单卡片
                    // 这枚子 ui 必须显式改成 top_down：它挂在 ui.horizontal 之下，不指定就会
                    // 继承横向布局，把卡片里的标题 / 清单行全挤成一行横排，卡片最小宽被撑到满屏。
                    ui.allocate_ui_with_layout(
                        egui::vec2(left_w, body_h),
                        Layout::top_down(Align::Min),
                        |ui| {
                        egui::Frame::new()
                            .fill(crate::ui::pane_fill(ui))
                            .stroke(egui::Stroke::new(1.0, crate::ui::pane_stroke(ui)))
                            .corner_radius(8.0)
                            .inner_margin(egui::Margin::symmetric(10, 8))
                            .show(ui, |ui| {
                                ui.label(
                                    RichText::new(format!("文件清单（{}）", rows.len()))
                                        .size(12.5)
                                        .strong(),
                                );
                                ui.add_space(4.0);
                                ScrollArea::vertical()
                                    .id_salt("merge_rows")
                                    .auto_shrink([false, false])
                                    .show(ui, |ui| {
                                        if rows.is_empty() {
                                            ui.label(
                                                RichText::new("两侧内容已经一致，无需写入。")
                                                    .size(12.5)
                                                    .strong(),
                                            );
                                        }
                                        // 处置标记的胶囊统一按「最长标签」的宽度画，所有文件名才会落在同一条竖线上
                                        let marker_w = rows
                                            .iter()
                                            .map(|e| {
                                                ui.painter()
                                                    .layout_no_wrap(
                                                        e.action_text(),
                                                        egui::FontId::proportional(11.5),
                                                        egui::Color32::WHITE,
                                                    )
                                                    .size()
                                                    .x
                                            })
                                            .fold(0.0, f32::max)
                                            + 14.0;
                                        for entry in rows.iter() {
                                            let write = entry.action.is_write();
                                            let conflict = entry.action == Action::SkipConflict;
                                            let local = preview.local_root.join(&entry.local_rel);
                                            let server = preview.server_root.join(&entry.server_rel);
                                            let regions = entry.regions_text();
                                            let ancestor = entry.ancestor.label();
                                            let note = entry.note.clone();
                                            let display = entry.display.clone();
                                            ui.horizontal(|ui| {
                                                // 处置标记走主页简约卡片同款胶囊：固定 104px 宽、左对齐，
                                                // 让所有文件名起点在一条竖线上；绿=会写、红=冲突、琥珀=跳过/待人工
                                                let disp_color = if conflict
                                                    || matches!(entry.action, Action::DeleteLocal | Action::DeleteServer)
                                                {
                                                    crate::ERR
                                                } else if write {
                                                    crate::OK
                                                } else {
                                                    crate::AMBER
                                                };
                                                let (fill, text_color) =
                                                    crate::home::pill_colors(ui, disp_color, true);
                                                let (_id, rect) =
                                                    ui.allocate_space(egui::vec2(marker_w, 20.0));
                                                ui.painter().rect_filled(rect, 10.0, fill);
                                                let galley = ui.painter().layout_no_wrap(
                                                    entry.action_text(),
                                                    egui::FontId::proportional(11.5),
                                                    text_color,
                                                );
                                                ui.painter().galley(
                                                    rect.center() - galley.size() / 2.0,
                                                    galley,
                                                    text_color,
                                                );
                                                ui.vertical(|ui| {
                                                    ui.horizontal(|ui| {
                                                        ui.label(
                                                            RichText::new(display)
                                                                .monospace()
                                                                .size(12.0),
                                                        )
                                                        .on_hover_text(format!(
                                                            "本地：{}\n服务器：{}",
                                                            local.display(),
                                                            server.display()
                                                        ));
                                                        if !regions.is_empty() {
                                                            ui.label(
                                                                RichText::new(regions)
                                                                    .size(11.0)
                                                                    .weak(),
                                                            );
                                                        }
                                                        if write && ancestor != "—" {
                                                            ui.label(
                                                                RichText::new(ancestor)
                                                                    .size(11.0)
                                                                    .weak(),
                                                            );
                                                        }
                                                    });
                                                    if !write && !note.is_empty() {
                                                        ui.label(
                                                            RichText::new(note).size(11.0).weak(),
                                                        );
                                                    }
                                                });
                                                ui.with_layout(
                                                    Layout::right_to_left(Align::Center),
                                                    |ui| {
                                                        if conflict
                                                            && ui
                                                                .button("手工处理")
                                                                .on_hover_text(
                                                                    "用 Beyond Compare 打开两侧文件对照",
                                                                )
                                                                .clicked()
                                                        {
                                                            let (left, right) =
                                                                crate::bcompare::order_sides(
                                                                    &server,
                                                                    &local,
                                                                    &self.cfg.bc_server_side,
                                                                );
                                                            self.open_bcompare(&[left, right], &[]);
                                                        }
                                                    },
                                                );
                                            });
                                            ui.add_space(2.0);
                                        }
                                    });
                            });
                    });
                    // 右：处理结果汇总表
                    ui.allocate_ui_with_layout(
                        egui::vec2(right_w, body_h),
                        Layout::top_down(Align::Min),
                        |ui| {
                        egui::Frame::new()
                            .fill(crate::ui::pane_fill(ui))
                            .stroke(egui::Stroke::new(1.0, crate::ui::pane_stroke(ui)))
                            .corner_radius(8.0)
                            .inner_margin(egui::Margin::symmetric(10, 8))
                            .show(ui, |ui| {
                                ui.label(RichText::new("处理结果").size(12.5).strong());
                                ui.add_space(4.0);
                                ScrollArea::vertical()
                                    .id_salt("merge_summary")
                                    .auto_shrink([false, false])
                                    .show(ui, |ui| {
                                        Self::merge_summary_row(
                                            ui,
                                            "写入",
                                            format!("{} 项", preview.writing()),
                                            crate::OK,
                                        );
                                        Self::merge_summary_row(
                                            ui,
                                            "行级合并",
                                            preview.line_merges().to_string(),
                                            crate::OK,
                                        );
                                        Self::merge_summary_row(
                                            ui,
                                            "整文件覆盖",
                                            preview.overwrites().to_string(),
                                            crate::OK,
                                        );
                                        Self::merge_summary_row(
                                            ui,
                                            "新增拷入",
                                            preview.copies().to_string(),
                                            crate::OK,
                                        );
                                        if preview.deletes() > 0 {
                                            Self::merge_summary_row(
                                                ui,
                                                "同步删除",
                                                format!("{} 项", preview.deletes()),
                                                crate::ERR,
                                            );
                                        }
                                        Self::merge_summary_row(
                                            ui,
                                            "跳过",
                                            format!("{} 项", preview.skipping()),
                                            crate::AMBER,
                                        );
                                        Self::merge_summary_row(
                                            ui,
                                            "冲突",
                                            preview.conflicts().to_string(),
                                            crate::ERR,
                                        );
                                        Self::merge_summary_row(
                                            ui,
                                            "待人工",
                                            preview.undecided().to_string(),
                                            crate::AMBER,
                                        );
                                        Self::merge_summary_row(
                                            ui,
                                            "两边都改",
                                            preview.binaries().to_string(),
                                            crate::AMBER,
                                        );
                                        Self::merge_summary_row(
                                            ui,
                                            "不在范围",
                                            preview.out_of_scope().to_string(),
                                            crate::AMBER,
                                        );
                                        Self::merge_summary_row(
                                            ui,
                                            "已一致",
                                            preview.agreed().to_string(),
                                            crate::OK,
                                        );
                                        Self::merge_summary_row(
                                            ui,
                                            "已核对",
                                            format!("{} 项", preview.checked),
                                            crate::OK,
                                        );
                                        if preview.unverified > 0 {
                                            Self::merge_summary_row(
                                                ui,
                                                "未核对内容",
                                                format!("{} 项", preview.unverified),
                                                crate::AMBER,
                                            );
                                        }
                                        if preview.budget_hit {
                                            Self::merge_summary_row(
                                                ui,
                                                "预算用满",
                                                format!("{} 项", preview.untried),
                                                crate::AMBER,
                                            );
                                        }
                                        ui.add_space(6.0);
                                        // 备份：展开 / 收起 + 清理
                                        let toggle = if dialog.show_backups {
                                            "收起备份记录"
                                        } else {
                                            "历次备份"
                                        };
                                        if ui.button(toggle).clicked() {
                                            dialog.show_backups = !dialog.show_backups;
                                        }
                                        if dialog.show_backups && dialog.backups.is_empty() {
                                            ui.label(
                                                RichText::new("还没有过一次合并写入，备份目录为空")
                                                    .size(11.0)
                                                    .weak(),
                                            );
                                        }
                                        if dialog.show_backups {
                                            for backup in dialog.backups.clone() {
                                                ui.horizontal(|ui| {
                                                    ui.label(
                                                        RichText::new(backup.name)
                                                            .monospace()
                                                            .size(11.5),
                                                    );
                                                    ui.label(
                                                        RichText::new(format!(
                                                            "{} 项 · {}",
                                                            backup.files,
                                                            crate::merge::human_bytes(backup.bytes)
                                                        ))
                                                        .size(11.0)
                                                        .weak(),
                                                    );
                                                    if backup.unfinished {
                                                        ui.label(
                                                            RichText::new("未完成")
                                                                .size(11.0)
                                                                .color(ink(ui, crate::AMBER)),
                                                        );
                                                    }
                                                    if ui
                                                        .add_enabled(
                                                            !running,
                                                            egui::Button::new("还原到这次"),
                                                        )
                                                        .on_hover_text(format!(
                                                            "还原：{}",
                                                            backup.dir.display()
                                                        ))
                                                        .clicked()
                                                    {
                                                        self.spawn_merge_restore(
                                                            dir,
                                                            backup.dir.clone(),
                                                        );
                                                    }
                                                });
                                            }
                                        }
                                        let stored = dialog
                                            .backups
                                            .iter()
                                            .filter(|item| !item.unfinished)
                                            .count();
                                        if ui
                                            .add_enabled(
                                                !running && !cleaning && stored > 0,
                                                egui::Button::new("清理备份"),
                                            )
                                            .on_hover_text(if stored == 0 {
                                                "备份目录里还没有已完成的备份"
                                            } else {
                                                "删掉已完成的历次备份；写到一半断掉的那次留着"
                                            })
                                            .clicked()
                                        {
                                            dialog.wipe = true;
                                            self.hint(
                                                "清理删的是这台机器上的备份副本，两边文件不动；删过之后那几次合并就退不回去了",
                                            );
                                        }
                                        if dialog.wipe {
                                            let finished: Vec<u64> = dialog
                                                .backups
                                                .iter()
                                                .filter(|item| !item.unfinished)
                                                .map(|item| item.bytes)
                                                .collect();
                                            let bytes: u64 = finished.iter().sum();
                                            ui.horizontal(|ui| {
                                                ui.label(
                                                    RichText::new(format!(
                                                        "删掉这 {} 次备份（约 {}）？",
                                                        finished.len(),
                                                        crate::merge::human_bytes(bytes)
                                                    ))
                                                    .size(11.5),
                                                );
                                                let yes = egui::Button::new(
                                                    RichText::new("确认清理")
                                                        .strong()
                                                        .color(crate::DANGER_TEXT),
                                                )
                                                .fill(crate::DANGER_FILL);
                                                if ui.add_enabled(!cleaning, yes).clicked() {
                                                    dialog.wipe = false;
                                                    self.spawn_merge_clean(dir);
                                                }
                                                if ui.button("先不清理").clicked() {
                                                    dialog.wipe = false;
                                                }
                                            });
                                            ui.label(
                                                RichText::new(backup_root.display().to_string())
                                                    .monospace()
                                                    .size(11.0)
                                                    .weak(),
                                            );
                                        }
                                    });
                            });
                    });
                });
            });
        if let Some(shown) = &shown {
            dialog.zoom.shown(shown.response.rect);
        }
        dialog.plan = plan;
        // 点标题栏的 × 时 egui 只把 open 写成 false，这一帧窗口还在淡出、show 照旧返回 Some；
        // open 是每帧新建的局部变量，不在这里丢掉内容，下一帧它又变回 true、窗口重新冒出来
        self.merge = if shown.is_none() || close || !open {
            None
        } else {
            Some(dialog)
        };
    }

    /// 汇总表里的一行：左胶囊标签、右数值，数值按处置语义上色（绿=会写/一致，琥珀=跳过/待人工，红=冲突）。
    /// 胶囊用主页简约卡片同款的配色（浅色粉彩底深字、深色半透明底原色字）。
    fn merge_summary_row(ui: &mut egui::Ui, label: &str, value: String, color: egui::Color32) {
        ui.horizontal(|ui| {
            let (fill, text_color) = crate::home::pill_colors(ui, color, true);
            egui::Frame::new()
                .fill(fill)
                .corner_radius(10.0)
                .inner_margin(egui::Margin::symmetric(7, 3))
                .show(ui, |ui| {
                    ui.label(RichText::new(label).size(11.5).color(text_color));
                });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(RichText::new(value).size(12.0).strong().color(ink(ui, color)));
            });
        });
        ui.add_space(2.0);
    }

    // ------------------------------------------------------------ 「全部上传」窗口

    /// 所有目录用同一条说明各起一个提交任务。会改动服务器，所以第一下只亮出确认按钮。
    /// 每个目录前有勾选框决定参不参与本次上传；点「待提交 X 项」可弹出明细核对。
    pub(crate) fn upload_all_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut dialog) = self.upload_all.clone() else {
            return;
        };
        // 窗口开着时目录列表可能增删：勾选状态跟着对齐，新出现的目录默认勾上
        dialog.checked.resize(self.cfg.dirs.len(), true);
        dialog.checked.truncate(self.cfg.dirs.len());
        if dialog.detail.is_some_and(|index| index >= self.cfg.dirs.len()) {
            dialog.detail = None;
        }
        // 行上显示的改动数只作参考（没检测过的写「未检测」），真正的清单由任务里现读 svn status
        let rows: Vec<(String, Option<usize>, bool)> = (0..self.cfg.dirs.len())
            .map(|index| {
                (
                    self.dir_label(index),
                    self.dirs.get(index).and_then(|view| view.changed),
                    self.pool.has(Kind::Commit, index),
                )
            })
            .collect();
        let total = rows.len();
        // 每个目录「还没更新到最新」的提醒：行上标一下，确认上传时再写进输出记录
        let outdated: Vec<Option<String>> =
            (0..total).map(|index| self.not_up_to_date_hint(index)).collect();
        let picked = dialog.checked.iter().filter(|checked| **checked).count();
        // 参与上传、又没更新到最新的目录：确认那一步要点名，别让人蒙着按下去
        let outdated_checked: Vec<usize> = (0..total)
            .filter(|index| dialog.checked.get(*index).copied().unwrap_or(false))
            .filter(|index| outdated.get(*index).and_then(|item| item.as_ref()).is_some())
            .collect();
        let busy = rows.iter().any(|(_, _, running)| *running);
        let mut open = true;
        let mut close = false;
        egui::Window::new("全部上传")
            .open(&mut open)
            .centered(ctx, "upload_all")
            .show(ctx, |ui| {
                ui.label(
                    RichText::new("风险：这一步会直接改动服务器上的仓库")
                        .strong()
                        .size(13.5)
                        .color(ink(ui, Color32::from_rgb(240, 100, 100))),
                );
                ui.label(
                    RichText::new(
                        "· 提交完别人立刻就能看到，本地撤不回来（要退回只能再提交一次）\n\
                         · 清单 = 各目录全部可提交项：未版本化(?) 提交时先加入版本控制、已丢失(!) 先标记删除，\n\
                         与修改项一起一次提交，不需要再手动标记删除和添加\n\
                         · 冲突 / 不完整的条目不会被带上；没有可提交改动的目录自动跳过\n\
                         · 勾选参与本次上传的目录（默认全选）；点「待提交 X 项」可先核对明细\n\
                         · 想逐条挑文件、自己核对差异，请用目录行里的「上传」",
                    )
                    .weak()
                    .size(11.5),
                );
                ui.separator();
                ui.label(RichText::new("提交说明（所有目录共用这一条）").strong().size(12.5));
                let field = ui.add_sized(
                    Vec2::new(500.0, 54.0),
                    TextEdit::multiline(&mut dialog.message).hint_text("填写本次提交说明"),
                );
                if dialog.focus {
                    field.request_focus();
                    dialog.focus = false;
                }
                ui.separator();
                ui.aligned_row(|ui| {
                    ui.label(RichText::new("参与上传的目录").strong().size(12.5));
                    if ui.button("全选").clicked() {
                        dialog.checked.iter_mut().for_each(|checked| *checked = true);
                    }
                    if ui.button("全不选").clicked() {
                        dialog.checked.iter_mut().for_each(|checked| *checked = false);
                    }
                    if picked == 0 {
                        ui.label(RichText::new("一个目录都没勾").weak().size(11.5));
                    }
                });
                ScrollArea::vertical()
                    .id_salt("upload_all_dirs")
                    .max_height(150.0)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for (index, (label, changed, running)) in rows.iter().enumerate() {
                            ui.aligned_row(|ui| {
                                let mut checked =
                                    dialog.checked.get(index).copied().unwrap_or(true);
                                if ui
                                    .checkbox(&mut checked, "")
                                    .on_hover_text("勾掉就不参与本次批量上传")
                                    .changed()
                                {
                                    dialog.checked[index] = checked;
                                }
                                ui.label(RichText::new(label).size(12.5).strong());
                                let note = match *changed {
                                    Some(0) => "无改动，自动跳过".to_owned(),
                                    Some(count) => format!("待提交 {count} 项"),
                                    None => "未检测，点击读取".to_owned(),
                                };
                                if matches!(*changed, None | Some(1..)) {
                                    // 点击「待提交 X 项」弹出明细窗口，核对具体哪些文件变了
                                    let hit = ui.add(
                                        egui::Label::new(
                                            RichText::new(note)
                                                .size(11.5)
                                                .underline()
                                                .color(ink(ui, Color32::from_rgb(240, 190, 70))),
                                        )
                                        .sense(egui::Sense::click()),
                                    );
                                    if hit.clicked() {
                                        dialog.detail = Some(index);
                                        // 打开明细时现读一遍 svn status，看到的清单是新鲜的
                                        self.spawn_status(index, false);
                                    }
                                    hit.on_hover_text("点击查看该目录的变动文件明细");
                                } else {
                                    ui.label(
                                        RichText::new(note)
                                            .size(11.5)
                                            .color(ink(ui, Color32::from_gray(130))),
                                    );
                                }
                                if outdated.get(index).and_then(|item| item.as_ref()).is_some() {
                                    ui.label(
                                        RichText::new("未更新")
                                            .size(11.0)
                                            .strong()
                                            .color(ink(ui, Color32::from_rgb(240, 190, 70))),
                                    )
                                    .on_hover_text(
                                        "该目录还没更新到最新版本：服务器上还有没拉到本地的新改动，\
                                         这次上传是在旧基线上做的；可以勾掉它先回主页点「更新」",
                                    );
                                }
                                if *running {
                                    ui.spinner();
                                    ui.label(RichText::new("该目录正在提交").weak().size(11.5));
                                }
                            });
                        }
                    });
                ui.separator();
                ui.horizontal(|ui| {
                    if !outdated_checked.is_empty() {
                        // 拦截的第二下：点名哪几个目录没更新到最新，让人知道自己是拿旧基线在上传
                        let names = outdated_checked
                            .iter()
                            .map(|index| rows[*index].0.clone())
                            .collect::<Vec<_>>()
                            .join("、");
                        ui.label(
                            RichText::new(format!("未更新到最新：{names}"))
                                .strong()
                                .size(12.0)
                                .color(ink(ui, Color32::from_rgb(240, 100, 100))),
                        )
                        .on_hover_text(
                            "这些目录在服务器上存在尚未同步到本地的新改动，本次上传将基于旧基线执行；\
                             若他人也修改了同一处，将产生冲突。如需避免，请取消后先更新这些目录。",
                        );
                    }
                });
                ui.aligned_row(|ui| {
                    let ready = total > 0 && picked > 0 && !busy;
                    if dialog.confirm {
                        let caption = if outdated_checked.is_empty() {
                            format!("确认上传？{picked} 个目录的改动会立即进服务器")
                        } else {
                            format!(
                                "确认上传？{picked} 个目录（{} 个未更新到最新）",
                                outdated_checked.len()
                            )
                        };
                        let button = egui::Button::new(
                            RichText::new(caption)
                                .strong()
                                .color(crate::DANGER_TEXT),
                        )
                        .fill(crate::DANGER_FILL);
                        if ui.add_enabled(ready, button).clicked() {
                            let message = dialog.message.clone();
                            for index in 0..total {
                                if dialog.checked.get(index).copied().unwrap_or(false) {
                                    // 没更新到最新也照样上传，但先往输出记录写一条：
                                    // 窗口一关提示就没了，这条留在记录里能对上往后查
                                    if let Some(warning) =
                                        outdated.get(index).and_then(|item| item.clone())
                                    {
                                        let label = self.dir_label(index);
                                        self.push(
                                            Level::Warning,
                                            format!("{label}：{warning}，建议先「更新」再上传"),
                                        );
                                    }
                                    self.spawn_upload_all(index, message.clone());
                                }
                            }
                            self.push(
                                Level::Info,
                                format!(
                                    "全部上传：已给 {picked} 个目录排上提交任务（没有改动的会自动跳过）"
                                ),
                            );
                            close = true;
                        }
                        if ui.button("先不上传").clicked() {
                            dialog.confirm = false;
                        }
                    } else if ui
                        .add_enabled(
                            ready,
                            egui::Button::new(
                                RichText::new(format!(
                                    "全部上传（{}）",
                                    if picked == total {
                                        format!("{total} 个目录")
                                    } else {
                                        format!("勾选 {picked} / {total} 个目录")
                                    }
                                ))
                                .strong(),
                            ),
                        )
                        .clicked()
                    {
                        dialog.confirm = true;
                        self.hint("此操作将改动服务器：核对上方目录后，再次点击「确认上传」");
                    }
                    if total == 0 {
                        ui.label(RichText::new("目录列表是空的").weak().size(11.5));
                    } else if busy {
                        ui.label(
                            RichText::new("有目录正在提交，等它结束后才能批量上传")
                                .weak()
                                .size(11.5),
                        );
                    } else if picked == 0 {
                        ui.label(RichText::new("先勾选至少一个目录").weak().size(11.5));
                    }
                });
            });
        // 「待提交明细」子弹窗：叠在「全部上传」窗口之上
        if let Some(index) = dialog.detail {
            self.upload_detail_window(ctx, index, &mut dialog);
        }
        if open && !close {
            self.upload_all = Some(dialog);
        } else {
            self.upload_all = None;
        }
    }

    /// 「全部上传」窗口里点「待提交 X 项」弹出的明细窗口：
    /// 列出该目录会进本次提交的全部条目（? / ! 也在内，提交时自动补 add / delete）。
    fn upload_detail_window(&mut self, ctx: &egui::Context, index: usize, dialog: &mut UploadAll) {
        let label = self.dir_label(index);
        let path = self
            .dir_path(index)
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        let reading = self.pool.has(Kind::Status, index);
        // 先把行状态克隆下来，闭包里还要借用 self 去发起「重新读取」
        let view = self.dirs.get(index).cloned().unwrap_or_default();
        let mut open = true;
        egui::Window::new(format!("待提交明细 · {label}"))
            .open(&mut open)
            .centered(ctx, "pending_detail")
            .default_size(Vec2::new(560.0, 430.0))
            .show(ctx, |ui| {
                if !path.is_empty() {
                    ui.label(RichText::new(path.as_str()).size(11.5).weak());
                }
                // 计数口径与提交页一致：? 折进新增、! 折进删除
                let mut added = 0;
                let mut modified = 0;
                let mut deleted = 0;
                for entry in &view.changes {
                    match entry.item {
                        crate::svn::Item::Added | crate::svn::Item::Unversioned => added += 1,
                        crate::svn::Item::Modified | crate::svn::Item::Replaced => modified += 1,
                        crate::svn::Item::Deleted | crate::svn::Item::Missing => deleted += 1,
                        _ => {}
                    }
                }
                ui.aligned_row(|ui| {
                    ui.label(RichText::new(format!("共 {} 项", view.changes.len()))
                        .strong()
                        .size(12.5));
                    ui.label(
                        RichText::new(format!("新增 {added}"))
                            .size(12.0)
                            .color(ink(ui, Color32::from_rgb(60, 190, 110))),
                    );
                    ui.label(
                        RichText::new(format!("修改 {modified}"))
                            .size(12.0)
                            .color(ink(ui, Color32::from_rgb(90, 160, 240))),
                    );
                    ui.label(
                        RichText::new(format!("删除 {deleted}"))
                            .size(12.0)
                            .color(ink(ui, Color32::from_rgb(235, 90, 90))),
                    );
                    if ui.button("重新读取").clicked() {
                        self.spawn_status(index, false);
                    }
                    if reading {
                        ui.spinner();
                        ui.label(RichText::new("正在读取 svn status …").weak().size(11.5));
                    }
                });
                ui.label(
                    RichText::new(
                        "这些就是「全部上传」会提交的条目：未版本化(?)、已丢失(!) 提交时会先加入版本控制或标记删除，无需手动标记",
                    )
                    .weak()
                    .size(11.0),
                );
                ui.separator();
                ScrollArea::vertical()
                    .id_salt(("upload_detail", index))
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 2.0;
                        if view.changes.is_empty() && !reading {
                            ui.label(RichText::new(if view.changed.is_some() {
                                "该目录没有会进本次上传的改动"
                            } else {
                                "尚未读到该目录的修改清单，点上方「重新读取」"
                            })
                            .weak());
                        }
                        for entry in &view.changes {
                            let rgb = entry.item.color();
                            let color = ink(ui, Color32::from_rgb(rgb.0, rgb.1, rgb.2));
                            // 显示相对目录的路径（能看出子目录里的变动），取不到就退回文件名
                            let shown = entry
                                .path
                                .strip_prefix(&path)
                                .map(|rel| rel.trim_start_matches(['\\', '/']))
                                .filter(|rel| !rel.is_empty())
                                .unwrap_or(&entry.name);
                            ui.horizontal(|ui| {
                                ui.label(
                                    RichText::new(entry.item.mark().to_string())
                                        .strong()
                                        .monospace()
                                        .size(12.5)
                                        .color(color),
                                )
                                .on_hover_text(entry.item.text());
                                ui.label(RichText::new(shown).size(12.0).color(color))
                                    .on_hover_text(format!(
                                        "{}\n状态：{}",
                                        entry.path, entry.item.text()
                                    ));
                                if entry.item.needs_add() {
                                    ui.label(
                                        RichText::new("提交时新增")
                                            .size(10.5)
                                            .color(ink(ui, Color32::from_rgb(240, 190, 70))),
                                    )
                                    .on_hover_text(
                                        "这个文件目前只在本地，提交时会先加入版本控制（svn add），再作为新文件上传到服务器",
                                    );
                                } else if entry.item.needs_delete() {
                                    ui.label(
                                        RichText::new("提交时删除")
                                            .size(10.5)
                                            .color(ink(ui, Color32::from_rgb(240, 190, 70))),
                                    )
                                    .on_hover_text(
                                        "这个文件本地已经删掉了，提交时会先标记删除（svn delete），服务器上的那份也一并删除",
                                    );
                                }
                            });
                        }
                    });
            });
        if !open {
            dialog.detail = None;
        }
    }

    /// 「发现新版本」确认对话框：顶部「↑ 新版本」按钮与设置里的「立即更新」
    /// 都打开它，点「开始更新」后直接进入下载，不再绕道设置页。
    pub(crate) fn update_confirm_dialog(&mut self, ctx: &egui::Context) {
        if !self.show_update_confirm {
            return;
        }
        let Some(manifest) = self.update_info.clone() else {
            // 没有可用的更新信息（理论上只有 update_ready 才打开）就顺手关掉
            self.show_update_confirm = false;
            return;
        };
        let downloading = self.pool.has(Kind::DownloadUpdate, usize::MAX);
        let mut open = self.show_update_confirm;
        // 点了「稍后再说」要到窗口收尾时才写回，见下面那句
        let mut later = false;
        egui::Window::new("发现新版本")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(440.0)
            .centered(ctx, "update_available")
            .show(ctx, |ui| {
                let latest = manifest.version.trim();
                // 版本号没变、只是更新源重新发布过构建时不做「旧 → 新」的对比：
                // 同一个号画两遍看着像没更新，这里只把要装上去的版本说清楚
                let replaced = update::same_version(latest, APP_VERSION);
                ui.horizontal(|ui| {
                    ui.add_space(2.0);
                    if !replaced {
                        ui.label(RichText::new(format!("V{APP_VERSION}")).size(17.0).weak());
                        ui.label(RichText::new("→").size(17.0).weak());
                    }
                    // 新旧版本号同字号、同字重：字大一号会把两个号看成两个量级。
                    // 新版号走金色（与日志区 Warning 同一档），在深浅两套主题下都压得住
                    ui.label(
                        RichText::new(format!("V{latest}"))
                            .size(17.0)
                            .strong()
                            .color(ink(ui, Color32::from_rgb(240, 190, 70))),
                    );
                    if manifest.prerelease {
                        ui.label(RichText::new("测试版").size(11.5).weak());
                    }
                    if replaced {
                        ui.label(RichText::new("内容有更新（版本号没变）").size(11.5).weak());
                    }
                    if !manifest.published_at.trim().is_empty() {
                        ui.label(
                            RichText::new(format!("发布于 {}", manifest.published_at.trim()))
                                .weak()
                                .size(11.5),
                        );
                    }
                });
                // 更新说明放在与明细窗口同风格的灰边卡片里；支持多行（发布端用 \n 或 notes.txt）
                if !manifest.notes.trim().is_empty() {
                    ui.add_space(6.0);
                    let (card_fill, card_stroke) = if ui.visuals().dark_mode {
                        (Color32::from_gray(40), Color32::from_gray(65))
                    } else {
                        (Color32::WHITE, Color32::from_gray(200))
                    };
                    Frame::new()
                        .inner_margin(8.0)
                        .corner_radius(5.0)
                        .fill(card_fill)
                        .stroke(egui::Stroke::new(1.0, card_stroke))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.label(RichText::new("更新说明").strong().size(12.0));
                            ScrollArea::vertical()
                                .max_height(130.0)
                                .id_salt("update_notes")
                                .auto_shrink([false, false])
                                .show(ui, |ui| {
                                    // egui Label 按原文渲染换行，发布时写的多行说明原样展示
                                    ui.label(RichText::new(manifest.notes.trim()).size(12.0));
                                });
                        });
                }
                ui.add_space(6.0);
                if downloading {
                    // 下载中：状态直接在对话框里，不用翻日志区
                    ui.aligned_row(|ui| {
                        ui.spinner();
                        ui.label(RichText::new("正在下载新版本并校验…").size(12.5));
                    });
                    ui.label(
                        RichText::new("完成后程序会自动换上并重启；进度详情见底部输出区。")
                            .weak()
                            .size(11.5),
                    );
                } else if let Some(error) = &self.update_error {
                    ui.label(
                        RichText::new(format!("上次下载失败：{error}"))
                            .size(11.5)
                            .color(ink(ui, Color32::from_rgb(240, 100, 100))),
                    );
                } else {
                    ui.label(
                        RichText::new(
                            "点「开始更新」后下载新版本并自动校验，然后程序自动退出、把新版本换上并重新启动。",
                        )
                        .weak()
                        .size(11.5),
                    );
                }
                ui.add_space(8.0);
                ui.aligned_row(|ui| {
                    // 动作行整组靠右；add 顺序反过来，「开始更新」才仍然排在「稍后再说」左边
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.add_enabled(!downloading, egui::Button::new("稍后再说")).clicked() {
                            later = true;
                        }
                        let start = ui.add_enabled(
                            !downloading,
                            egui::Button::new(
                                RichText::new(if downloading { "下载中…" } else { "开始更新" })
                                    .strong(),
                            ),
                        );
                        if start.clicked() {
                            // 保持在对话框里看下载状态，不再一按就关
                            self.update_error = None;
                            self.begin_update();
                        }
                    });
                });
            });
        // 关窗口这件事统一在这里收口：在按钮里改 self.show_update_confirm 会被下面这行
        // 按旧的 open 覆盖回去，窗口关不掉（点「稍后再说」等于没点）
        self.show_update_confirm = open && !later;
    }

    // ---------------------------------------------------- 「差异过滤与提交屏蔽」窗口

    /// 这扇窗与里面四格输入框都是固定尺寸：窗口不许拖大拖小，输入框也不跟着变形。四格分作两页
    /// 标签，每页两格，切页不动窗口大小。
    /// 目录行「更多 → 差异过滤与提交屏蔽」开的那扇窗：第一页是「本地差异比对」的过滤条件
    /// （`bc_filter`，这里是唯一编辑入口）与「屏蔽可提交项」（`commit_excludes`，决定哪些改动
    /// 不进本次提交），第二页是「参与合并的文件」白名单与「不参与合并的文件」黑名单
    /// （`merge_include` 与 `merge_exclude`，只在双向合并时放行与收回范围，既不改差异数也不管提交）。
    /// 四格都落盘、长期生效；各管各的口径，互不牵连。边写边存，「确认」关窗并按新规则重读本目录。
    pub(crate) fn commit_rules_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut dialog) = self.commit_rules.clone() else {
            return;
        };
        let dir = dialog.dir;
        // 窗口开着时目录可能已被移除：这个窗口没有归属，直接收掉
        if self.cfg.dirs.get(dir).is_none() {
            self.commit_rules = None;
            return;
        }
        let label = self.dir_label(dir);
        // 双向合并插件卸载后，第二页（合并范围）就没有意义：不露那个标签，也把页码拉回第一页
        let merge_on = self.plugin_installed(crate::plugins::PluginId::Merge);
        if !merge_on {
            dialog.page = 0;
        }
        // 命中数按最近一次读取的完整清单现算（被挡掉的那些也在里面）：这扇窗从主页「更多」
        // 就能开，那时没有提交页的掩码可读，而 changes 早滤干净了、数出来永远是 0
        let excludes = self.cfg.dirs[dir].commit_excludes.clone();
        let read_once = self.dirs.get(dir).and_then(|view| view.changed).is_some();
        let hidden = match (self.dirs.get(dir), self.dir_path(dir)) {
            (Some(view), Some(root)) => {
                crate::commit::excluded_count(&view.all_changes, &root, &excludes)
            }
            _ => 0,
        };
        // 过滤那格只能报上次比对的数：新条件要重跑一遍比对才落地
        let diff_note = match self.dirs.get(dir).and_then(|view| view.local_diff.as_ref()) {
            Some(summary) => format!("上次比对：本地差异 {} 项", summary.total()),
            None => "上次比对：尚未比对".to_owned(),
        };
        let excludes_before = self.cfg.dirs[dir].commit_excludes.clone();
        let merge_before = self.cfg.dirs[dir].merge_include.clone();
        let exclude_before = self.cfg.dirs[dir].merge_exclude.clone();
        let mut open = true;
        // 标题栏的 ✕ 由 `open` 管，「确认」按钮另记一笔：Window::open 已经把 open 借走，
        // 闭包里再改就是同一份可变借用两次
        let mut close = false;
        let mut confirmed = false;
        egui::Window::new(format!("差异过滤、提交屏蔽与合并范围 · {label}"))
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(560.0)
            .centered(ctx, "commit_rules")
            .show(ctx, |ui| {
                // 四格输入框定死尺寸：宽度是窗口宽减去两侧窗框内边距（egui 默认 6），
                // 高度约四行半、写满就地滚动；窗口本身不可拖，两页各摆两格，切页也不会跳高
                let field = Vec2::new(548.0, 74.0);
                ui.label(
                    RichText::new(
                        "本目录的四套规则均一行一条，保存于配置中长期生效，各管各的口径、互不牵连。分作两页只为少占屏幕，改动照样边写边存。",
                    )
                    .weak()
                    .size(11.5),
                );
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui
                        .selectable_label(dialog.page == 0, "差异过滤与提交屏蔽")
                        .on_hover_text("上格决定差异比对里看不看得到，下格决定条目进不进本次提交")
                        .clicked()
                    {
                        dialog.page = 0;
                    }
                    if merge_on
                        && ui
                            .selectable_label(dialog.page == 1, "双向合并范围")
                            .on_hover_text("划定双向合并涉及哪些文件：先按白名单放行，再按黑名单收回")
                            .clicked()
                    {
                        dialog.page = 1;
                    }
                });
                ui.add_space(6.0);
                if dialog.page == 0 {
                    ui.label(RichText::new("差异比对过滤条件 · 存本目录配置").strong().size(12.5));
                    let filter_field = rules_field(
                        ui,
                        "rules_filter",
                        &mut dialog.filter_buf,
                        "-*.iml\n-.project\n-bin\\",
                        field,
                    );
                    if dialog.focus {
                        filter_field.request_focus();
                        dialog.focus = false;
                    }
                    ui.label(
                        RichText::new(
                            "语法与 Beyond Compare 一致：`-` 开头表示排除，`;` 亦可分隔，结尾 \\ 表示目录，`.\\` 锚定根目录，* 与 ? 为通配符。此格仅作用于差异比较，不影响条目是否进入提交。",
                        )
                        .weak()
                        .size(11.0),
                    );
                    ui.aligned_row(|ui| {
                        ui.label(RichText::new(diff_note).weak().size(11.0));
                        if ui.button("清空筛选条件").clicked() {
                            dialog.filter_buf.clear();
                        }
                    });
                    ui.add_space(8.0);

                    ui.label(RichText::new("屏蔽可提交项 · 存本目录配置").strong().size(12.5));
                    rules_field(
                        ui,
                        "rules_excludes",
                        &mut self.cfg.dirs[dir].commit_excludes,
                        "bin\\\n*.log\n.\\auth.json",
                        field,
                    );
                    ui.label(
                        RichText::new(
                            "语法与上一格相同，无需书写 `-`（缺失时自动补全）；多段路径未带 `.`\\ 锚定时也会自动补上，否则不会命中。两格规则分别存储，互不影响。",
                        )
                        .weak()
                        .size(11.0),
                    );
                    ui.aligned_row(|ui| {
                        ui.label(
                            RichText::new(if !read_once {
                                "读取本目录的修改状态后即可显示命中数".to_owned()
                            } else if hidden > 0 {
                                format!("当前命中 {hidden} 项：不进入提交清单，提交页中亦不显示")
                            } else {
                                "当前未命中任何条目".to_owned()
                            })
                            .weak()
                            .size(11.0),
                        );
                        if ui.button("清空屏蔽规则").clicked() {
                            self.cfg.dirs[dir].commit_excludes.clear();
                        }
                    });
                } else {
                    ui.label(RichText::new("参与合并的文件 · 存本目录配置").strong().size(12.5));
                    rules_field(
                        ui,
                        "rules_merge_include",
                        &mut self.cfg.dirs[dir].merge_include,
                        "*.java\n*.xml\nsrc\\main\\",
                        field,
                    );
                    ui.label(
                        RichText::new(
                            "语法与「差异比对过滤条件」相同，无需书写 `+`（缺失时自动补全）。本格只划定「双向合并」涉及哪些文件，白名单外的差异原样保留；留空时按内置的代码与文本类型清单执行。",
                        )
                        .weak()
                        .size(11.0),
                    );
                    ui.aligned_row(|ui| {
                        if ui.button("清空白名单").clicked() {
                            self.cfg.dirs[dir].merge_include.clear();
                        }
                    });
                    ui.add_space(8.0);

                    ui.label(RichText::new("不参与合并的文件 · 存本目录配置").strong().size(12.5));
                    rules_field(
                        ui,
                        "rules_merge_exclude",
                        &mut self.cfg.dirs[dir].merge_exclude,
                        "target\\\n*.min.js\n.\\pom.xml",
                        field,
                    );
                    ui.label(
                        RichText::new(
                            "语法与「屏蔽可提交项」相同。本格在白名单之后再次收窄范围：凡命中的差异均不参与合并，即使其同时满足白名单；留空时不额外排除。",
                        )
                        .weak()
                        .size(11.0),
                    );
                    ui.aligned_row(|ui| {
                        ui.label(
                            RichText::new("不参与合并的条数见「双向合并」预览页脚的「不在范围 N 项」")
                                .weak()
                                .size(11.0),
                        );
                        if ui.button("清空黑名单").clicked() {
                            self.cfg.dirs[dir].merge_exclude.clear();
                        }
                    });
                }
                ui.add_space(8.0);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui
                        .add(egui::Button::new(RichText::new("确认").strong()))
                        .on_hover_text(
                            "关闭窗口，并按当前规则重新读取本目录：主页的「待提交 N」「本地差异 N」\
                             与此处的命中数一并更新",
                        )
                        .clicked()
                    {
                        close = true;
                        confirmed = true;
                    }
                });
            });
        // 行视图拼回配置里的分号串；只在真改过时写盘，每帧 persist 会把配置反复落盘
        let filter = crate::commit::bc_filter_rules(&dialog.filter_buf);
        let mut changed = false;
        if self.cfg.dirs[dir].bc_filter != filter {
            self.cfg.dirs[dir].bc_filter = filter;
            dialog.filter_dirty = true;
            changed = true;
        }
        if self.cfg.dirs[dir].commit_excludes != excludes_before {
            changed = true;
        }
        // 白名单只划合并范围，不影响差异数与提交清单，改过只要落盘
        if self.cfg.dirs[dir].merge_include != merge_before {
            changed = true;
        }
        // 黑名单是白名单之后的再收窄，落盘口径与白名单一致
        if self.cfg.dirs[dir].merge_exclude != exclude_before {
            changed = true;
        }
        if changed {
            self.persist();
        }
        if open && !close {
            self.commit_rules = Some(dialog);
        } else {
            self.commit_rules = None;
            if confirmed {
                // 确认是这扇窗唯一会主动重读的出口：检测带着本地差异比对一起跑，
                // 主页的「待提交 N」「本地差异 N」和窗口里的命中数按新规则一次算齐
                self.spawn_refresh(dir, false);
            } else if dialog.filter_dirty {
                // 筛选条件就是差异口径，改完在关窗时按新口径重算一次
                self.spawn_local_diff(dir, false);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 正常改写：只有以 from 开头的地址才换前缀，剩下的尾巴原样保留
    #[test]
    fn rewrite_url_swaps_only_the_prefix() {
        assert_eq!(
            rewrite_url(
                "http://192.168.1.251:8080/svn/hrp",
                "http://192.168.1.251:8080/svn",
                "https://svn.example.com/repo",
            )
            .as_deref(),
            Some("https://svn.example.com/repo/hrp")
        );
    }

    /// 两头多打的斜杠不该算进前缀
    #[test]
    fn rewrite_url_trims_trailing_slashes() {
        assert_eq!(
            rewrite_url("http://old/svn/hrp", "http://old/svn/", "https://new/repo/").as_deref(),
            Some("https://new/repo/hrp")
        );
    }

    /// 改不动的三种情况：前缀不是开头、新前缀为空、旧前缀为空；
    /// 中段出现同样不算前缀
    #[test]
    fn rewrite_url_rejects_bad_input() {
        assert_eq!(
            rewrite_url("http://old/hrp", "http://other", "https://new"),
            None,
            "前缀不是当前地址的开头"
        );
        assert_eq!(rewrite_url("http://old/hrp", "http://old", ""), None, "新前缀为空");
        assert_eq!(rewrite_url("http://old/hrp", "", "https://new"), None, "旧前缀为空");
        assert_eq!(rewrite_url("http://a/b", "b", "c"), None, "中段出现不算前缀");
    }

    /// 批量分组只动勾选的目录：前后空白掐掉，空串就是清除
    #[test]
    fn apply_batch_group_writes_only_checked_dirs() {
        let _appdata = crate::testbed::lock_appdata();
        std::env::set_var(
            "APPDATA",
            std::env::temp_dir().join("repo_manager_batch_group_test"),
        );
        let mut app = crate::testbed::stub_app(false);
        let n = app.apply_batch_group(&[true, false, true, false], "  生产  ");
        assert_eq!(n, 2, "勾了两个该改两个");
        assert_eq!(app.cfg.dirs[0].group, "生产");
        assert_eq!(app.cfg.dirs[1].group, "", "没勾的不动");
        assert_eq!(app.cfg.dirs[2].group, "生产");

        let n = app.apply_batch_group(&[true, false, false, false], "");
        assert_eq!(n, 1, "本来就没分组的目录不算改动");
        assert_eq!(app.cfg.dirs[0].group, "", "空串即清除");
    }

    /// 批量标记：tag 非空连颜色一起写，tag 为空连颜色一起清；没勾的不受牵连
    #[test]
    fn apply_batch_tag_sets_and_clears_with_color() {
        let _appdata = crate::testbed::lock_appdata();
        std::env::set_var(
            "APPDATA",
            std::env::temp_dir().join("repo_manager_batch_tag_test"),
        );
        let mut app = crate::testbed::stub_app(false);
        let n = app.apply_batch_tag(&[true, true, false, false], " 冻结 ", "#FF8040");
        assert_eq!(n, 2);
        assert_eq!(app.cfg.dirs[0].tag, "冻结");
        assert_eq!(app.cfg.dirs[0].tag_color, "#FF8040");
        assert_eq!(app.cfg.dirs[1].tag_color, "#FF8040");

        let n = app.apply_batch_tag(&[true, false, false, false], "  ", "");
        assert_eq!(n, 1);
        assert_eq!(app.cfg.dirs[0].tag, "", "tag 为空连颜色一起清");
        assert_eq!(app.cfg.dirs[0].tag_color, "");
        assert_eq!(app.cfg.dirs[1].tag, "冻结", "没勾的不受牵连");
        assert_eq!(app.cfg.dirs[1].tag_color, "#FF8040");
    }

    /// 只跑合并窗口那一帧：这扇窗挂在帧末，测试台那条路只跑主页，不补调就永远画不出来
    fn merge_frame(
        app: &mut RepoApp,
        ctx: &egui::Context,
        events: Vec<egui::Event>,
    ) -> Vec<(String, egui::Rect)> {
        let mut input = crate::testbed::base_input();
        input.events = events;
        let mut out = ctx.run_ui(input, |ui| app.merge_dialog(ui.ctx()));
        out.textures_delta.clear();
        out.shapes
            .iter()
            .filter_map(|item| match &item.shape {
                egui::Shape::Text(text) => {
                    Some((text.galley.text().to_string(), item.shape.visual_bounding_rect()))
                }
                _ => None,
            })
            .filter(|(text, _)| !text.trim().is_empty())
            .collect()
    }

    /// 插件没装时，合并窗口整扇收起（即使状态里还挂着 merge），而不是画一半；
    /// 装回来要能立刻恢复
    #[test]
    fn the_merge_window_stays_shut_when_the_plugin_is_uninstalled() {
        let _appdata = crate::testbed::lock_appdata();
        std::env::set_var(
            "APPDATA",
            std::env::temp_dir().join("repo_manager_merge_plugin_test"),
        );
        let ctx = egui::Context::default();
        let mut app = crate::testbed::stub_app(false);
        app.merge = Some(crate::MergePreview {
            dir: 0,
            plan: Some(merge_fixture()),
            ..Default::default()
        });
        // 移除双向合并（dll 不在 = 没装）
        app.plugin_present = Some(false);
        merge_frame(&mut app, &ctx, Vec::new());
        let drawn = merge_frame(&mut app, &ctx, Vec::new());
        let listed = |drawn: &[(String, egui::Rect)]| {
            drawn.iter().any(|(text, _)| text.starts_with("文件清单"))
        };
        assert!(!listed(&drawn), "插件没装时不该画合并窗口");
        assert!(app.merge.is_none(), "被挡住时要把窗口状态清掉，别留着下帧又冒出来");

        // 装回来就恢复（把 dll 放回插件目录）
        app.plugin_present = Some(true);
        app.merge = Some(crate::MergePreview {
            dir: 0,
            plan: Some(merge_fixture()),
            ..Default::default()
        });
        merge_frame(&mut app, &ctx, Vec::new());
        let drawn = merge_frame(&mut app, &ctx, Vec::new());
        assert!(listed(&drawn), "装回来后窗口要能画出来");
    }

    /// 入口也挡：插件没装时 open_merge 不能起预览任务、不能开窗口
    #[test]
    fn open_merge_is_refused_when_the_plugin_is_uninstalled() {
        let _appdata = crate::testbed::lock_appdata();
        std::env::set_var(
            "APPDATA",
            std::env::temp_dir().join("repo_manager_merge_plugin_open_test"),
        );
        let mut app = crate::testbed::stub_app(false);
        app.plugin_present = Some(false);
        app.open_merge(0);
        assert!(app.merge.is_none(), "插件没装时不能开合并窗口");
    }

    fn has(drawn: &[(String, egui::Rect)], label: &str) -> bool {
        drawn.iter().any(|(text, _)| text == label)
    }

    fn click_of(drawn: &[(String, egui::Rect)], label: &str) -> egui::Rect {
        drawn
            .iter()
            .find(|(text, _)| text == label)
            .map(|(_, rect)| *rect)
            .unwrap_or_else(|| {
                panic!(
                    "窗口里没画出「{label}」：{:?}",
                    drawn.iter().map(|(t, _)| t).collect::<Vec<_>>()
                )
            })
    }

    /// 一份自造的清单：五种处置各一行，外加一行「已一致」——它只进页脚计数，不该占一行
    fn merge_fixture() -> crate::merge::Plan {
        use crate::merge::{Action, AncInfo, Entry, Finger, Plan, Region, RegionKind};
        let finger = |size: u64, modified: u64| Some(Finger { size, modified });
        Plan {
            entries: vec![
                Entry {
                    rel: "src/a.java".to_owned(),
                    display: r"src\A.java".to_owned(),
                    local_rel: r"src\A.java".to_owned(),
                    server_rel: "src/a.java".to_owned(),
                    action: Action::MergeBoth,
                    ancestor: AncInfo::Rev(190),
                    regions: vec![Region {
                        anc: (1, 2),
                        local: Some((1, 2)),
                        server: None,
                        kind: RegionKind::LocalOnly,
                    }],
                    local: finger(120, 7),
                    server: finger(140, 9),
                    write_local: None,
                    write_server: Some(b"merged".to_vec()),
                    delete_local: false,
                    delete_server: false,
                    observed_local: None,
                    observed_server: None,
                    note: String::new(),
                },
                Entry {
                    rel: "src/b.java".to_owned(),
                    display: r"src\B.java".to_owned(),
                    local_rel: r"src\B.java".to_owned(),
                    server_rel: "src/b.java".to_owned(),
                    action: Action::SkipConflict,
                    ancestor: AncInfo::Base,
                    regions: vec![Region {
                        anc: (3, 4),
                        local: Some((3, 4)),
                        server: Some((3, 4)),
                        kind: RegionKind::Conflict,
                    }],
                    local: finger(90, 5),
                    server: finger(96, 6),
                    write_local: None,
                    write_server: None,
                    delete_local: false,
                    delete_server: false,
                    observed_local: None,
                    observed_server: None,
                    note: String::new(),
                },
                Entry {
                    rel: "src/c.java".to_owned(),
                    display: r"src\C.java".to_owned(),
                    local_rel: r"src\C.java".to_owned(),
                    server_rel: "src/c.java".to_owned(),
                    action: Action::SkipUndecided,
                    ancestor: AncInfo::Unknown(crate::merge::Uncertain::WindowClosed {
                        distance: 60,
                        window: 15,
                    }),
                    regions: Vec::new(),
                    local: finger(60, 3),
                    server: finger(64, 4),
                    write_local: None,
                    write_server: None,
                    delete_local: false,
                    delete_server: false,
                    observed_local: None,
                    observed_server: None,
                    note: "往回 15 版内都没有比基线更接近的状态（与基线差 60 行）".to_owned(),
                },
                Entry {
                    rel: "src/d.java".to_owned(),
                    display: r"src\D.java".to_owned(),
                    local_rel: r"src\D.java".to_owned(),
                    server_rel: "src/d.java".to_owned(),
                    action: Action::SkipUnchanged,
                    ancestor: AncInfo::Base,
                    regions: Vec::new(),
                    local: finger(50, 1),
                    server: finger(50, 1),
                    write_local: None,
                    write_server: None,
                    delete_local: false,
                    delete_server: false,
                    observed_local: None,
                    observed_server: None,
                    note: String::new(),
                },
                Entry {
                    rel: "src/e.java".to_owned(),
                    display: r"src\E.java".to_owned(),
                    local_rel: r"src\E.java".to_owned(),
                    server_rel: "src/e.java".to_owned(),
                    action: Action::SkipBlacklisted,
                    ancestor: AncInfo::NotNeeded,
                    regions: Vec::new(),
                    local: finger(70, 2),
                    server: finger(80, 2),
                    write_local: None,
                    write_server: None,
                    delete_local: false,
                    delete_server: false,
                    observed_local: None,
                    observed_server: None,
                    note: String::new(),
                },
            ],
            local_root: std::path::PathBuf::from(r"D:\dev\hrp"),
            server_root: std::path::PathBuf::from(r"C:\wc\hrp"),
            checked: 5,
            unverified: 1,
            outside_whitelist: 2,
            blacklisted: 1,
            ..Default::default()
        }
    }

    /// 合并预览窗口：清单逐行按处置画出来、页脚按实报数，「已一致」只进计数不占行。
    /// 清单是 take 出来用完再放回去的，少放一次就是窗口每帧自己关掉；
    /// 「应用并备份」第一下只把按钮换成确认，绝不能已经起了写任务
    #[test]
    fn merge_window_lists_the_plan_and_asks_before_writing() {
        let _appdata = crate::testbed::lock_appdata();
        std::env::set_var(
            "APPDATA",
            std::env::temp_dir().join("repo_manager_merge_window_test"),
        );
        let ctx = egui::Context::default();
        ctx.set_theme(egui::ThemePreference::Dark);
        let mut app = crate::testbed::stub_app(false);
        app.merge = Some(crate::MergePreview {
            dir: 0,
            plan: Some(merge_fixture()),
            ..Default::default()
        });

        // 弹层第一帧只用于稳定形状，断言读第二帧
        merge_frame(&mut app, &ctx, Vec::new());
        let drawn = merge_frame(&mut app, &ctx, Vec::new());
        for label in [
            "冲突",
            "差异过多",
            "黑名单内",
            r"src\A.java",
            r"src\B.java",
            r"src\C.java",
            r"src\E.java",
            "r190",
            "本地 1 处",
            "本地 4–4 / 服务器 4–4",
            "写入",
            "合入服务器",
            "行级合并",
            "整文件覆盖",
            "新增拷入",
            "跳过",
            "冲突",
            "待人工",
            "两边都改",
            "不在范围",
            "已一致",
            "应用并备份",
        ] {
            assert!(has(&drawn, label), "窗口里应有「{label}」");
        }
        assert!(
            !has(&drawn, r"src\D.java"),
            "已一致的那份只进汇总计数，不该占文件清单的一行"
        );
        assert!(
            app.merge.is_some() && app.merge.as_ref().unwrap().plan.is_some(),
            "清单要在窗口里留住，不是每帧取出来就丢了"
        );

        let apply = click_of(&drawn, "应用并备份");
        merge_frame(
            &mut app,
            &ctx,
            vec![
                egui::Event::PointerMoved(apply.center()),
                egui::Event::PointerButton {
                    pos: apply.center(),
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
                egui::Event::PointerButton {
                    pos: apply.center(),
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
        assert!(
            !app.pool.is_writing(0),
            "第一下只该亮出确认，不能已经起了写任务"
        );
        assert!(app.merge.as_ref().unwrap().confirm, "第一下要进入待确认态");
        // 确认那颗按钮是下一帧才画出来的：点击这一帧画的还是「应用并备份」
        let drawn = merge_frame(&mut app, &ctx, Vec::new());
        assert!(
            has(&drawn, "确认写入 1 项"),
            "第二下的按钮要写明写入几项"
        );
    }

    /// 在 `rect` 中心走一遍「移过去 → 按下 → 抬起」，一帧内点完一颗按钮
    fn click(events: &mut Vec<egui::Event>, rect: egui::Rect) {
        events.push(egui::Event::PointerMoved(rect.center()));
        events.push(egui::Event::PointerButton {
            pos: rect.center(),
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: egui::Modifiers::NONE,
        });
        events.push(egui::Event::PointerButton {
            pos: rect.center(),
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        });
    }

    /// 最大化要的是「清单多看几十行」，不是把窗口拉大完事：多出来的那一截得落在清单视口上，
    /// 页脚跟着往下走、仍然贴在窗口下沿。页脚高度是上一帧实量的，所以比的是再画一帧之后的几何
    #[test]
    fn maximizing_the_merge_window_gives_the_extra_height_to_the_list() {
        let _appdata = crate::testbed::lock_appdata();
        std::env::set_var(
            "APPDATA",
            std::env::temp_dir().join("repo_manager_merge_zoom_test"),
        );
        let ctx = egui::Context::default();
        ctx.set_theme(egui::ThemePreference::Dark);
        let mut app = crate::testbed::stub_app(false);
        app.merge = Some(crate::MergePreview {
            dir: 0,
            plan: Some(merge_fixture()),
            ..Default::default()
        });
        let window = || {
            ctx.memory(|m| m.area_rect(egui::Id::new("merge")))
                .expect("窗口矩形")
        };
        merge_frame(&mut app, &ctx, Vec::new());
        let drawn = merge_frame(&mut app, &ctx, Vec::new());
        let normal = window();
        // 清单最后一行与页脚首行之间的空白 = 清单视口里没被行占满的那截。
        // 最大化时窗口顶边也一起上移，所以不能拿页脚的绝对高度说话
        // 清单卡片吃满窗口剩下的高度：最大化之后「标题到窗口下沿」这一截变长，
        // 多出来的高度就都进了清单视口（标题位置基本不动，所以比的是它到窗口底的距离）
        let slack = |drawn: &[(String, egui::Rect)]| {
            window().max.y - click_of(drawn, "文件清单（4）").max.y
        };
        let before = slack(&drawn);

        let mut events = Vec::new();
        click(&mut events, click_of(&drawn, "最大化"));
        merge_frame(&mut app, &ctx, events);
        let drawn = merge_frame(&mut app, &ctx, Vec::new());
        let max = window();
        assert!(has(&drawn, "还原大小"), "最大化之后那一枚要改口成「还原大小」");
        assert!(
            max.height() > normal.height() + 100.0,
            "窗口该铺满工作区：{normal:?} → {max:?}"
        );
        assert!(
            slack(&drawn) > before + 100.0,
            "多出来的高度该全进清单视口：标题到窗口底 {before} → {}",
            slack(&drawn)
        );
        assert!(
            has(&drawn, r"src\E.java"),
            "最大化之后清单视野更高，黑名单那份也该露出来"
        );

        let mut events = Vec::new();
        click(&mut events, click_of(&drawn, "还原大小"));
        merge_frame(&mut app, &ctx, events);
        // 还原只写一帧矩形，之后靠 egui 自己记住的位置：多跑一帧确认它没滑回去
        let drawn = merge_frame(&mut app, &ctx, Vec::new());
        let drawn = merge_frame(&mut app, &ctx, Vec::new());
        let back = window();
        let drift = (back.min - normal.min).length() + (back.max - normal.max).length();
        assert!(drift < 1.0, "还原要回最大化之前那个矩形：{normal:?} → {back:?}");
        assert!(
            has(&drawn, "我已确认无误，直接合并") && back.contains(click_of(&drawn, "我已确认无误，直接合并").center()),
            "还原后内容要跟着窗口一起回到原位"
        );
    }

    /// 「我已确认无误，直接合并」把确认写在字面上：一次点击就起写任务，不再亮第二颗确认按钮。
    /// 这份清单的两个根目录都指到临时区，落盘前的指纹重查必然对不上，后台那一趟一个字节也写不下去
    #[test]
    fn the_direct_merge_starts_writing_on_the_first_click() {
        let _appdata = crate::testbed::lock_appdata();
        std::env::set_var(
            "APPDATA",
            std::env::temp_dir().join("repo_manager_merge_direct_test"),
        );
        let ctx = egui::Context::default();
        ctx.set_theme(egui::ThemePreference::Dark);
        let mut app = crate::testbed::stub_app(false);
        let scratch = std::env::temp_dir().join("repo_manager_merge_direct_scratch");
        let _ = std::fs::remove_dir_all(&scratch);
        let mut plan = merge_fixture();
        plan.local_root = scratch.join("local");
        plan.server_root = scratch.join("server");
        app.merge = Some(crate::MergePreview {
            dir: 0,
            plan: Some(plan),
            ..Default::default()
        });
        merge_frame(&mut app, &ctx, Vec::new());
        let drawn = merge_frame(&mut app, &ctx, Vec::new());
        let mut events = Vec::new();
        click(&mut events, click_of(&drawn, "我已确认无误，直接合并"));
        merge_frame(&mut app, &ctx, events);
        assert!(
            app.pool.has(Kind::ApplyMerge, 0),
            "这一颗该一次点到位，直接起写任务"
        );
        assert!(
            !app.merge.as_ref().unwrap().confirm,
            "不备份这条路没有待确认态"
        );
        // 等后台那一趟真跑完再验盘，断言才不是蒙对的
        for _ in 0..200 {
            if !app.pool.poll().done.is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(
            !scratch.join("local").exists() && !scratch.join("server").exists(),
            "预览之后的样子对不上，一个字节都不该落下去"
        );
        let _ = std::fs::remove_dir_all(&scratch);
    }

    /// 「清理备份」和写入一样分两步：第一下只把确认亮出来，条数与体积都按已完成的几次算，
    /// 写到一半断掉的那次不进确认；第二下才真的起删除任务
    #[test]
    fn cleaning_backups_arms_the_confirm_before_deleting() {
        let _appdata = crate::testbed::lock_appdata();
        std::env::set_var(
            "APPDATA",
            std::env::temp_dir().join("repo_manager_merge_clean_test"),
        );
        let ctx = egui::Context::default();
        ctx.set_theme(egui::ThemePreference::Dark);
        let mut app = crate::testbed::stub_app(false);
        let backup = |name: &str, bytes: u64, unfinished: bool| crate::merge::BackupInfo {
            dir: std::env::temp_dir().join(name),
            name: name.to_owned(),
            files: 1,
            bytes,
            unfinished,
        };
        app.merge = Some(crate::MergePreview {
            dir: 0,
            plan: Some(merge_fixture()),
            backups: vec![
                backup("merge-backup-20260102-120000", 4096, false),
                backup("merge-backup-20260101-120000", 5120, true),
            ],
            ..Default::default()
        });
        merge_frame(&mut app, &ctx, Vec::new());
        let drawn = merge_frame(&mut app, &ctx, Vec::new());
        let mut events = Vec::new();
        click(&mut events, click_of(&drawn, "清理备份"));
        merge_frame(&mut app, &ctx, events);
        assert!(
            !app.pool.has(Kind::CleanBackup, 0),
            "第一下只该亮确认，不能已经删起来了"
        );
        assert!(app.merge.as_ref().unwrap().wipe, "第一下要进入待确认态");
        let drawn = merge_frame(&mut app, &ctx, Vec::new());
        assert!(
            has(&drawn, "删掉这 1 次备份（约 4 KB）？"),
            "确认只算已完成的那次，未完成那条是还原凭据"
        );
        let mut events = Vec::new();
        click(&mut events, click_of(&drawn, "确认清理"));
        merge_frame(&mut app, &ctx, events);
        assert!(
            app.pool.has(Kind::CleanBackup, 0),
            "第二下才起删除任务"
        );
        assert!(!app.merge.as_ref().unwrap().wipe, "确认点完就把确认行收掉");
    }
}
