//! 主页：目录列表（详细整行 / 简约正方形卡片）、样式切换器、行内编辑器，
//! 以及卡片右下角那个收纳全部操作的「+」菜单。

use egui::{Align, Color32, Frame, Layout, RichText, ScrollArea, TextEdit, Ui, Vec2};
use egui::Key;

use crate::{
    field_margin, ink, pale, pane_fill, pane_hover, pane_stroke, quiet, CommitRules, DirView,
    Maintain, RepoApp, UploadAll,
};
use crate::ui::AlignedRow;

/// 简约形态下宽行卡片的尺寸（逻辑点）：左列放名称 / 标记 / 版本，右侧竖排计数胶囊、
/// 最右一枚圆形「+」。264×92 下两列并排仍能放进 1400 宽的窗口，名称放得下七八个汉字
const CARD_W: f32 = 264.0;
const CARD_H: f32 = 92.0;
/// 计数胶囊列的固定宽度：要装下「待提交 999+」这种最宽的组合，再留一点余量
const PILL_W: f32 = 74.0;

/// 拖动落点。分组标题条整条通栏，落上去 = 并入那一组；目录行 / 卡片各切成两半，
/// 落在前半或后半 = 插到它之前或之后，分组也跟着那一行走——
/// 所以拖到未分组的行上，就等于把这行从分组里拉出来
#[derive(Clone, Debug, PartialEq)]
pub enum DropZone {
    Header(String),
    Row(usize, Edge),
}

/// 落点在某一行的哪一侧：详细列表按上、下切，简约卡片按左、右切
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edge {
    Before,
    After,
}

/// 把刚画出来的一块矩形登记成两块落点。先裁进可视区：滚出视野的那一行
/// 坐标还留在列表顶上，不裁就会在屏幕另一头接住落点
fn half_zones(
    rect: egui::Rect,
    clip: egui::Rect,
    index: usize,
    horizontal: bool,
) -> Vec<(DropZone, egui::Rect)> {
    let rect = rect.intersect(clip);
    if rect.is_negative() {
        return Vec::new();
    }
    let (before, after) = if horizontal {
        let x = rect.center().x;
        (
            egui::Rect::from_min_max(rect.min, egui::pos2(x, rect.max.y)),
            egui::Rect::from_min_max(egui::pos2(x, rect.min.y), rect.max),
        )
    } else {
        let y = rect.center().y;
        (
            egui::Rect::from_min_max(rect.min, egui::pos2(rect.max.x, y)),
            egui::Rect::from_min_max(egui::pos2(rect.min.x, y), rect.max),
        )
    };
    vec![
        (DropZone::Row(index, Edge::Before), before),
        (DropZone::Row(index, Edge::After), after),
    ]
}

/// 指针落在上一帧登记的哪块区域上。属于被拖那一行自己的半区跳过：原地放下等于没动
fn hit_zone(zones: &[(DropZone, egui::Rect)], from: usize, pos: egui::Pos2) -> Option<DropZone> {
    zones.iter().find_map(|(zone, rect)| {
        if !rect.contains(pos) {
            return None;
        }
        match zone {
            DropZone::Row(index, _) if *index == from => None,
            other => Some(other.clone()),
        }
    })
}

/// 这一点被浮层（弹窗 / 弹层 / 菜单）盖住了没有。拖动只看几何——按下点落在哪一行的
/// 矩形里，于是压在列表上的弹窗也当成了列表本身；`layer_id_at` 只认可以交互的 Area，
/// 主页列表在背景层，所以那一点的顶层不是背景层就是按到了浮层身上。
fn covered_by_overlay(ctx: &egui::Context, pos: egui::Pos2) -> bool {
    ctx
        .layer_id_at(pos)
        .is_some_and(|layer| layer.order != egui::Order::Background)
}

/// 计数标记的字号与高度：10.9 号字的行高 15 + 上下内边距 3/4（去描边后 Frame 少画 2，
/// 非对称补足 20）；圆角由它取半向下推出来（胶囊）
const MARK_FONT: f32 = 10.9;
const MARK_H: f32 = 20.0;

/// 计数胶囊的配色：(底色, 文字色)。
/// 有待办时按语义色出一枚淡底深字胶囊——浅色主题走设计稿的粉彩底、压暗的字；
/// 深色主题用半透明色底，字直接用原色。没待办就是一枚不抢眼的中性灰。
pub(crate) fn pill_colors(ui: &Ui, hue: Color32, active: bool) -> (Color32, Color32) {
    if active {
        if ui.visuals().dark_mode {
            (tint(hue, 44), hue)
        } else {
            (pale(hue, 0.16), ink(ui, hue))
        }
    } else if ui.visuals().dark_mode {
        (Color32::from_white_alpha(16), Color32::from_gray(150))
    } else {
        (Color32::from_rgb(243, 245, 250), Color32::from_gray(138))
    }
}

/// 只换 alpha、不动 RGB：卡片上计数标记的底色 / 描边要的是「同一颜色淡一点」，
/// 而 gamma_multiply 会连 alpha 一起乘、把颜色本身也调淡。
fn tint(color: Color32, alpha: u8) -> Color32 {
    let [r, g, b, _] = color.to_array();
    Color32::from_rgba_unmultiplied(r, g, b, alpha)
}

/// 计数标记上的数字：四位数起换「999+」，不然窄标记会被撑歪
fn capped(count: usize) -> String {
    if count > 999 {
        "999+".to_owned()
    } else {
        count.to_string()
    }
}

/// 标记胶囊的默认颜色：配置里没存颜色或颜色读不出来时用它
const DEFAULT_TAG: Color32 = Color32::from_rgb(90, 160, 240);

/// 配置里的 "#RRGGBB" → 颜色；空或解析失败回退默认蓝
pub fn parse_tag_color(hex: &str) -> Color32 {
    let text = hex.trim().trim_start_matches('#');
    if text.len() == 6 {
        if let Ok(value) = u32::from_str_radix(text, 16) {
            return Color32::from_rgb((value >> 16) as u8, (value >> 8) as u8, value as u8);
        }
    }
    DEFAULT_TAG
}

/// 目录标记胶囊的颜色：按配置里的 tag_color 解析，读不出来就是默认蓝
pub fn tag_color(dir: &crate::config::DirConfig) -> Color32 {
    parse_tag_color(&dir.tag_color)
}

/// 取色器要的 [r, g, b]（0..255，同 egui 的 color_edit_button_srgb）
pub fn tag_color_rgb(hex: &str) -> [u8; 3] {
    let [r, g, b, _] = parse_tag_color(hex).to_array();
    [r, g, b]
}

/// 取色器吐出的 [r, g, b] 写回配置里的 "#RRGGBB"
pub fn tag_color_hex(rgb: [u8; 3]) -> String {
    format!("#{:02X}{:02X}{:02X}", rgb[0], rgb[1], rgb[2])
}

/// 详细行右下角的标签色块：实心填上标签色，文字按底色亮度取近黑 / 近白，
/// 深浅两套主题都不用另调一套
fn tag_block(ui: &mut Ui, text: &str, color: Color32) {
    let [r, g, b, _] = color.to_array();
    let text_color = if 0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32 > 150.0 {
        Color32::from_rgb(26, 20, 4)
    } else {
        Color32::from_rgb(250, 252, 250)
    };
    Frame::new()
        .fill(color)
        .corner_radius(4.0)
        .inner_margin(egui::Margin::symmetric(6, 2))
        .show(ui, |ui| {
            ui.label(RichText::new(text).size(11.5).color(text_color));
        });
}

impl RepoApp {
    // ------------------------------------------------------------ 主页面

    fn dir_row(&mut self, ui: &mut Ui, index: usize) {
        let dir = self.cfg.dirs[index].clone();
        let view = self.dirs.get(index).cloned().unwrap_or_default();
        let busy = self.pool.is_busy(index);
        let busy_label = self.pool.busy_label(index);
        let label = self.dir_label(index);
        let is_selected = self.selected == Some(index);

        let lamp = ink(
            ui,
            match view.remote {
                Some(true) => crate::OK,
                Some(false) => crate::ERR,
                None if busy => crate::AMBER,
                None => Color32::from_gray(120),
            },
        );
        // 悬停反馈：读上一帧这行的命中状态（悬停本来就是逐帧跟着指针走的，滞后一帧看不出）
        let hovered = ui
            .ctx()
            .read_response(egui::Id::new(("dir_row", index)))
            .is_some_and(|hit| hit.hovered());
        // 手里正拖着这一行：描边换成主色、底色抬到悬停那一档，看得出它被「拿起来了」
        let dragging = self.drag == Some(index);
        let frame = Frame::new()
            .inner_margin(7.0)
            // 分了组的行整块往右让一截：底色条跟着一起缩（外边距），不是在通栏行里加大内边距
            .outer_margin(egui::Margin {
                left: if dir.group.trim().is_empty() { 0 } else { 14 },
                ..Default::default()
            })
            .corner_radius(8.0)
            .stroke(egui::Stroke::new(
                if dragging { 1.6 } else { 1.0 },
                // 比全局 pane_stroke 深一小档：纯白行底上浅色描边有点看不清，但不做多
                if dragging {
                    self.accent_ink()
                } else if ui.visuals().dark_mode {
                    Color32::from_rgba_unmultiplied(255, 255, 255, 54)
                } else {
                    Color32::from_rgb(216, 220, 230)
                },
            ))
            .fill(if is_selected {
                ui.visuals().selection.bg_fill
            } else if hovered || dragging {
                pane_hover(ui)
            } else if ui.visuals().dark_mode {
                pane_fill(ui)
            } else {
                // 浅色主题跟简约卡片一样用白底，两种样式看起来是一套的
                Color32::WHITE
            });
        frame.show(ui, |ui| {
            // 行内文字默认可选中，会抢占点击；关掉后整行空白处才能选中本行
            ui.style_mut().interaction.selectable_labels = false;
            ui.aligned_row(|ui| {
                ui.label(RichText::new("●").size(15.0).color(lamp)).on_hover_text({
                    let mut tip = view.remote_msg.clone();
                    if tip.is_empty() {
                        tip = "尚未检测".to_owned();
                    }
                    if !view.checked_at.is_empty() {
                        tip.push_str(&format!("（{}）", view.checked_at));
                    }
                    tip
                });
                if self.edit_label == Some(index) {
                    self.alias_editor(ui, index);
                } else {
                    ui.label(RichText::new(&label).strong()).on_hover_text(
                        "别名可随时改：更多 → 修改别名（留空则显示文件夹名）",
                    );
                }
                ui.label(
                    RichText::new(&dir.path)
                        .size(12.5)
                        .color(ui.visuals().weak_text_color()),
                );
                // 分组编辑器画在路径后面（「更多 → 设置分组」打开）
                if self.edit_group == Some(index) {
                    self.group_editor(ui, index);
                }
                // 目录标记挪到了第二行右下角的色块里（编辑也在那里），主行不再重复显示
                let changed_before = view.changed.is_some();
                if let Some(changed) = view.changed {
                    if changed > 0 {
                        ui.label(
                            RichText::new(format!("待提交 {changed} 项"))
                                .size(12.0)
                                .color(ink(ui, crate::AMBER)),
                        )
                        .on_hover_text(
                            "口径与「全部上传」一致：未版本化(?)、已丢失(!) 也算在内——\n\
                             上传时它们会先加入版本控制或标记删除，再和改动一并提交，无需手动标记\n\
                             本目录已屏蔽的条目不算在内",
                        );
                    } else {
                        ui.label(
                            RichText::new("无待提交项")
                                .size(12.0)
                                .color(ink(ui, crate::OK)),
                        );
                    }
                }
                let conflict_before = view.conflicts.filter(|count| *count > 0).is_some();
                if let Some(blocked) = view.conflicts.filter(|count| *count > 0) {
                    // 冲突条目提交不了，所以不算进上面的「待提交」；不单独标出来，
                    // 用户就只能进了提交页才知道这个目录卡住了
                    let hit = ui.add(
                        egui::Label::new(
                            RichText::new(format!("冲突 {blocked} 项"))
                                .size(12.0)
                                .strong()
                                .underline()
                                .color(ink(ui, Color32::from_rgb(255, 80, 160))),
                        )
                        .sense(egui::Sense::click()),
                    );
                    if hit.clicked() {
                        self.open_commit(index);
                    }
                    hit.on_hover_text(
                        "冲突、不完整这类必须人工处理的条目，解决之前这个目录提交不上去。\n\
                         点击打开该目录的提交页。",
                    );
                }
                // 本地端（「绑定对比对象」里那个不受版本控制的目录）与这个工作副本的逐文件
                // 差异，界面上叫「本地差异」，一条都没有就叫「本地无差异」；
                // 点一下选中本目录并打开 Beyond Compare。
                if let Some(summary) = view.local_diff.as_ref() {
                    if changed_before || conflict_before {
                        ui.label(RichText::new("·").size(12.0).weak());
                    }
                    let color = if summary.is_clean() {
                        ink(ui, crate::OK)
                    } else {
                        ink(ui, crate::AMBER)
                    };
                    let tip = self.local_diff_tip(index, summary, self.pool.comparing(index));
                    let hit = ui
                        .add(
                            egui::Label::new(RichText::new(summary.label()).size(12.0).color(color))
                                .sense(egui::Sense::click()),
                        )
                        .on_hover_text(tip);
                    if hit.clicked() {
                        self.selected = Some(index);
                        self.open_bcompare_matched();
                    }
                }
                if busy {
                    ui.spinner();
                    if let Some(text) = busy_label {
                        ui.label(RichText::new(text).weak().size(11.5));
                    }
                }
                // 六个按钮必须排在同一个 horizontal 里：不同按钮的文字混排高度略有差异
                // （例如含 ↑↓ 箭头时行高比纯中文略高），一旦把「移除 / 更多」和它们分成
                // 两组并列，两组就会按各自高度居中而错开约半个像素（实测 0.5px）。
                // 同一个 horizontal 内由同一个 Ui 摆放，中心线才完全一致。
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {

                    ui.aligned_row(|ui| {
                        let confirm = self.confirm_remove == Some(index);
                        let mut button = egui::Button::new(if confirm {
                            RichText::new("确认移除?")
                                .strong()
                                .color(crate::DANGER_TEXT)
                        } else {
                            RichText::new("移除")
                        });
                        if confirm {
                            button = button.fill(crate::DANGER_FILL);
                        }
                        if quiet(ui, |ui| ui.add_enabled(!busy, button))
                            .on_hover_text("仅从列表移除，不删除磁盘文件")
                            .clicked()
                        {
                            if confirm {
                                self.remove_directory(index);
                            } else {
                                self.confirm_remove = Some(index);
                                self.hint("再次点击「确认移除」可把该目录从列表移除（不会删除文件）");
                            }
                        }
                        quiet(ui, |ui| {
                            ui.menu_button("更多", |ui| self.more_menu(ui, index, busy, &view))
                        });
                        if quiet(ui, |ui| ui.button("打开目录")).clicked() {
                            self.open_folder(index);
                        }
                        if quiet(ui, |ui| ui.button("历史")).clicked() {
                            self.selected = Some(index);
                            self.open_history(index);
                        }
                        if quiet(ui, |ui| ui.button("上传")).clicked() {
                            self.selected = Some(index);
                            self.open_commit(index);
                        }
                        if quiet(ui, |ui| ui.button("更新")).clicked() {
                            self.selected = Some(index);
                            self.confirm_remove = None;
                            self.spawn_update(index);
                        }
                    });
                });
            });
            ui.horizontal(|ui| {
                match &view.info {
                    Some(info) => {
                        // Git 目录在地址前面带上当前分支（SVN 没有分支概念，恒为空不画）
                        if !info.branch.is_empty() {
                            ui.label(
                                RichText::new(&info.branch)
                                    .size(12.0)
                                    .monospace()
                                    .strong(),
                            )
                            .on_hover_text("当前分支");
                        }
                        ui.label(RichText::new(&info.url).size(12.0).weak());
                        // 「本地」按内容算：没有待更新项就说明工作副本已经包含 HEAD 的全部
                        // 改动，根目录的版本号因为混合版本停在旧值，不代表内容还是旧的
                        let up_to_date =
                            view.out_of_date == Some(0) && !view.remote_rev.is_empty();
                        let local_rev = if up_to_date {
                            view.remote_rev.clone()
                        } else {
                            info.revision.clone()
                        };
                        ui.label(
                            RichText::new(format!(
                                "本地 {}",
                                crate::repo::fmt_rev(view.kind, &local_rev)
                            ))
                            .size(12.0)
                            .weak(),
                        );
                        if up_to_date {
                            ui.label(
                                RichText::new("已是最新")
                                    .size(12.0)
                                    .color(ink(ui, crate::OK)),
                            );
                        } else if let Some(pending) =
                            view.out_of_date.filter(|count| *count > 0)
                        {
                            // git 的可更新数是提交个数，svn 是待更新条目数，单位跟着后端走
                            let unit = if view.kind == crate::repo::RepoKind::Git {
                                "个提交"
                            } else {
                                "项"
                            };
                            ui.label(
                                RichText::new(format!(
                                    "远端 {}（可更新 {pending} {unit}）",
                                    crate::repo::fmt_rev(view.kind, &view.remote_rev)
                                ))
                                .size(12.0)
                                .color(ink(ui, crate::AMBER)),
                            );
                        }
                        // 优先显示服务器上的最后提交（检测时从仓库 URL 读取）；
                        // 服务器不可达时才退回本地 BASE 的信息，并标注来源
                        let from_server = !view.last_rev.is_empty() || !view.last_author.is_empty();
                        let (last_rev, last_author, last_date) = if from_server {
                            (view.last_rev.clone(), view.last_author.clone(), view.last_date.clone())
                        } else {
                            (info.revision.clone(), info.last_author.clone(), info.last_date.clone())
                        };
                        if !last_date.is_empty() {
                            ui.label(
                                RichText::new(format!(
                                    "{}最后提交 {} · {last_author} · {last_date}",
                                    if from_server { "服务器" } else { "本地" },
                                    crate::repo::fmt_rev(view.kind, &last_rev)
                                ))
                                .size(12.0)
                                .weak(),
                            );
                        }
                        if !info.repos_root.is_empty() {
                            if let Some(host) = info.repos_root.strip_prefix("https://").or(info.repos_root.strip_prefix("http://")) {
                                ui.label(RichText::new(format!("仓库 {}", host.split('/').next().unwrap_or(""))).size(12.0).weak());
                            }
                        }
                    }
                    None => {
                        ui.label(
                            RichText::new(if view.checked_at.is_empty() { "未检测" } else { "非工作副本或无法读取" })
                                .size(12.0)
                                .color(ink(ui, crate::ERR)),
                        );
                    }
                }
                // 行尾右下角：目录标签的色块（实心填标签色）；正在编辑时这一位换成编辑行。
                // 没标签也不编辑时这里什么都不画，不给行白白加高
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if self.edit_tag == Some(index) {
                        self.tag_editor(ui, index);
                    } else if !dir.tag.is_empty() {
                        tag_block(ui, &dir.tag, tag_color(&dir));
                    }
                });
            });
        });
    }

    /// 「本地差异」那枚标记的悬停说明：差异多少、三类各多少、跟哪个目录比的、
    /// 什么时候算的。详细行那枚和卡片上那枚共用一份，免得改了这处漏了那处。
    fn local_diff_tip(&self, index: usize, s: &crate::localdiff::Summary, busy: bool) -> String {
        let bound = self.cfg.dirs.get(index).map(|d| d.bc_target.clone()).unwrap_or_default();
        let checked = self.dirs.get(index).map(|v| v.local_checked_at.clone()).unwrap_or_default();
        let mut tip = format!("{}\n", s.label());
        tip.push_str(&format!(
            "内容不同 {} · 只在本地开发侧 {} · 只在服务器端 {} · 一致 {}\n",
            s.differing,
            s.only_local,
            s.only_server,
            s.same
        ));
        if s.unverified > 0 {
            tip.push_str(&format!("未核对内容 {}\n", s.unverified));
        }
        if self.cfg.dirs.get(index).map(|d| d.bc_filter.trim()).unwrap_or_default().is_empty() {
            tip.push_str("未设对比筛选条件（更多 → 差异过滤与提交屏蔽）\n");
        }
        if !s.samples.is_empty() {
            tip.push_str("点击打开 Beyond Compare：\n");
            for rel in s.samples.iter().take(8) {
                tip.push_str(&format!("  {rel}\n"));
            }
        }
        if busy {
            tip.push_str("正在重新比对，显示的是上一次结果。");
        } else {
            tip.push_str(&format!("本地开发侧：{bound}{}", if checked.is_empty() { String::new() } else { format!(" · {checked}") }));
        }
        tip
    }

    // ------------------------------------------------------------ 简约样式（正方形卡片）

    /// 「更多」菜单的内容。详细模式的目录行直接弹它，简约模式的「+」菜单把它当作二级菜单，
    /// 两边共用一份，免得改了这处漏了那处
    fn more_menu(&mut self, ui: &mut Ui, index: usize, busy: bool, view: &DirView) {
        let git = view.kind == crate::repo::RepoKind::Git;
        if ui.button("修改别名").clicked() {
            self.edit_label = Some(index);
            self.label_buf = self.cfg.dirs[index].label.clone();
            self.label_focus = true;
            ui.close();
        }
        if ui
            .button(if self.cfg.dirs[index].group.is_empty() {
                "设置分组"
            } else {
                "修改分组"
            })
            .on_hover_text("给目录归个堆：同名分组的目录在列表里共用一个可折叠的标题（留空取消分组）")
            .clicked()
        {
            self.edit_group = Some(index);
            self.group_buf = self.cfg.dirs[index].group.clone();
            self.group_focus = true;
            ui.close();
        }
        if ui
            .button(if self.cfg.dirs[index].tag.is_empty() {
                "设置标记"
            } else {
                "修改标记"
            })
            .on_hover_text("给目录挂一个自己认得的记号：一段文字加一个自选颜色（文字留空即清除标记）")
            .clicked()
        {
            self.edit_tag = Some(index);
            self.tag_buf = self.cfg.dirs[index].tag.clone();
            self.tag_focus = true;
            ui.close();
        }
        // 更新通知订阅：开启后自动检测发现服务器有新版本时弹一次系统通知
        let subscribed = self.cfg.dirs[index].notify;
        if ui
            .button(if subscribed { "关闭更新通知" } else { "订阅更新通知" })
            .on_hover_text(
                "开启后，检测到该目录在服务器上有新版本时发送一次 Windows 系统通知\
                 （含最新版本、最新提交人与未更新项数；同一版本只通知一次，多个目录合并为一条）",
            )
            .clicked()
        {
            self.cfg.dirs[index].notify = !subscribed;
            // 重新开启时清掉已通知记录：当前积压的更新会作为「新版本」通知一次
            if !subscribed {
                self.cfg.dirs[index].notified_rev = String::new();
            }
            self.persist();
            self.hint(if subscribed {
                format!("已关闭「{}」的更新通知", self.dir_label(index))
            } else {
                format!("已开启「{}」的更新通知", self.dir_label(index))
            });
            ui.close();
        }
        // 仓库地址 / 清理 / 解决冲突是 SVN 工作副本的概念，Git 目录没有对应操作，直接不出现
        if !git
            && ui
                .button("修改仓库地址")
                .on_hover_text("服务器换地址 / 换端口后，把工作副本指到新的仓库地址，只改元数据不动文件")
                .clicked()
        {
            self.open_relocate(index);
            ui.close();
        }
        if ui
            .button(if self.cfg.dirs[index].bc_target.is_empty() {
                "⚖ 绑定对比对象"
            } else {
                "⚖ 重新绑定对比对象"
            })
            .on_hover_text("给这个目录绑定 Beyond Compare 对比的另一侧（例如服务器上的 *_server 副本），打开时就不靠文件夹名去猜记录")
            .clicked()
        {
            if let Some(target) = rfd::FileDialog::new()
                .set_title("选择对比对象目录")
                .pick_folder()
            {
                self.cfg.dirs[index].bc_target = target.display().to_string();
                self.persist();
                self.hint(format!("已绑定对比对象：{}", self.cfg.dirs[index].bc_target));
                // 立刻起一次比对，免得新绑定要等下一轮自动刷新（默认 120 秒）才出数
                self.spawn_local_diff(index, false);
                ui.close();
            }
        }
        if !self.cfg.dirs[index].bc_target.is_empty() {
            ui.label(
                RichText::new(format!("已绑定：{}", self.cfg.dirs[index].bc_target))
                    .weak()
                    .size(11.5),
            );
            if ui.button("✖ 解除绑定").clicked() {
                self.cfg.dirs[index].bc_target.clear();
                self.persist();
                self.hint("已解除绑定，Beyond Compare 退回按对比记录匹配");
                // 顺带把挂着的本地端差异数清掉
                self.spawn_local_diff(index, false);
                ui.close();
            }
        }
        ui.separator();
        // 四套规则装在同一扇窗的两页标签里：第一页是差异比对的名称筛选（只管「本地差异」那个数）
        // 与屏蔽可提交项（决定哪些改动不进提交清单），第二页划定双向合并涉及哪些文件。
        // 窗口是独立浮层，不受菜单点哪儿都收起的限制
        if ui
            .button("差异过滤与提交屏蔽")
            .on_hover_text(
                "本目录的四套规则在此一并编辑，分作两页、均一行一条、保存于配置中长期生效：第一页是\
                 差异比对的名称筛选（仅影响「本地差异」计数）与屏蔽可提交项（命中的条目不进入提交\
                 清单，提交页中亦不显示）；第二页划定双向合并涉及哪些文件",
            )
            .clicked()
        {
            let bc = self.cfg.dirs[index].bc_filter.clone();
            self.commit_rules = Some(CommitRules::new(index, &bc));
            ui.close();
        }
        // 本地差异和 Beyond Compare 显示对不上号（本软件报差异、BC 看不到）时，
        // 让用户把判定展开成报告文件发回来：占用、行尾、编码这些在别人机器上的现象才带得出来
        if !self.cfg.dirs[index].bc_target.is_empty()
            && ui
                .button("导出本地差异报告")
                .on_hover_text(
                    "把「本地差异」的每一项判定展开成文字报告：两边的大小和修改时间、判成不同的原因、\
                     第一个差异字节、差在哪几行。BC 看不到差异而本软件报了差异时，导出这个文件发给开发者排查。",
                )
                .clicked()
        {
            let stamp = chrono::Local::now().format("%Y%m%d_%H%M%S");
            if let Some(path) = rfd::FileDialog::new()
                .set_title("保存本地差异诊断报告")
                .set_file_name(format!("本地差异报告_{stamp}.txt"))
                .save_file()
            {
                self.export_local_diff_report(index, path);
            }
            ui.close();
        }
        ui.separator();
        ui.add_enabled_ui(!busy, |ui| {
            if git {
                return;
            }
            if ui.button(Maintain::Cleanup.label()).clicked() {
                self.spawn_maintain(index, Maintain::Cleanup);
                ui.close();
            }
            if ui
                .button("解决冲突")
                .on_hover_text("列出该目录里必须先人工处理的条目，逐条挑收场策略（我的 / 服务器的 / 丢弃改动）")
                .clicked()
            {
                self.open_resolve(index);
                ui.close();
            }
            // 双向合并是可安装插件：没装就不在「更多」里露头
            if self.plugin_installed(crate::plugins::PluginId::Merge) {
                // 两侧一致时也允许预览，以建立后续修改与删除判定所需的同步状态。
                let bound = !self.cfg.dirs[index].bc_target.trim().is_empty();
                let clear = view.blocked.is_empty();
                let writing = self.pool.is_writing(index);
                let tip = match (bound, clear, writing) {
                    (false, ..) => "先在「更多 → 绑定对比对象」里指定本地开发侧的目录".to_owned(),
                    (true, false, _) => "该目录有未处理的条目，先在「解决冲突」里收干净再合并".to_owned(),
                    (true, true, true) => "该目录还有写操作在跑，等这一趟结束再合并".to_owned(),
                    (true, true, false) =>
                        "核对两侧文件并预览合并；已一致的文件会记住同步状态，供下次判断修改和删除".to_owned(),
                };
                if ui
                    .add_enabled(
                        bound && clear && !writing,
                        egui::Button::new(RichText::new("双向合并（预览）").strong()),
                    )
                    .on_hover_text(tip)
                    .clicked()
                {
                    self.open_merge(index);
                    ui.close();
                }
            }
        });
        if ui.button("▲ 上移").clicked() {
            self.move_directory(index, -1);
            ui.close();
        }
        if ui.button("▼ 下移").clicked() {
            self.move_directory(index, 1);
            ui.close();
        }
        if ui.button("复制仓库 URL").clicked() {
            match view.info.clone() {
                Some(info) => {
                    ui.ctx().copy_text(info.url);
                    self.hint("已复制仓库地址到剪贴板");
                }
                None => self.hint("尚未取得仓库地址，请先刷新"),
            }
            ui.close();
        }
    }

    /// 别名输入框：详细模式画在目录行的名称位置，简约模式画在列表上方那一行
    fn alias_editor(&mut self, ui: &mut Ui, index: usize) {
        let field = ui.add(
            TextEdit::singleline(&mut self.label_buf)
                .id_salt(("alias", index))
                .desired_width(240.0)
                .margin(field_margin(ui))
                .hint_text("别名，留空则显示文件夹名"),
        );
        if self.label_focus {
            field.request_focus();
            self.label_focus = false;
        }
        let enter = field.has_focus() && ui.input(|i| i.key_pressed(Key::Enter));
        let esc = field.has_focus() && ui.input(|i| i.key_pressed(Key::Escape));
        if esc {
            self.edit_label = None;
            self.hint("已取消修改别名");
        } else if enter || field.lost_focus() {
            let text = self.label_buf.trim().to_owned();
            self.edit_label = None;
            if self.cfg.dirs[index].label != text {
                self.cfg.dirs[index].label.clone_from(&text);
                self.persist();
                self.hint(if text.is_empty() {
                    "已清除别名，列表改为显示文件夹名".to_owned()
                } else {
                    format!("别名已改为：{text}")
                });
            }
        }
    }

    /// 配置里各目录在用的分组名：去重、排序，排除 exclude（自己当前的那份，点了也是白点）
    pub(crate) fn existing_groups(&self, exclude: &str) -> Vec<String> {
        let mut names: Vec<String> = self
            .cfg
            .dirs
            .iter()
            .map(|d| d.group.trim().to_owned())
            .filter(|g| !g.is_empty() && g != exclude)
            .collect();
        names.sort();
        names.dedup();
        names
    }

    /// 配置里各目录在用的标记：名字 + 首次出现时的颜色（套用旧标记时颜色一并沿用），
    /// 去重、按名字排序，排除 exclude
    pub(crate) fn existing_tags(&self, exclude: &str) -> Vec<(String, String)> {
        let mut tags: Vec<(String, String)> = Vec::new();
        for dir in &self.cfg.dirs {
            let name = dir.tag.trim();
            if name.is_empty() || name == exclude {
                continue;
            }
            if !tags.iter().any(|(seen, _)| seen == name) {
                tags.push((name.to_owned(), dir.tag_color.clone()));
            }
        }
        tags.sort_by(|a, b| a.0.cmp(&b.0));
        tags
    }

    /// 分组输入框：详细模式画在目录行的路径后面，简约模式画在列表上方那一行。
    /// Enter 或点别处提交，Esc 取消，留空取消分组（交互同别名编辑）。
    /// 编辑期间输入框下方挂着在用的分组名，点一下直接套上，不必每次手敲
    fn group_editor(&mut self, ui: &mut Ui, index: usize) {
        let field = ui.add(
            TextEdit::singleline(&mut self.group_buf)
                .id_salt(("dir_group", index))
                .desired_width(160.0)
                .margin(field_margin(ui))
                .hint_text("分组，留空取消分组"),
        );
        if self.group_focus {
            field.request_focus();
            self.group_focus = false;
        }
        let enter = field.has_focus() && ui.input(|i| i.key_pressed(Key::Enter));
        let esc = field.has_focus() && ui.input(|i| i.key_pressed(Key::Escape));
        if esc {
            self.edit_group = None;
            self.hint("已取消修改分组");
        } else if enter || field.lost_focus() {
            let text = self.group_buf.trim().to_owned();
            self.edit_group = None;
            if self.cfg.dirs[index].group != text {
                self.cfg.dirs[index].group.clone_from(&text);
                self.persist();
                let label = self.dir_label(index);
                self.hint(if text.is_empty() {
                    format!("已取消 {label} 的分组")
                } else {
                    format!("{label} 已归入分组：{text}")
                });
            }
        }
        // 在用分组下拉：点一下直接套上。编辑器函数只在编辑期间被调，这里不用再查状态；
        // 点下拉项会先让输入框失焦（按当前内容空提交一次，预填时等于没改），随即套上新分组
        let names = self.existing_groups(self.cfg.dirs[index].group.trim());
        if !names.is_empty() {
            egui::Popup::from_response(&field)
                .open(true)
                .gap(2.0)
                .show(|ui| {
                    ui.set_min_width(140.0);
                    ScrollArea::vertical()
                        .max_height(160.0)
                        .show(ui, |ui| {
                            ui.spacing_mut().item_spacing.y = 2.0;
                            for name in &names {
                                if ui
                                    .add(egui::Button::new(RichText::new(name.clone())).small())
                                    .clicked()
                                {
                                    self.apply_group(index, name);
                                }
                            }
                        });
                });
        }
    }

    /// 套用在用分组：写回并关闭编辑行，口径与手输提交一致
    fn apply_group(&mut self, index: usize, text: &str) {
        self.group_buf = text.to_owned();
        self.edit_group = None;
        if self.cfg.dirs[index].group != text {
            self.cfg.dirs[index].group = text.to_owned();
            self.persist();
        }
        let label = self.dir_label(index);
        self.hint(format!("{label} 已归入分组：{text}"));
    }

    /// 标记编辑行：一段文字 + 一枚 RGB 取色按钮（egui 自带的取色弹层：RGB / HSV 滑块与
    /// hex 输入）。位置同分组编辑器；文字清空时把颜色一起清掉，下次设置标记从头选
    fn tag_editor(&mut self, ui: &mut Ui, index: usize) {
        ui.aligned_row(|ui| {
            let field = ui.add(
                TextEdit::singleline(&mut self.tag_buf)
                    .id_salt(("dir_tag", index))
                    .desired_width(140.0)
                    .margin(field_margin(ui))
                    .hint_text("留空清除"),
            );
            // 取色按钮只在有标记文字时才写回配置：没有文字的标记不显示，颜色存着也是死数据
            let current = self.cfg.dirs[index].tag_color.clone();
            let mut rgb = tag_color_rgb(&current);
            let picked = crate::ui::color_swatch(ui, &mut rgb);
            if picked.changed() && !self.cfg.dirs[index].tag.trim().is_empty() {
                self.cfg.dirs[index].tag_color = tag_color_hex(rgb);
                self.persist();
            }
            if self.tag_focus {
                field.request_focus();
                self.tag_focus = false;
            }
            let enter = field.has_focus() && ui.input(|i| i.key_pressed(Key::Enter));
            let esc = field.has_focus() && ui.input(|i| i.key_pressed(Key::Escape));
            if esc {
                self.edit_tag = None;
                self.hint("已取消修改标记");
            } else if enter || field.lost_focus() {
                let text = self.tag_buf.trim().to_owned();
                self.edit_tag = None;
                if self.cfg.dirs[index].tag != text {
                    self.cfg.dirs[index].tag.clone_from(&text);
                    if text.is_empty() {
                        self.cfg.dirs[index].tag_color.clear();
                    }
                    self.persist();
                }
                self.hint(if text.is_empty() {
                    "已清除标记".to_owned()
                } else {
                    format!("标记已改为：{text}")
                });
            }
            // 在用标记下拉：点一下连名字带颜色一起套上，口径与手输提交一致
            let tags = self.existing_tags(self.cfg.dirs[index].tag.trim());
            if !tags.is_empty() {
                egui::Popup::from_response(&field)
                    .open(true)
                    .gap(2.0)
                    .show(|ui| {
                        ui.set_min_width(140.0);
                        ScrollArea::vertical()
                            .max_height(160.0)
                            .show(ui, |ui| {
                                ui.spacing_mut().item_spacing.y = 2.0;
                                for (name, color) in &tags {
                                    let [r, g, b] = tag_color_rgb(color);
                                    let dot = RichText::new("● ").color(Color32::from_rgb(r, g, b));
                                    let label = RichText::new(name.clone());
                                    if ui
                                        .add(egui::Button::new((dot, label)).small())
                                        .clicked()
                                    {
                                        self.apply_tag(index, name, color);
                                    }
                                }
                            });
                    });
            }
        });
    }

    /// 套用在用标记：名字与颜色一起写回并关闭编辑行；名字没变就不动（免得把
    /// 用户刚选的颜色覆盖掉）
    fn apply_tag(&mut self, index: usize, text: &str, color: &str) {
        self.tag_buf = text.to_owned();
        self.edit_tag = None;
        if self.cfg.dirs[index].tag != text {
            self.cfg.dirs[index].tag = text.to_owned();
            self.cfg.dirs[index].tag_color = color.to_owned();
            self.persist();
        }
        self.hint(format!("标记已改为：{text}"));
    }

    /// 主页最右侧的样式切换器：两个选项同在一只圆角长方形里，当前那项被一个框圈住。
    /// 这个 egui 分支既没有 Switch 也没有 SelectableLabel，整只手画。
    fn compact_switch(&mut self, ui: &mut Ui) {
        let on = self.cfg.home_compact;
        let dark = ui.visuals().dark_mode;
        let pad = 2.0;
        let cell = Vec2::new(46.0, 22.0);
        let (rect, _) = ui.allocate_exact_size(
            Vec2::new(cell.x * 2.0 + pad * 2.0, cell.y + pad * 2.0),
            egui::Sense::hover(),
        );
        let cell_rect = |index: usize| {
            egui::Rect::from_min_size(
                egui::pos2(rect.min.x + pad + index as f32 * cell.x, rect.min.y + pad),
                cell,
            )
        };
        let accent = ui.visuals().selection.bg_fill;
        let painter = ui.painter();
        painter.rect_filled(rect, 7.0, pane_hover(ui));
        painter.rect_stroke(
            rect,
            7.0,
            egui::Stroke::new(1.0, pane_stroke(ui)),
            egui::StrokeKind::Inside,
        );
        let current = cell_rect(on as usize);
        // 选中格：深色档与 selection.bg_fill 同口径（主题色占比四成上下），
        // 透明度太低会显得比主题色深一截
        painter.rect_filled(current, 5.0, tint(accent, if dark { 104 } else { 34 }));
        painter.rect_stroke(
            current,
            5.0,
            egui::Stroke::new(1.5, ink(ui, accent)),
            egui::StrokeKind::Inside,
        );
        let strong = ui.visuals().strong_text_color();
        let weak = ui.visuals().weak_text_color();
        let tip = "切换主页目录列表的样式\n详细：整行 + 一排按钮，信息全\n简约：正方形卡片，只留名称和版本，按钮收进「+」";
        for (index, name) in ["详细", "简约"].into_iter().enumerate() {
            let cell = cell_rect(index);
            // `place` 只摆这一格、不动父 Ui 的光标，而且它的布局本来就是居中的
            ui.place(
                cell,
                egui::Label::new(
                    RichText::new(name)
                        .size(12.0)
                        .color(if (index == 1) == on { strong } else { weak }),
                ),
            );
            // 感应区压在字之上：点当前那项什么都不做，点另一侧才切
            if ui
                .interact(cell, egui::Id::new(("home_style", index)), egui::Sense::click())
                .on_hover_text(tip)
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .clicked()
                && (index == 1) != on
            {
                self.cfg.home_compact = index == 1;
                self.persist();
                self.hint(if self.cfg.home_compact {
                    "已切到简约样式：目录显示为卡片"
                } else {
                    "已切回详细样式"
                });
            }
        }
    }

    /// 卡片里塞不下行内输入框，简约形态下别名单独占一行，摆在列表上方
    fn compact_editors(&mut self, ui: &mut Ui) {
        let total = self.cfg.dirs.len();
        let fill = pane_fill(ui);
        if let Some(index) = self.edit_label.filter(|i| *i < total) {
            let label = self.dir_label(index);
            Frame::new()
                .inner_margin(6.0)
                .corner_radius(8.0)
                .stroke(egui::Stroke::new(1.0, pane_stroke(ui)))
                .fill(fill)
                .show(ui, |ui| {
                    ui.aligned_row(|ui| {
                        ui.label(RichText::new(format!("修改别名 · {label}：")).size(12.0));
                        self.alias_editor(ui, index);
                    });
                });
            ui.add_space(4.0);
        }
        if let Some(index) = self.edit_group.filter(|i| *i < total) {
            let label = self.dir_label(index);
            Frame::new()
                .inner_margin(6.0)
                .corner_radius(8.0)
                .stroke(egui::Stroke::new(1.0, pane_stroke(ui)))
                .fill(fill)
                .show(ui, |ui| {
                    ui.aligned_row(|ui| {
                        ui.label(RichText::new(format!("修改分组 · {label}：")).size(12.0));
                        self.group_editor(ui, index);
                    });
                });
            ui.add_space(4.0);
        }
        if let Some(index) = self.edit_tag.filter(|i| *i < total) {
            let label = self.dir_label(index);
            Frame::new()
                .inner_margin(6.0)
                .corner_radius(8.0)
                .stroke(egui::Stroke::new(1.0, pane_stroke(ui)))
                .fill(fill)
                .show(ui, |ui| {
                    ui.aligned_row(|ui| {
                        ui.label(RichText::new(format!("修改标记 · {label}：")).size(12.0));
                        self.tag_editor(ui, index);
                    });
                });
            ui.add_space(4.0);
        }
    }

    // ------------------------------------------------------------ 拖动：并入分组 / 改顺序

    /// 要按住这么久才算想拖动：一按下就拖，选个目录都会把行拽走
    const DRAG_HOLD: f64 = 1.0;

    /// 记下鼠标在谁身上按下、什么时刻按下，够 `DRAG_HOLD` 才交给 update_drag 转成拖动
    fn arm_drag(&mut self, ui: &Ui, rect: egui::Rect, index: usize) {
        if self.drag.is_some() || self.drag_pending.is_some() {
            return;
        }
        let (down, origin, now) = ui.input(|i| {
            (
                i.pointer.button_down(egui::PointerButton::Primary),
                i.pointer.press_origin(),
                i.time,
            )
        });
        // 遮没遮住要读 memory，不能塞进上面那个 input 闭包里（egui 的读锁套自己会死锁）
        let pressed_here =
            down && origin.is_some_and(|o| rect.contains(o) && !covered_by_overlay(ui.ctx(), o));
        if pressed_here {
            self.drag_pending = Some((index, now));
        }
    }

    /// 每帧画主页之前调一次：上一帧登记的落点区域 + 当前指针 = 这一帧该落在哪儿。
    /// 松手就搬行；还按着就更新插入线和跟手的小标签；
    /// 既没有落下事件也没了按下状态（拖出窗口松手、焦点被抢）当取消处理
    pub(crate) fn update_drag(&mut self, ui: &Ui) {
        // 按够了时长才真的开始拖，没按够就松手的只当是一次点击
        if let Some((index, since)) = self.drag_pending {
            let (holding, pos, now) = ui.input(|i| {
                (
                    i.pointer.button_down(egui::PointerButton::Primary),
                    i.pointer.interact_pos(),
                    i.time,
                )
            });
            if !holding {
                self.drag_pending = None;
            } else if self.drag.is_none()
                && now - since >= Self::DRAG_HOLD
                // 按够时间了，但指针这会儿停在弹窗 / 弹层上：不算起拖
                && !pos.is_some_and(|pos| covered_by_overlay(ui.ctx(), pos))
            {
                self.drag = Some(index);
                self.drag_pending = None;
            }
        }
        let Some(from) = self.drag else { return };
        let ctx = ui.ctx();
        let pos = ctx.input(|i| i.pointer.interact_pos());
        let released = ctx.input(|i| i.pointer.any_released());
        let holding = ctx.input(|i| i.pointer.button_down(egui::PointerButton::Primary));
        if released || !holding {
            if released {
                if let Some(pos) = pos {
                    // 松手在浮层上只当取消：那一下是给浮层的，不是给底下这行的
                    if !covered_by_overlay(ctx, pos) {
                        if let Some(zone) = hit_zone(&self.drop_zones, from, pos) {
                            match zone {
                                DropZone::Header(group) => self.drop_on_header(from, &group),
                                DropZone::Row(target, edge) => self.drop_beside(from, target, edge),
                            }
                        }
                    }
                }
            }
            self.drag = None;
            self.drag_pos = None;
            self.drag_target = None;
            return;
        }
        if let Some(pos) = pos {
            self.drag_pos = Some(pos);
            self.drag_target = hit_zone(&self.drop_zones, from, pos);
        }
        self.paint_drag_ghost(ui);
    }

    /// 落在分组标题条上：并入那一组，排到同组末尾。已经在这一组里就什么都不动
    fn drop_on_header(&mut self, from: usize, group: &str) {
        let want = group.trim().to_owned();
        if self.cfg.dirs[from].group.trim() == want {
            return;
        }
        let Some(last) = self
            .cfg
            .dirs
            .iter()
            .enumerate()
            .filter(|(index, dir)| *index != from && dir.group.trim() == want)
            .map(|(index, _)| index)
            .max()
        else {
            return;
        };
        // 摘掉自己之后，同组末行的位置往前挪了一位；插到它后面
        let insert = if from < last { last } else { last + 1 };
        // 收着的组接住落点要先展开：不然这行一进去就看不见，像是拖失败了
        self.cfg.collapsed_groups.retain(|g| g.trim() != want);
        self.shift_directory(from, insert, Some(want.clone()));
        self.hint(format!("已把 {} 并入分组「{want}」", self.dir_label(insert)));
    }

    /// 落在另一行的前半 / 后半：插到它之前或之后，分组跟着那一行走。
    /// 目标行没分组，于是这一挪就等于把它从原来的分组里拉出来
    fn drop_beside(&mut self, from: usize, target: usize, edge: Edge) {
        let len = self.cfg.dirs.len();
        if from >= len || target >= len || from == target {
            return;
        }
        let was = self.cfg.dirs[from].group.trim().to_owned();
        let group = self.cfg.dirs[target].group.clone();
        // 目标行在「摘掉自己之后」的位置，也就是搬完紧接着它的那一位
        let at = if from < target { target - 1 } else { target };
        let insert = match edge {
            Edge::Before => at,
            Edge::After => (at + 1).min(len - 1),
        };
        self.shift_directory(from, insert, Some(group));
        let name = self.dir_label(insert);
        let beside = self.dir_label(at);
        let new = self.cfg.dirs[insert].group.trim().to_owned();
        let mut note = format!(
            "已把 {name} 移到 {beside} {}",
            if edge == Edge::Before { "之前" } else { "之后" }
        );
        if new != was {
            let changed = if new.is_empty() {
                format!("（移出分组「{was}」）")
            } else {
                format!("（并入分组「{new}」）")
            };
            note.push_str(&changed);
        }
        self.hint(note);
    }

    /// 跟手的那张小标签：手里拖的是哪个目录，一眼看得见
    fn paint_drag_ghost(&self, ui: &Ui) {
        let (Some(from), Some(pos)) = (self.drag, self.drag_pos) else { return };
        let painter = ui.ctx().layer_painter(egui::LayerId::new(
            egui::Order::Foreground,
            egui::Id::new("dir_drag_ghost"),
        ));
        let color = ui.visuals().strong_text_color();
        let galley =
            painter.layout_no_wrap(self.dir_label(from), egui::FontId::proportional(12.5), color);
        let pad = Vec2::splat(5.0);
        let rect = egui::Rect::from_min_size(
            pos + Vec2::new(14.0, 12.0),
            galley.size() + pad * 2.0,
        );
        painter.rect_filled(rect, 7.0, ui.visuals().window_fill);
        painter.rect_stroke(
            rect,
            7.0,
            egui::Stroke::new(1.0, self.accent_ink()),
            egui::StrokeKind::Inside,
        );
        painter.galley(rect.min + pad, galley, color);
    }

    /// 插入线：只有「正是要往这儿落」的那一行才画。详细列表横着排，线落在行上沿或下沿；
    /// 卡片网格先左后右，线落在卡片左沿或右沿
    fn paint_insert_line(&self, ui: &Ui, rect: egui::Rect, index: usize, horizontal: bool) {
        let Some(DropZone::Row(target, edge)) = self.drag_target.clone() else { return };
        if target != index || self.drag == Some(index) {
            return;
        }
        let stroke = egui::Stroke::new(2.0, self.accent_ink());
        if horizontal {
            let x = if edge == Edge::Before { rect.min.x } else { rect.max.x };
            ui.painter().vline(
                x,
                egui::Rangef::new(rect.min.y + 6.0, rect.max.y - 6.0),
                stroke,
            );
        } else {
            let y = if edge == Edge::Before { rect.min.y } else { rect.max.y };
            ui.painter().hline(
                egui::Rangef::new(rect.min.x + 6.0, rect.max.x - 6.0),
                y,
                stroke,
            );
        }
    }

    /// 显示序列：把目录摆成「段」，每段是（组名, 该组行号列表）。
    /// 同名分组的行不管散在哪儿，全部拉到该组**第一次出现**的位置集中渲染——
    /// 后加的目录落在别的组后面，也不该裂出第二个同名标题；
    /// 未分组行按原相对顺序插空（相邻的并成一段）。配置本身不搬动。
    pub(crate) fn group_segments(&self) -> Vec<(Option<String>, Vec<usize>)> {
        let mut segments: Vec<(Option<String>, Vec<usize>)> = Vec::new();
        let mut taken = vec![false; self.cfg.dirs.len()];
        for index in 0..self.cfg.dirs.len() {
            if taken[index] {
                continue;
            }
            taken[index] = true;
            let group = self.cfg.dirs[index].group.trim().to_owned();
            if group.is_empty() {
                match segments.last_mut() {
                    Some((None, rows)) => rows.push(index),
                    _ => segments.push((None, vec![index])),
                }
            } else {
                let rows: Vec<usize> = (index..self.cfg.dirs.len())
                    .filter(|i| self.cfg.dirs[*i].group.trim() == group)
                    .collect();
                for row in &rows {
                    taken[*row] = true;
                }
                segments.push((Some(group), rows));
            }
        }
        segments
    }

    /// 简约形态的网格：列数按可用宽度算，一横行一横行地摆正方形卡片。
    /// 分组标题独占一整行：先按 group_segments 摆成「标题 / 卡片」的可见序列
    /// （收起的组整段跳过），再按列数流式铺开
    fn dir_cards(&mut self, ui: &mut Ui) {
        let gap = 10.0;
        let cols = (((ui.available_width() + gap) / (CARD_W + gap)).floor() as usize).max(1);
        ScrollArea::vertical()
            .id_salt("dir_cards")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = gap;
                // 同详细模式：卡片里的文字要是不关掉选中，就会把点选和拖动从整张卡片的感应区抢走
                ui.style_mut().interaction.selectable_labels = false;
                let mut row: Vec<usize> = Vec::new();
                for (header, rows) in self.group_segments() {
                    if let Some(group) = &header {
                        // 标题独占一整行：先收掉手里没铺满的横行
                        if !row.is_empty() {
                            self.card_row(ui, &row, gap);
                            row.clear();
                        }
                        if self.group_header(ui, group, rows.len()) {
                            continue;
                        }
                    }
                    for index in rows {
                        // 行内「移除」会当场缩短 cfg.dirs，画之前重新比一次上界
                        if index >= self.cfg.dirs.len() {
                            continue;
                        }
                        row.push(index);
                        if row.len() == cols {
                            self.card_row(ui, &row, gap);
                            row.clear();
                        }
                    }
                }
                if !row.is_empty() {
                    self.card_row(ui, &row, gap);
                }
            });
    }

    /// 摆一横行卡片。行内「移除」会当场缩短 cfg.dirs，每格都重新比一次上界
    fn card_row(&mut self, ui: &mut Ui, row: &[usize], gap: f32) {
        ui.horizontal(|ui| {
            for (offset, &index) in row.iter().enumerate() {
                if index >= self.cfg.dirs.len() {
                    break;
                }
                let (clicked, double_clicked) = self.dir_card(ui, index);
                if clicked {
                    self.selected = Some(index);
                    self.confirm_remove = None;
                }
                if double_clicked {
                    self.open_folder(index);
                }
                if offset + 1 < row.len() && index + 1 < self.cfg.dirs.len() {
                    ui.add_space(gap);
                }
            }
        });
    }

    /// 分组标题：通栏底色条，点整条就收起 / 展开（收起状态记进配置）。
    /// 返回 true = 该组当前收着，调用方应跳过整段目录行
    fn group_header(&mut self, ui: &mut Ui, group: &str, count: usize) -> bool {
        let collapsed = self.cfg.collapsed_groups.iter().any(|g| g == group);
        let title = format!("{} {group}（{count}）", if collapsed { "▶" } else { "▼" });
        // 手里正拖着别的行为这条标题而来时，整条点亮：一眼看出松手会并进哪一组
        let targeted = matches!(&self.drag_target, Some(DropZone::Header(g)) if g == group);
        // 底色：带一点主色的淡染 + 发丝描边，一眼和下面的目录行分层；
        // 标题文字用正文色（层级交给字重和这条淡染底，不靠把字涂蓝）
        let accent = self.accent();
        let fill = if ui.visuals().dark_mode {
            tint(accent, if targeted { 66 } else { 30 })
        } else {
            tint(pale(accent, 0.21), if targeted { 255 } else { 200 })
        };
        let bar = Frame::new()
            .fill(fill)
            .stroke(egui::Stroke::new(
                if targeted { 1.8 } else { 1.0 },
                if targeted { self.accent_ink() } else { pane_stroke(ui) },
            ))
            .corner_radius(8.0)
            .inner_margin(egui::Margin::symmetric(8, 3))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(RichText::new(&title).strong().size(13.0));
            });
        // 落点登记：整条标题条都是「并入这一组」。收起的组照样画条、照样接落点，
        // 松手时由 drop_on_header 把组展开，否则落进去的行看不见，像是拖失败了
        let zone = bar.response.rect.intersect(ui.clip_rect());
        if !zone.is_negative() {
            self.drop_zones.push((DropZone::Header(group.to_owned()), zone));
        }
        // 感应区压在整条底色上：点空白处也能收起 / 展开
        let hit = ui
            .interact(
                bar.response.rect,
                egui::Id::new(("group_header", group)),
                egui::Sense::click(),
            )
            .on_hover_text(if collapsed { "点击展开这一组" } else { "点击收起这一组" })
            .on_hover_cursor(egui::CursorIcon::PointingHand);
        if hit.clicked() {
            if collapsed {
                self.cfg.collapsed_groups.retain(|g| g != group);
            } else {
                self.cfg.collapsed_groups.push(group.to_owned());
            }
            self.persist();
        }
        collapsed
    }

    /// 简约形态下的一张宽行卡片：左列自上而下是名称（带状态灯）/ 标记胶囊 / 一行版本，
    /// 右侧竖排计数胶囊（可更新 / 待提交 / 差异）；右键卡片弹出全部操作菜单。
    /// 显示哪些块由 cfg.card_show 逐项开关。返回 (点中卡片, 双击卡片)。
    fn dir_card(&mut self, ui: &mut Ui, index: usize) -> (bool, bool) {
        let view = self.dirs.get(index).cloned().unwrap_or_default();
        let busy = self.pool.is_busy(index);
        let label = self.dir_label(index);
        let path = self.cfg.dirs[index].path.clone();
        let is_selected = self.selected == Some(index);
        let dark = ui.visuals().dark_mode;
        let pending = view.out_of_date.unwrap_or(0);
        let changed = view.changed.unwrap_or(0);
        let conflicts = view.conflicts.unwrap_or(0);
        let weak = ui.visuals().weak_text_color();

        let show = self.cfg.card_show;
        let (_id, rect) = ui.allocate_space(Vec2::new(CARD_W, CARD_H));
        // 整张卡片的感应区先注册，里面的标记和「+」都注册在它之后：
        // 点在控件上时控件优先命中，点在空白处才是「选中这一张」；
        // 顺带让它可拖——整张卡片都是把手，挪到别的卡片左/右侧就改顺序
        let hit = ui.interact(
            rect,
            egui::Id::new(("dir_card", index)),
            egui::Sense::click_and_drag(),
        );
        self.arm_drag(ui, rect, index);
        // 落点登记：卡片网格先左后右，于是按左、右两半区分给「插到这张之前 / 之后」
        self.drop_zones.extend(half_zones(rect, ui.clip_rect(), index, true));
        self.paint_insert_line(ui, rect, index, true);
        let dragging = self.drag == Some(index);
        // 边框把状态再标一遍，余光里也能看出来：冲突最要紧（不人工处理就提交不上去），
        // 其次是连不上服务器，再次是正在跑任务
        let border = if conflicts > 0 {
            ink(ui, Color32::from_rgb(255, 80, 160))
        } else if view.remote == Some(false) {
            ink(ui, crate::ERR)
        } else if busy {
            ink(ui, crate::AMBER)
        } else {
            pane_stroke(ui)
        };
        // 悬停浮起：卡片整块是手绘的，这里只把「画」出来的那块往上挪 1px，感应区留在原地不动
        let hovered = ui
            .ctx()
            .read_response(egui::Id::new(("dir_card", index)))
            .is_some_and(|resp| resp.hovered());
        let card = rect.translate(Vec2::new(0.0, if hovered { -1.0 } else { 0.0 }));
        // 柔和投影：一层半透明靛影加模糊，画在填充之前，上面那步会盖住中间、只留四周的虚影
        let mut shadow = egui::epaint::RectShape::filled(
            card,
            10.0,
            Color32::from_rgba_unmultiplied(
                if dark { 0 } else { 58 },
                if dark { 0 } else { 76 },
                if dark { 0 } else { 126 },
                if dark { 120 } else { 44 },
            ),
        );
        shadow.blur_width = 10.0;
        ui.painter().add(shadow);
        // 选中先铺一层不透光的底：主色本身是半透明的，底下那圈投影不盖住就会一起被叠暗，
        // 同一个选中态在两种样式里看着就不是一个颜色
        if is_selected {
            ui.painter().rect_filled(card, 10.0, ui.visuals().panel_fill);
        }
        ui.painter().rect_filled(
            card,
            10.0,
            if is_selected {
                ui.visuals().selection.bg_fill
            } else if hovered || dragging {
                pane_hover(ui)
            } else if dark {
                pane_fill(ui)
            } else {
                // 浅色主题点名要白底：卡片要比页面底色更亮，浮起的感觉靠那圈投影撑
                Color32::WHITE
            },
        );
        ui.painter().rect_stroke(
            card,
            10.0,
            egui::Stroke::new(
                if is_selected { 1.6 } else { 1.0 },
                if dragging { self.accent_ink() } else { border },
            ),
            egui::StrokeKind::Inside,
        );
        let inner = card.shrink(10.0);
        // 先把右侧的水平分位定下来：计数胶囊列贴着右内边，左列（名称 / 标记 / 版本）吃剩下的宽度
        let pill_right = inner.max.x;
        let pill_left = pill_right - PILL_W;
        let left_w = pill_left - 8.0 - inner.min.x;
        // 检测状态灯（配色和悬停说明同详细模式的那枚 ●）
        let lamp = ink(
            ui,
            match view.remote {
                Some(true) => crate::OK,
                Some(false) => crate::ERR,
                None if busy => crate::AMBER,
                None => Color32::from_gray(120),
            },
        );
        // 第一行：状态灯 + 名称 +（忙碌时的）转圈
        let name_rect = egui::Rect::from_min_size(inner.min, Vec2::new(left_w, 20.0));
        let mut name_row = ui.new_child(
            egui::UiBuilder::new()
                .id(egui::Id::new(("dir_card_name", index)))
                .max_rect(name_rect)
                .layout(Layout::left_to_right(Align::Center)),
        );
        if show.lamp {
            name_row.label(RichText::new("●").size(13.0).color(lamp));
            name_row.add_space(4.0);
        }
        name_row.add(egui::Label::new(RichText::new(&label).strong().size(13.5)).truncate());
        if busy {
            // 忙碌只多一枚转圈：任务文字挤进来会把名称顶出去，进度看边框颜色就够了。
            // 颜色显式给：跟着主题走的话浅色底上那圈几乎看不见
            name_row.add_space(4.0);
            name_row.add(egui::Spinner::new().size(13.0).color(ink(ui, crate::AMBER)));
        }
        // 标记色块：名称正下方，实心标签色（与详细模式右下角那块同一套），颜色在「修改标记」的取色器里选
        let tag = self.cfg.dirs[index].tag.clone();
        if show.tag && !tag.is_empty() {
            let tag_rect = egui::Rect::from_min_size(
                egui::pos2(inner.min.x, inner.min.y + 24.0),
                Vec2::new(left_w, 18.0),
            );
            let mut tag_row = ui.new_child(
                egui::UiBuilder::new()
                    .id(egui::Id::new(("dir_card_tag", index)))
                    .max_rect(tag_rect)
                    .layout(Layout::left_to_right(Align::Center)),
            );
            tag_block(&mut tag_row, &tag, tag_color(&self.cfg.dirs[index]));
        }
        // 底部一行：本地 / 远端版本各一小段，口径与详细模式一致
        let up_to_date = view.out_of_date == Some(0) && !view.remote_rev.is_empty();
        let local = match &view.info {
            Some(info) => {
                let rev = if up_to_date { &view.remote_rev } else { &info.revision };
                format!("本地 {}", crate::repo::fmt_rev(view.kind, rev))
            }
            None => "本地 —".to_owned(),
        };
        let remote = if view.remote_rev.is_empty() {
            "远端 —".to_owned()
        } else {
            format!("远端 {}", crate::repo::fmt_rev(view.kind, &view.remote_rev))
        };
        // 远端有更新时，远端那段用详细模式里「远端 rX（可更新 N 项）」的黄色
        let remote_color = if pending > 0 {
            ink(ui, crate::AMBER)
        } else {
            weak
        };
        if show.ver_local || show.ver_remote {
            let ver_rect = egui::Rect::from_min_size(
                egui::pos2(inner.min.x, inner.max.y - 17.0),
                Vec2::new(left_w, 17.0),
            );
            let mut ver_row = ui.new_child(
                egui::UiBuilder::new()
                    .id(egui::Id::new(("dir_card_ver", index)))
                    .max_rect(ver_rect)
                    .layout(Layout::left_to_right(Align::Center)),
            );
            if show.ver_local {
                ver_row.label(RichText::new(&local).size(10.5).color(weak));
            }
            if show.ver_local && show.ver_remote {
                ver_row.label(RichText::new("·").size(10.5).color(weak));
            }
            if show.ver_remote {
                ver_row.label(RichText::new(&remote).size(10.5).color(remote_color));
            }
        }
        // 右侧竖排计数胶囊：可更新（原 ↓ 的活儿）、待提交（原 ↑ 的活儿）、本地差异（原 ↔ 的活儿），
        // 整列垂直居中；差异那枚只对绑定过本地端的卡片出现，不拿一枚 0 冒充「已经比过了」
        let update_tip = format!("服务器上有 {pending} 项本地还没更新\n点击立即更新该目录");
        let commit_tip = format!(
            "本地有 {changed} 项待提交（口径同「全部上传」，不含本目录已屏蔽的条目）\n点击打开该目录的提交页"
        );
        // 整张卡没有一件事可做（可更新 / 待提交 / 差异全是 0）时胶囊一起转绿，
        // 这是「这目录很干净」的余光信号；只要有一件待办，0 的那几枚还是中性灰
        let all_clear = pending == 0
            && changed == 0
            && view.local_diff.as_ref().is_none_or(|s| s.is_clean());
        let mut pills: Vec<(String, bool, Color32, String)> = Vec::new();
        if show.pending {
            pills.push((format!("可更新 {}", capped(pending)), pending > 0, crate::AMBER, update_tip));
        }
        if show.changed {
            pills.push((format!("待提交 {}", capped(changed)), changed > 0, crate::ACCENT, commit_tip));
        }
        if show.diff {
            if let Some(summary) = view.local_diff.as_ref() {
                pills.push((
                    format!("差异 {}", capped(summary.total())),
                    !summary.is_clean(),
                    // 差异是「两边对不上」的提醒，走粉红；别跟可更新的琥珀撞色
                    crate::ERR,
                    self.local_diff_tip(index, summary, self.pool.comparing(index)),
                ));
            }
        }
        let mut update_clicked = false;
        let mut commit_clicked = false;
        let mut local_clicked = false;
        if !pills.is_empty() {
            let count = pills.len() as f32;
            let total_h = count * MARK_H + (count - 1.0) * 4.0;
            let col_rect = egui::Rect::from_min_max(
                egui::pos2(pill_left, card.center().y - total_h / 2.0),
                egui::pos2(pill_right, card.center().y + total_h / 2.0),
            );
            let mut col = ui.new_child(
                egui::UiBuilder::new()
                    .id(egui::Id::new(("dir_card_band", index)))
                    .max_rect(col_rect)
                    .layout(Layout::top_down(Align::Center))
                    .sense(egui::Sense::hover()),
            );
            col.spacing_mut().item_spacing.y = 4.0;
            for (text, active, hue, tip) in &pills {
                let hit = self.count_badge(&mut col, text, *active, *hue, tip, all_clear);
                // 点中哪一枚看文字前缀：跳页动作跟着各自原来的箭头标记走
                if text.starts_with("可更新") {
                    update_clicked = hit;
                } else if text.starts_with("待提交") {
                    commit_clicked = hit;
                } else {
                    local_clicked = hit;
                }
            }
        }

        let mut tip = format!("{label}\n{path}\n");
        if let Some(info) = &view.info {
            tip.push_str(&format!("仓库：{}\n", info.url));
        }
        tip.push_str(&format!("{local} · {remote}\n"));
        tip.push_str(&format!(
            "服务器可更新 {pending} 项 · 本地待提交 {changed} 项\n"
        ));
        if let Some(summary) = view.local_diff.as_ref() {
            tip.push_str(&format!("{}\n", summary.label()));
        }
        if conflicts > 0 {
            tip.push_str(&format!("冲突 / 不完整 {conflicts} 项，必须先人工处理\n"));
        }
        if !view.remote_msg.is_empty() {
            tip.push_str(&view.remote_msg);
            if !view.checked_at.is_empty() {
                tip.push_str(&format!("（{}）", view.checked_at));
            }
        }
        let hit = hit.on_hover_ui(|ui| {
            ui.add(
                egui::Label::new(RichText::new(&tip).size(12.0))
                    .wrap_mode(egui::TextWrapMode::Extend),
            );
        });
        // 右键弹操作菜单（在指针处打开）：原来右下角那枚「+」收进来了，卡片本身干净不少
        egui::Popup::context_menu(&hit).show(|ui| self.card_menu(ui, index, busy, &view));
        if update_clicked && !busy {
            self.selected = Some(index);
            self.spawn_update(index);
        }
        if commit_clicked {
            self.selected = Some(index);
            self.open_commit(index);
        }
        if local_clicked {
            self.selected = Some(index);
            self.open_bcompare_matched();
        }
        (hit.clicked(), hit.double_clicked())
    }

    /// 卡片右侧的圆角长方形计数标记：有 待办时按各自的语义色给「淡底 + 深字」，
    /// 没有待办就是一枚中性灰胶囊；`all_clear`（整卡无待办）时全部转绿。
    /// 不走 `Button`：它的底色 / 描边 / 圆角 / 文字色分 inactive、hovered、active 三套 visuals，
    /// 鼠标进出就换一套，看着就是标记跳一下；`Frame` 不吃控件状态，进出画出来完全一样。
    fn count_badge(
        &mut self,
        ui: &mut Ui,
        text: &str,
        active: bool,
        hue: Color32,
        tip: &str,
        all_clear: bool,
    ) -> bool {
        let (fill, text_color) = if all_clear {
            pill_colors(ui, crate::OK, true)
        } else {
            pill_colors(ui, hue, active)
        };
        let chip = Frame::new()
            .fill(fill)
            .corner_radius((MARK_H / 2.0).floor())
            // 上下 2/4：粗体数字的行高比常规字高 1px，去掉这 1px 才能凑足 MARK_H 的 20
            .inner_margin(egui::Margin {
                left: 5,
                right: 5,
                top: 2,
                bottom: 4,
            })
            .show(ui, |ui| {
                // Extend：竖排布局下内容 ui 会给到整列宽度，缺省的 wrap 会让
                // 「待提交 999+」这种最宽的组合折成两行，把胶囊撑高一倍。
                // 数字单独拆段走 Name("bold") 粗体（fonts::install 保证该族已绑定）
                let mut job = egui::text::LayoutJob::default();
                let number_font = egui::FontId {
                    size: MARK_FONT,
                    family: egui::FontFamily::Name("bold".into()),
                };
                match text.rfind(' ') {
                    Some(idx) => {
                        job.append(
                            &text[..=idx],
                            0.0,
                            egui::TextFormat::simple(
                                egui::FontId::proportional(MARK_FONT),
                                text_color,
                            ),
                        );
                        job.append(
                            &text[idx + 1..],
                            0.0,
                            egui::TextFormat::simple(number_font, text_color),
                        );
                    }
                    None => {
                        job.append(
                            text,
                            0.0,
                            egui::TextFormat::simple(
                                egui::FontId::proportional(MARK_FONT),
                                text_color,
                            ),
                        );
                    }
                }
                ui.add(
                    egui::Label::new(job)
                        .selectable(false)
                        .wrap_mode(egui::TextWrapMode::Extend),
                );
            });
        // 感应区最后注册，压在字之上；0 的那枚点了没意义，所以按下也不响应
        let area = ui.interact(
            chip.response.rect,
            ui.id().with(("dir_card_badge", text)),
            egui::Sense::click(),
        );
        area.on_hover_text(tip).clicked() && active
    }

    /// 「+」里收纳的操作菜单：一级是四个常用动作，「更多」下面接详细模式那份菜单
    fn card_menu(&mut self, ui: &mut Ui, index: usize, busy: bool, view: &DirView) {
        if ui.button("更新").clicked() {
            self.selected = Some(index);
            self.confirm_remove = None;
            self.spawn_update(index);
            ui.close();
        }
        if ui.button("上传").clicked() {
            self.selected = Some(index);
            self.open_commit(index);
            ui.close();
        }
        if ui.button("历史").clicked() {
            self.selected = Some(index);
            self.open_history(index);
            ui.close();
        }
        if ui.button("打开目录").clicked() {
            self.open_folder(index);
            ui.close();
        }
        ui.separator();
        ui.menu_button("更多", |ui| self.more_menu(ui, index, busy, view));
        ui.separator();
        // 移除仍要点两次确认：菜单一点就收，所以第二下要重新展开点「确认移除」
        let confirm = self.confirm_remove == Some(index);
        let mut button = egui::Button::new(if confirm {
            RichText::new("确认移除?")
                .strong()
                .color(crate::DANGER_TEXT)
        } else {
            RichText::new("移除")
        });
        if confirm {
            button = button.fill(crate::DANGER_FILL);
        }
        if ui
            .add_enabled(!busy, button)
            .on_hover_text("仅从列表移除，不删除磁盘文件")
            .clicked()
        {
            if confirm {
                self.remove_directory(index);
            } else {
                self.confirm_remove = Some(index);
                self.hint("再次展开「+」点「确认移除」可把该目录从列表移除（不会删除文件）");
            }
            ui.close();
        }
    }

    /// 添加目录这条横栏。它挂在中央面板之外的顶栏上（见 `RepoApp::ui`），
    /// 所以能顶到窗口两边；下面的目录列表照旧留横向边距。
    pub fn add_dir_bar(&mut self, ui: &mut Ui) {
        let margin = field_margin(ui);
        ui.aligned_row(|ui| {
            ui.label(RichText::new("添加仓库目录").strong());
            let width = (ui.available_width() - 250.0).max(200.0);
            let field = ui.add(
                TextEdit::singleline(&mut self.new_path)
                    .hint_text(r"工作副本目录，例如 C:\Test")
                    .margin(margin)
                    .desired_width(width),
            );
            let enter = field.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
            if ui.button("选择文件夹…").clicked() {
                if let Some(folder) = rfd::FileDialog::new()
                    .set_title("选择 SVN 工作副本目录")
                    .pick_folder()
                {
                    self.new_path = folder.display().to_string();
                }
            }
            if enter || ui.button("＋ 添加").clicked() {
                let path = self.new_path.clone();
                self.add_directory(path);
            }
        });
        ui.aligned_row(|ui| {
            ui.label(RichText::new("别名(可选)").weak().size(12.0));
            ui.add(
                TextEdit::singleline(&mut self.new_label)
                    .hint_text("列表中显示的名称")
                    .margin(margin)
                    .desired_width(200.0),
            );
            ui.label(RichText::new("也可以直接把文件夹拖到窗口里添加").weak().size(12.0));
        });
    }

    /// 目录列表上方那一组：标题、冲突汇总、一排批量操作按钮
    fn toolbar_row(&mut self, ui: &mut Ui) {
        let count = self.cfg.dirs.len();
        let changes: usize = self.dirs.iter().filter_map(|d| d.changed).sum();
        let title = ui.label(
            RichText::new(format!("目录列表（{count} 个，待提交合计 {changes} 项）")).strong(),
        );
        title.on_hover_text(
            "单击选中，双击打开目录。拖动要先按住 1 秒再挪手，按得没这么久只算点了一下：\n\
             · 拖到分组标题条上 = 并入那一组，排在同组末尾\n\
             · 拖到另一行的上半 / 下半（卡片是左半 / 右半）= 按显示顺序插到它前面或后面，\n\
             并跟着那一行的分组走——拖到未分组的行上就是把它移出分组\n\
             · 松手时不在任何行或标题条上 = 什么都没动",
        );
        // 目录多的时候冲突标记要滚动才看得见，这里聚合一份：不用滚就知道有没有卡住
        let blocked: Vec<(usize, usize)> = self
            .dirs
            .iter()
            .enumerate()
            .filter_map(|(index, view)| {
                Some((index, view.conflicts.filter(|count| *count > 0)?))
            })
            .collect();
        if !blocked.is_empty() {
            let total: usize = blocked.iter().map(|(_, count)| count).sum();
            let list = blocked
                .iter()
                .map(|(index, count)| format!("{}：{count} 项", self.dir_label(*index)))
                .collect::<Vec<_>>()
                .join("\n");
            // 和目录行「待提交 N 项 · 冲突 N 项」用同一根分隔点，接在标题后面才不像半句话
            ui.label(RichText::new("·").size(12.0).weak());
            let first = blocked[0].0;
            let hit = ui.add(
                egui::Label::new(
                    RichText::new(format!("冲突 {total} 项待处理"))
                        .size(12.0)
                        .strong()
                        .underline()
                        .color(ink(ui, Color32::from_rgb(255, 80, 160))),
                )
                .sense(egui::Sense::click()),
            );
            if hit.clicked() {
                self.open_commit(first);
            }
            hit.on_hover_text(format!(
                "必须人工处理才能继续提交的条目（冲突 / 不完整），按目录列：\n{list}\n\n点击打开第一个有冲突目录的提交页。"
            ));
        }
        if ui.button("全部检测").clicked() {
            self.hint("正在检测全部目录 …");
            self.spawn_all_refresh();
        }
        if ui.button("全部更新").clicked() {
            for index in 0..self.cfg.dirs.len() {
                self.spawn_update(index);
            }
        }
        if ui
            .button("全部上传")
            .on_hover_text("所有目录使用同一条提交说明一次性提交（将改动服务器，需再次确认）")
            .clicked()
        {
            self.upload_all = Some(UploadAll {
                message: String::new(),
                confirm: false,
                focus: true,
                // 打开时默认全选，想排除谁就在窗口里去掉勾
                checked: vec![true; self.cfg.dirs.len()],
                detail: None,
            });
        }
        if ui
            .button("批量修改仓库")
            .on_hover_text("服务器迁移时批量替换多个目录的仓库地址前缀（每个目录单独执行任务，需再次确认）")
            .clicked()
        {
            self.batch_relocate = Some(crate::BatchRelocate {
                focus: true,
                checked: vec![false; self.cfg.dirs.len()],
                ..Default::default()
            });
        }
        if ui
            .button("批量分组 / 标记")
            .on_hover_text("给勾选的多个目录统一设置分组或标记（只改本地配置，随时能改回）")
            .clicked()
        {
            self.batch_label = Some(crate::BatchLabel {
                focus: true,
                checked: vec![false; self.cfg.dirs.len()],
                ..Default::default()
            });
        }
    }

    pub fn main_page(&mut self, ui: &mut Ui) {
        // 先用上一帧登记的落点把手里这行落定，再清空区域让这一帧重新登记
        self.update_drag(ui);
        self.drop_zones.clear();
        ui.aligned_row(|ui| {
            self.toolbar_row(ui);
            // 子布局从右往左排，切换器才能顶在这一行的最右侧
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                self.compact_switch(ui);
            });
        });
        if self.cfg.dirs.is_empty() {
            ui.add_space(30.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("还没有目录").size(18.0).weak());
                ui.label(RichText::new("在上方输入或选择 SVN 工作副本目录，然后回车添加").weak());
            });
            return;
        }
        if self.cfg.home_compact {
            self.compact_editors(ui);
            self.dir_cards(ui);
            return;
        }
        ScrollArea::vertical()
            .id_salt("dir_list")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 5.0;
                // 文字默认是「可选中」的 Label，那种 Label 既收点也收拖，注册又排在整行感应区之后，
                // 于是点名字不选中、按住名字拖不动：这一片关掉文字选中
                ui.style_mut().interaction.selectable_labels = false;
                // 按显示序列画：同名组拉到一起共用一枚标题（收起的组整段跳过），
                // 未分组行按原顺序插空。上移 / 下移也按这个显示顺序走
                for (header, rows) in self.group_segments() {
                    if let Some(group) = &header {
                        if self.group_header(ui, group, rows.len()) {
                            continue;
                        }
                    }
                    for index in rows {
                        // 行内「移除」会当场修改 cfg.dirs，画之前重新比一次上界
                        if index >= self.cfg.dirs.len() {
                            continue;
                        }
                        // 整行可点选中，也可按住拖走：scope 的点击感应区注册在行内所有控件之下，
                        // 因此行内按钮、菜单依旧优先命中，不会被抢走；
                        // 用固定 id 保证这一行的感应区每帧都是同一个控件（目录顺序变化也不影响）
                        let row_hit = ui
                            .scope_builder(
                                egui::UiBuilder::new()
                                    .id(egui::Id::new(("dir_row", index)))
                                    .sense(egui::Sense::click_and_drag()),
                                |ui| self.dir_row(ui, index),
                            )
                            .response;
                        self.arm_drag(ui, row_hit.rect, index);
                        // 落点登记：整行按上、下两半区分给「插到这行之前 / 之后」
                        self.drop_zones
                            .extend(half_zones(row_hit.rect, ui.clip_rect(), index, false));
                        self.paint_insert_line(ui, row_hit.rect, index, false);
                        if row_hit.clicked() {
                            self.selected = Some(index);
                            self.confirm_remove = None;
                        }
                        if row_hit.double_clicked() {
                            self.open_folder(index);
                        }
                    }
                }
            });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;
    use crate::jobs::{Data, Kind};
    use crate::testbed::{base_input, stub_app, Stage};
    use egui::{Pos2, Rect, ThemePreference};

    /// 添加目录这条横栏里，输入框要和同一行的按钮一样高（他点名要的一致）：
    /// 量真正画出来的框——两个输入框 + 两枚按钮，四块框必须同高
    #[test]
    fn add_dir_bar_boxes_share_one_height() {
        let ctx = egui::Context::default();
        ctx.set_theme(ThemePreference::Dark);
        crate::fonts::install(&ctx, crate::fonts::AUTO);
        ctx.style_mut_of(egui::Theme::Dark, |style| {
            crate::ui::tune_style(style, crate::ui::ACCENT);
        });
        let mut app = crate::testbed::stub_app(false);
        let mut out = ctx.run_ui(base_input(), |ui| app.add_dir_bar(ui));
        out.textures_delta.clear();
        let heights: Vec<f32> = out
            .shapes
            .iter()
            .filter_map(|item| match &item.shape {
                egui::Shape::Rect(rect) => Some(rect.rect.height()),
                _ => None,
            })
            .collect();
        assert_eq!(heights.len(), 4, "该量到两个输入框和两枚按钮：{heights:?}");
        let tallest = heights.iter().cloned().fold(f32::MIN, f32::max);
        let shortest = heights.iter().cloned().fold(f32::MAX, f32::min);
        assert_eq!(shortest, tallest, "添加目录栏里的框高不齐：{heights:?}");
    }

    /// 工具条那行小字要和按钮排在同一条水平线上（他说的「对齐」是这个意思）
    #[test]
    fn toolbar_text_sits_on_the_button_line() {
        let mut stage = Stage::new(false);
        // 不套 tune_style 就复现不出来：按钮上下各 5 点的内边距正是把小字顶高的原因
        stage.ctx.style_mut_of(egui::Theme::Dark, |style| {
            crate::ui::tune_style(style, crate::ui::ACCENT)
        });
        let texts = stage.step(None);
        let title = texts
            .iter()
            .find(|(text, _)| text.starts_with("目录列表"))
            .map(|(_, rect)| *rect)
            .expect("工具条里应有「目录列表」这行小字");
        let button = Stage::rect_of(&texts, "全部检测").expect("工具条里应有「全部检测」按钮");
        assert!(
            (title.center().y - button.center().y).abs() < 1.5,
            "小字 {title:?} 没和按钮 {button:?} 对齐"
        );
    }

    #[test]
    fn switch_toggles_between_detailed_and_compact() {
        let _appdata = crate::testbed::lock_appdata();
        std::env::set_var(
            "APPDATA",
            std::env::temp_dir().join("repo_manager_home_style_test"),
        );
        let mut stage = Stage::new(false);
        let texts = stage.step(None);
        // 详细模式：整行的按钮都在，卡片的关键字一个都没有
        assert!(Stage::rect_of(&texts, "更新").is_some(), "默认应该是详细样式");
        assert!(Stage::rect_of(&texts, "+").is_none());

        // 切换器自带两格，点哪一格就切到哪一档
        let to_compact = Stage::rect_of(&texts, "简约").expect("切换器里应有「简约」那格");
        stage.step(Some(to_compact.center()));
        assert!(stage.app.cfg.home_compact, "点「简约」应该切成卡片样式");
        let texts = stage.step(None);
        assert!(Stage::rect_of(&texts, "更新").is_none(), "简约模式下六个按钮都收起来了");
        assert!(Stage::rect_of(&texts, "+").is_none(), "「+」按钮已移除，卡片上不该再有加号");

        let to_detail = Stage::rect_of(&texts, "详细").expect("切换器里应有「详细」那格");
        stage.step(Some(to_detail.center()));
        assert!(!stage.app.cfg.home_compact, "点「详细」应该切回整行样式");
        let saved = std::fs::read_to_string(config::config_file()).unwrap_or_default();
        assert!(saved.contains("home_compact"), "样式选择要写进配置文件");
    }

    /// 切换器是可点的，鼠标停上去要换成手型指针
    #[test]
    fn style_switch_points_with_a_hand_cursor() {
        let mut stage = Stage::new(false);
        let texts = stage.step(None);
        let cell = Stage::rect_of(&texts, "详细").expect("切换器的「详细」那一格");
        let Stage { ctx, app } = &mut stage;
        let mut input = base_input();
        input.events = vec![egui::Event::PointerMoved(cell.center())];
        let mut out = ctx.run_ui(input, |ui| app.main_page(ui));
        let cursor = out.platform_output.cursor_icon;
        out.textures_delta.clear();
        assert_eq!(cursor, egui::CursorIcon::PointingHand, "停在切换器上应该是手型指针");
    }

    /// Git 目录的详细行按 sha 口径显示：本地 / 远端都是 8 位短 sha，可更新按「个提交」计
    #[test]
    fn git_row_shows_sha_and_commit_unit() {
        let mut stage = Stage::new(false);
        stage.app.cfg.dirs.push(config::DirConfig {
            path: r"C:\wc\gitrepo".to_owned(),
            label: "GIT".to_owned(),
            ..Default::default()
        });
        stage.app.dirs.push(DirView {
            kind: crate::repo::RepoKind::Git,
            info: Some(crate::svn::WcInfo {
                is_wc: true,
                url: "https://github.com/demo/demo.git".to_owned(),
                repos_root: "https://github.com/demo/demo.git".to_owned(),
                revision: "a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0".to_owned(),
                branch: "git-backend".to_owned(),
                node_kind: "dir".to_owned(),
                last_rev: "a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0".to_owned(),
                last_author: "dev".to_owned(),
                last_date: "2026-10-10 18:00".to_owned(),
            }),
            remote: Some(true),
            remote_rev: "f0e1d2c3a4b5c6d7e8f9a0b1c2d3e4f5a6b7c8d9".to_owned(),
            out_of_date: Some(2),
            changed: Some(1),
            ..Default::default()
        });
        let texts = stage.step(None);
        for label in [
            "git-backend",
            "本地 a1b2c3d4",
            "远端 f0e1d2c3（可更新 2 个提交）",
            "待提交 1 项",
        ] {
            assert!(
                Stage::rect_of(&texts, label).is_some(),
                "Git 目录行上应该画得出「{label}」，实际有：{:?}",
                texts.iter().map(|(t, _)| t).collect::<Vec<_>>()
            );
        }
    }

    /// 卡片是手摆的块状布局，只有跑一帧才知道文字有没有叠在一起、计数有没有真画出来
    #[test]
    fn compact_card_paints_versions_and_counts() {
        let mut stage = Stage::new(true);
        let texts = stage.step(None);
        for label in [
            "HRP",
            "本地 r120",
            "远端 r128",
            "可更新 5",
            "待提交 3",
            // 无待更新项时本地版本号按远端显示
            "本地 r200",
            "远端 r200",
            "可更新 0",
            "待提交 0",
            // 没检测到信息时的占位；1287 项要压成 999+ 才不会把标记撑出卡片
            "本地 —",
            "远端 —",
            "待提交 999+",
            "empty",
        ] {
            assert!(
                Stage::rect_of(&texts, label).is_some(),
                "卡片上应该画得出「{label}」，实际有：{:?}",
                texts.iter().map(|(t, _)| t).collect::<Vec<_>>()
            );
        }
        // 名称超长时按矩形宽度截断。注意 galley 里存的仍是原始整串文字，
        // 只能量画出来的宽度：左列宽 162，扣掉状态灯和间距后名称剩 145 上下
        let long = "这是一个故意写得很长的目录别名用来验证截断";
        let wide = texts
            .iter()
            .find(|(text, _)| text == long)
            .map(|(_, rect)| rect.width())
            .expect("超长别名那一行总得画出来");
        assert!(
            wide <= 148.0,
            "超长别名没被截进名称区，实际画了 {wide:.0} 宽"
        );

        // 任何两段文字都不许互相压字（正方形卡片里手摆的块最容易挤在名称和标记之间）
        let mut collided: Vec<(&str, &str)> = Vec::new();
        for (i, (left_text, left_rect)) in texts.iter().enumerate() {
            for (right_text, right_rect) in texts.iter().skip(i + 1) {
                if left_rect.intersects(*right_rect) {
                    collided.push((left_text, right_text));
                }
            }
        }
        assert!(collided.is_empty(), "文字重叠：{collided:?}");

        // 「+」已移除，卡片上不该再有任何加号文字
        assert!(
            Stage::rect_of(&texts, "+").is_none(),
            "「+」按钮已移除，不该再画出加号"
        );
    }

    /// 形态要求逐项落地：名称带灯在左上、版本一行在左下、计数胶囊竖排贴右、右键出菜单
    #[test]
    fn compact_card_shape_and_corner_layout() {
        let mut stage = Stage::new(true);
        let Stage { ctx, app } = &mut stage;
        let mut out = ctx.run_ui(base_input(), |ui| app.main_page(ui));
        // 没有渲染器，字体增量不应用就得显式丢弃，否则 epaint 在 Drop 时 panic
        out.textures_delta.clear();
        let rects: Vec<(egui::Rect, u8, Color32, Color32)> = out
            .shapes
            .iter()
            .filter_map(|item| match &item.shape {
                egui::Shape::Rect(rect) => Some((
                    rect.rect,
                    rect.corner_radius.nw,
                    rect.fill,
                    rect.stroke.color,
                )),
                _ => None,
            })
            .collect();
        // 文字要连排版原点和落笔颜色一起收：`visual_bounding_rect` 量的是字形实际占的位子，
        // 不同字号的左右侧相机不一样，拿它比边线必然差出几像素
        let painted: Vec<(String, egui::Pos2, Color32)> = out
            .shapes
            .iter()
            .filter_map(|item| match &item.shape {
                egui::Shape::Text(text) => Some((
                    text.galley.text().to_string(),
                    text.pos,
                    text.galley
                        .job
                        .sections
                        .first()
                        .map(|section| section.format.color)
                        .unwrap_or(Color32::PLACEHOLDER),
                )),
                _ => None,
            })
            .collect();
        let found = |label: &str| {
            painted
                .iter()
                .find(|(text, _, _)| text == label)
                .unwrap_or_else(|| panic!("没画出「{label}」"))
        };
        // 落笔前 egui 会取整，位置比较一律留 0.6 的容差
        let near = |a: f32, b: f32| (a - b).abs() < 0.6;
        let warn = Color32::from_rgb(240, 190, 70);

        let cards: Vec<egui::Rect> = rects
            .iter()
            .filter(|(rect, radius, ..)| {
                (rect.width() - CARD_W).abs() < 0.6
                    && (rect.height() - CARD_H).abs() < 0.6
                    && *radius == 10
            })
            .map(|(rect, _, _, _)| *rect)
            .collect();
        assert_eq!(cards.len(), 12, "四张卡片各一个填充 + 一个描边 + 一层投影");
        let mut squares: Vec<egui::Rect> = cards.clone();
        squares.sort_by_key(|rect| rect.left() as i32);
        squares.dedup();
        assert_eq!(squares.len(), 4, "四张卡片");
        assert!(
            squares.windows(2).all(|pair| near(pair[0].top(), pair[1].top())),
            "卡片要排成同一横行，不能歪到下一行：{squares:?}"
        );
        // 「+」已移除：卡片上不该再有任何 30 见方的圆形按钮
        assert_eq!(
            rects
                .iter()
                .filter(|(rect, radius, ..)| {
                    (rect.width() - 30.0).abs() < 0.6
                        && (rect.height() - 30.0).abs() < 0.6
                        && *radius == 15
                })
                .count(),
            0,
            "「+」按钮已移除，不该再有圆钮"
        );

        // 第一张卡片：服务器有 5 项待更新、本地有 3 项待提交
        let card = squares[0];
        let pills: Vec<(egui::Rect, Color32)> = rects
            .iter()
            .filter(|(rect, radius, ..)| {
                *radius == 10
                    && (rect.height() - MARK_H).abs() < 1.0
                    && card.contains_rect(*rect)
            })
            .map(|(rect, _, fill, _)| (*rect, *fill))
            .collect();
        assert_eq!(pills.len(), 2, "卡片右侧应该有两枚计数胶囊");
        let (top, bottom) = if pills[0].0.top() <= pills[1].0.top() {
            (pills[0], pills[1])
        } else {
            (pills[1], pills[0])
        };
        assert!(top.0.bottom() <= bottom.0.top(), "两枚胶囊要一上一下，不能叠");
        assert!(
            near(top.0.right(), bottom.0.right()),
            "两枚胶囊右边缘要对齐：{} vs {}",
            top.0.right(),
            bottom.0.right()
        );
        assert!(
            near(card.max.x - top.0.right(), 10.0),
            "胶囊列贴着卡片右内边"
        );
        // 两枚都记着待办 → 各按自己的语义色出活跃淡底（待更新琥珀、待提交主色）
        assert_eq!(
            pills.iter().map(|(_, fill)| *fill).collect::<Vec<_>>(),
            vec![tint(crate::AMBER, 44), tint(crate::ACCENT, 44)],
            "有待办的标记按语义色填活跃淡底"
        );
        // 第二张卡片三个方向都是 0 → 整卡全干净，两枚一起转绿
        let quiet_pills: Vec<Color32> = rects
            .iter()
            .filter(|(rect, radius, ..)| {
                *radius == 10
                    && (rect.height() - MARK_H).abs() < 1.0
                    && squares[1].contains_rect(*rect)
            })
            .map(|(_, _, fill, _)| *fill)
            .collect();
        assert_eq!(
            quiet_pills,
            vec![tint(crate::OK, 44), tint(crate::OK, 44)],
            "全干净卡片的标记要一起转绿"
        );

        // 名称带状态灯在左上、版本一行在左下
        let texts = stage.step(None);
        let name = Stage::rect_of(&texts, "HRP").expect("名称");
        // 名称用的是字形紧贴的可见矩形，左右侧相机天然不完全对称，容差放宽到 2 像素
        let centered = |a: f32, b: f32| (a - b).abs() < 2.0;
        assert!(
            name.max.y <= card.min.y + 30.0,
            "名称要在卡片上部第一行，实际 {name:?}，卡片 {card:?}"
        );
        assert!(
            name.min.x >= card.min.x + 20.0,
            "名称要排在状态灯右边"
        );
        // 状态灯：排在名称行最前面，量可见矩形落在卡片左上角那一块里就行
        let lamp = Stage::rect_of(&texts, "●").expect("状态灯");
        assert!(
            lamp.max.x <= card.min.x + 40.0 && lamp.max.y <= card.min.y + 40.0,
            "状态灯要在卡片左上角，实际 {lamp:?}，卡片 {card:?}"
        );
        let local = found("本地 r120").1;
        let remote = found("远端 r128").1;
        assert!(
            centered(local.y, remote.y),
            "本地 / 远端版本要排在同一行：{} vs {}",
            local.y,
            remote.y
        );
        assert!(remote.x > local.x, "远端版本排在本地版本右边");
        assert!(near(local.x, card.min.x + 10.0), "版本行贴左内边");
        // 远端有更新 → 用详细模式里「远端 rX（可更新 N 项）」的那个黄色；没更新的卡片保持弱色
        assert_eq!(found("远端 r128").2, warn, "有可更新项时远端段要走警告色");
        assert_eq!(found("本地 r120").2, found("远端 r44").2, "没得更新时两段同为弱色");
        assert_ne!(found("远端 r44").2, warn, "另一张卡片没待更新项，不该跟着变黄");
    }

    /// 标记色块按标记文字收口。以前交给 `Frame` 画在竖排居中的子 Ui 里，成员被沿横轴
    /// 拉满整宽，块就成了一个和卡片等宽的空盒子，把名称的视觉重量全抢走了
    #[test]
    fn compact_tag_block_hugs_its_text() {
        let mut stage = Stage::new(true);
        stage.app.cfg.dirs[0].tag = "Java8".to_owned();
        stage.app.cfg.dirs[0].tag_color = "#B03A2E".to_owned();
        let Stage { ctx, app } = &mut stage;
        let mut out = ctx.run_ui(base_input(), |ui| app.main_page(ui));
        // 没有渲染器，字体增量不应用就得显式丢弃，否则 epaint 在 Drop 时 panic
        out.textures_delta.clear();
        let rects: Vec<(egui::Rect, u8)> = out
            .shapes
            .iter()
            .filter_map(|item| match &item.shape {
                egui::Shape::Rect(rect) => Some((rect.rect, rect.corner_radius.nw)),
                _ => None,
            })
            .collect();
        let pills: Vec<egui::Rect> = rects
            .iter()
            // 标记现在是实心色块（圆角 4）；计数胶囊圆角 10、卡片圆角 10，都不会撞上
            .filter(|(_, radius)| *radius == 4)
            .map(|(rect, _)| *rect)
            .collect();
        assert_eq!(pills.len(), 1, "只有挂了标记的那张卡片该有色块");
        let pill = pills[0];
        let texts = stage.step(None);
        let text = Stage::rect_of(&texts, "Java8").expect("标记文字要画出来");
        assert!(
            pill.width() - text.width() < 20.0,
            "胶囊要贴着标记文字：框 {} 宽，字 {} 宽",
            pill.width(),
            text.width()
        );
        let card = rects
            .iter()
            .find(|(rect, radius)| {
                *radius == 10
                    && (rect.width() - CARD_W).abs() < 0.6
                    && rect.contains_rect(pill)
            })
            .map(|(rect, _)| *rect)
            .expect("胶囊所在的那张卡片");
        // 近黑近白容差 2 像素：胶囊现在左对齐贴左内边，不再居中
        assert!(
            (card.min.x + 10.0 - pill.min.x).abs() < 2.0,
            "胶囊要贴着卡片左内边：卡片左 {}，胶囊左 {}",
            card.min.x,
            pill.min.x
        );
        assert!(pill.min.y > card.min.y + 20.0, "胶囊要在名称行下面");
        assert!(card.contains_rect(pill), "胶囊不许画出卡片");
        assert!(
            !Stage::rect_of(&texts, "HRP")
                .expect("卡片名称")
                .intersects(text),
            "胶囊压在名称上：名称 {}，标记 {}",
            Stage::rect_of(&texts, "HRP").unwrap(),
            text
        );
    }

    /// 计数标记是实心色块，深浅两套主题都得是同一块色，而且数字不能和底色撞在一起
    #[test]
    fn badge_colors_stay_readable_in_both_themes() {
        for theme in [ThemePreference::Dark, ThemePreference::Light] {
            let mut stage = Stage::new(true);
            stage.ctx.set_theme(theme);
            let Stage { ctx, app } = &mut stage;
            let mut out = ctx.run_ui(base_input(), |ui| app.main_page(ui));
            out.textures_delta.clear();
            let chips: Vec<(egui::Rect, Color32)> = out
                .shapes
                .iter()
                .filter_map(|item| match &item.shape {
                    egui::Shape::Rect(rect)
                        if rect.corner_radius.nw == 10
                            && (rect.rect.height() - MARK_H).abs() < 1.0 =>
                    {
                        Some((rect.rect, rect.fill))
                    }
                    _ => None,
                })
                .collect();
            assert_eq!(chips.len(), 8, "{theme:?}：四张卡片八枚标记");
            // 新配色：有待办的按各自语义色出淡底（深色主题半透明、浅色主题粉彩）；
            // 全干净卡片一起转绿；只有半干净的 0 胶囊才是中性灰（第三张卡 changed=1287）
            let (amber_active, accent_active, ok_active, quiet_fill) = match theme {
                ThemePreference::Dark | ThemePreference::System => (
                    tint(crate::AMBER, 44),
                    tint(crate::ACCENT, 44),
                    tint(crate::OK, 44),
                    Color32::from_white_alpha(16),
                ),
                ThemePreference::Light => (
                    pale(crate::AMBER, 0.16),
                    pale(crate::ACCENT, 0.16),
                    pale(crate::OK, 0.16),
                    Color32::from_rgb(243, 245, 250),
                ),
            };
            assert!(
                chips
                    .iter()
                    .all(|(_, fill)| [amber_active, accent_active, ok_active, quiet_fill].contains(fill)),
                "{theme:?}：标记底色只该是语义淡底或中性灰，实际 {chips:?}"
            );
            // 每张卡片右上角两枚：有待办的第一枚走琥珀淡底，全干净的第二张整对转绿
            assert_eq!(chips[0].1, amber_active, "{theme:?}:有待办的标记该填琥珀淡底");
            assert_eq!(chips[2].1, ok_active, "{theme:?}：全干净卡片的标记该转绿");
            // 数字压在底色上，两者相同就直接糊了
            let labels: Vec<(egui::Rect, Color32)> = out
                .shapes
                .iter()
                .filter_map(|item| match &item.shape {
                    egui::Shape::Text(shape) => Some((
                        item.shape.visual_bounding_rect(),
                        shape.galley.job.sections.first()?.format.color,
                    )),
                    _ => None,
                })
                .collect();
            for (rect, fill) in &chips {
                let digits: Vec<Color32> = labels
                    .iter()
                    .filter(|(bounds, _)| rect.contains_rect(*bounds))
                    .map(|(_, color)| *color)
                    .collect();
                assert!(!digits.is_empty(), "{theme:?}：标记 {rect:?} 里没有数字");
                assert!(
                    digits.iter().all(|color| color != fill),
                    "{theme:?}：数字颜色和底色相同，读不出来"
                );
            }
        }
    }

    /// 简约卡片跑任务时只多一枚转圈：地方小，再挤一行任务文字就把名称压掉了
    #[test]
    fn busy_card_shows_only_a_spinner() {
        let mut stage = Stage::new(true);
        let quiet = stage.step(None);
        stage.app.pool.spawn(Kind::Refresh, 0, "正在检测 …".to_owned(), |_| {
            Data::Run {
                dir: 0,
                ok: true,
                message: String::new(),
                reload: false,
            }
        });
        let texts = stage.step(None);
        assert!(
            Stage::rect_of(&texts, "正在检测 …").is_none(),
            "忙碌时不该再显示任务文字"
        );
        for label in ["HRP", "本地 r120", "远端 r128", "可更新 5", "待提交 3"] {
            assert!(
                Stage::rect_of(&texts, label).is_some(),
                "转圈不该挤掉卡片原有的「{label}」"
            );
        }
        let _ = quiet;
    }

    /// 右键卡片弹出一级菜单，悬停「更多」接出二级菜单：两级都要真能出来
    #[test]
    fn plus_menu_expands_two_levels() {
        let mut stage = Stage::new(true);
        let texts = stage.step(None);
        let card = Stage::rect_of(&texts, "HRP").expect("第一张卡片");
        stage.right_click(card.center());
        let texts = stage.step(None);
        for label in ["更新", "上传", "历史", "打开目录", "移除"] {
            assert!(Stage::rect_of(&texts, label).is_some(), "右键菜单里应有「{label}」");
        }
        // 二级菜单靠悬停展开（点「更多」反而会把整层菜单收掉）；
        // 子菜单按钮的文字里带着右箭头，只能按前缀找
        let more = texts
            .iter()
            .find(|(text, _)| text.starts_with("更多"))
            .map(|(_, rect)| *rect)
            .expect("一级菜单里应有「更多」");
        stage.hover(more.center());
        let texts = stage.step(None);
        assert!(
            Stage::rect_of(&texts, "▲ 上移").is_some(),
            "悬停「更多」应展开二级菜单，实际：{:?}",
            texts.iter().map(|(t, _)| t).collect::<Vec<_>>()
        );
    }

    /// 规则窗口的入口就在目录行的「更多」里，不必先进上传页：点它要按**这一行**开窗口，
    /// 并把这一行的差异筛选条件（配置里是分号串）拆成一行一条带进框里
    #[test]
    fn row_menu_opens_the_rules_window_for_that_directory() {
        let mut stage = Stage::new(false);
        stage.app.cfg.dirs[1].bc_filter = "-*.iml;-bin\\".to_owned();
        let texts = stage.step(None);
        // 每行目录一枚「更多」，第二枚就是 dirs[1]（CRM）
        let more = texts
            .iter()
            .filter(|(text, _)| text == "更多")
            .nth(1)
            .map(|(_, rect)| *rect)
            .expect("第二行目录应有「更多」");
        stage.step(Some(more.center()));
        let texts = stage.step(None);
        let item = Stage::rect_of(&texts, "差异过滤与提交屏蔽").unwrap_or_else(|| {
            panic!(
                "「更多」里应有规则入口，实际：{:?}",
                texts.iter().map(|(t, _)| t).collect::<Vec<_>>()
            )
        });
        let _ = stage.step(Some(item.center()));
        let rules = stage.app.commit_rules.as_ref().expect("点规则入口要打开窗口");
        assert_eq!(rules.dir, 1, "开的是这一行的规则");
        assert_eq!(rules.filter_buf, "-*.iml\n-bin\\", "分号串要拆成一行一条");
    }

    /// 「更多 → 双向合并（预览）」的启用条件：绑了本地开发侧、比出过差异、工作副本没有
    /// 待人工处理的条目、这一行没有写操作在跑，四样齐了才点得动；Git 目录里这条压根不画。
    /// 验法是点下去看窗口开没开——置灰的那几下点了不该拿错的现状算清单
    #[test]
    fn merge_entry_only_opens_when_the_directory_is_ready() {
        let _appdata = crate::testbed::lock_appdata();
        std::env::set_var(
            "APPDATA",
            std::env::temp_dir().join("repo_manager_merge_entry_test"),
        );
        fn summary(n: usize) -> crate::localdiff::Summary {
            crate::localdiff::Summary {
                differing: n,
                ..Default::default()
            }
        }
        fn conflict() -> crate::svn::StatusEntry {
            crate::svn::StatusEntry {
                path: "Build".to_owned(),
                name: "Build".to_owned(),
                item: crate::svn::Item::Conflict,
                props: String::new(),
                checked: false,
            }
        }
        /// 详细模式第一行的「更多」点开，返回那一帧的菜单
        fn open_menu(stage: &mut Stage) -> Vec<(String, egui::Rect)> {
            let texts = stage.step(None);
            let more = texts
                .iter()
                .filter(|(text, _)| text == "更多")
                .next()
                .map(|(_, rect)| *rect)
                .expect("第一行目录应有「更多」");
            stage.step(Some(more.center()));
            stage.step(None)
        }
        /// 菜单里那条双向合并：点一下，返回窗口开没开
        fn click_merge(stage: &mut Stage, texts: &[(String, egui::Rect)]) -> bool {
            let item = Stage::rect_of(texts, "双向合并（预览）")
                .expect("置灰也要把这条画出来，让「为什么点不动」看得见");
            stage.step(Some(item.center()));
            stage.app.merge.is_some()
        }
        fn unbound(app: &mut RepoApp) {
            app.dirs[0].local_diff = Some(summary(3));
        }
        fn no_diff(app: &mut RepoApp) {
            app.cfg.dirs[0].bc_target = r"D:\dev\hrp".to_owned();
            app.dirs[0].local_diff = Some(summary(0));
        }
        fn blocked(app: &mut RepoApp) {
            app.cfg.dirs[0].bc_target = r"D:\dev\hrp".to_owned();
            app.dirs[0].local_diff = Some(summary(3));
            app.dirs[0].blocked.push(conflict());
        }
        fn writing(app: &mut RepoApp) {
            app.cfg.dirs[0].bc_target = r"D:\dev\hrp".to_owned();
            app.dirs[0].local_diff = Some(summary(3));
            app.pool.spawn(Kind::ApplyMerge, 0, "正在写入 …".to_owned(), |_| {
                Data::Run {
                    dir: 0,
                    ok: true,
                    message: String::new(),
                    reload: false,
                }
            });
        }

        // 条件齐了点开就开窗，并且开的是这一行。
        let mut stage = Stage::new(false);
        stage.app.cfg.dirs[0].bc_target = r"D:\dev\hrp".to_owned();
        stage.app.dirs[0].local_diff = Some(summary(3));
        let texts = open_menu(&mut stage);
        let item = Stage::rect_of(&texts, "双向合并（预览）").unwrap_or_else(|| {
            panic!(
                "SVN 目录的「更多」里应有双向合并入口，实际：{:?}",
                texts.iter().map(|(t, _)| t).collect::<Vec<_>>()
            )
        });
        stage.step(Some(item.center()));
        assert_eq!(
            stage.app.merge.as_ref().map(|preview| preview.dir),
            Some(0),
            "条件齐了点开要开这一行的窗口"
        );

        let mut stage = Stage::new(false);
        no_diff(&mut stage.app);
        let texts = open_menu(&mut stage);
        assert!(click_merge(&mut stage, &texts), "两侧一致时也应能预览并建立同步状态");

        // 逐条拆掉一个条件：这条还画着，但点下去开不了窗
        for (name, setup) in [
            ("未绑定本地端", unbound as fn(&mut RepoApp)),
            ("有未处理条目", blocked as fn(&mut RepoApp)),
            ("有写操作在跑", writing as fn(&mut RepoApp)),
        ] {
            let mut stage = Stage::new(false);
            setup(&mut stage.app);
            let texts = open_menu(&mut stage);
            assert!(
                !click_merge(&mut stage, &texts),
                "「{name}」时这一条该是点不动的，实际却开了窗"
            );
        }

        // Git 目录：这一条不画，而不是画出来点不动
        let mut stage = Stage::new(false);
        stage.app.cfg.dirs[0].bc_target = r"D:\dev\hrp".to_owned();
        stage.app.dirs[0].local_diff = Some(summary(3));
        stage.app.dirs[0].kind = crate::repo::RepoKind::Git;
        let texts = open_menu(&mut stage);
        assert!(
            Stage::rect_of(&texts, "▲ 上移").is_some(),
            "菜单要确实展开，不然这条断言是空的"
        );
        assert!(
            Stage::rect_of(&texts, "双向合并（预览）").is_none(),
            "Git 目录要的是分支合并，菜单里不该出现这一条"
        );
    }

    /// 窗口认的是目录行下标，行序一变这个下标就指到别的目录上——「在 A 的规则框里改的却是 B」。
    /// 上移 / 下移是两行对调，窗口没有可靠的换算可做，收掉；拖动搬运和移除行本来就是
    /// 整段位移，跟 relocate / rollback 一样把下标换算过去或把越界的收掉
    #[test]
    fn rules_window_never_points_at_the_wrong_row() {
        let _appdata = crate::testbed::lock_appdata();
        let appdata = std::env::temp_dir().join("repo_manager_rules_index_test");
        let _ = std::fs::remove_dir_all(&appdata);
        std::env::set_var("APPDATA", &appdata);
        let mut stage = Stage::new(false);

        stage.app.commit_rules = Some(CommitRules::new(1, ""));
        stage.app.move_directory(1, -1);
        assert!(
            stage.app.commit_rules.is_none(),
            "窗口那一行跟别行对调过，留着就是拿下标改错的目录"
        );

        stage.app.commit_rules = Some(CommitRules::new(1, ""));
        stage.app.shift_directory(2, 0, None);
        assert_eq!(
            stage.app.commit_rules.as_ref().map(|d| d.dir),
            Some(2),
            "第 3 行插到最前，原来第 2 行的窗口该跟着右移一位"
        );

        stage.app.commit_rules = Some(CommitRules::new(1, ""));
        stage.app.remove_directory(0);
        assert!(
            stage.app.commit_rules.is_none(),
            "上面少了一行，窗口指着的已不是原来那个目录"
        );

        stage.app.commit_rules = Some(CommitRules::new(0, ""));
        stage.app.remove_directory(1);
        assert!(
            stage.app.commit_rules.is_some(),
            "删的是窗口下面的行，第 1 行没动，不该无端收窗"
        );
    }

    /// 绑定过本地端的卡片多一枚「差异」胶囊：它排在计数列的最下面，不能顶出卡片
    #[test]
    fn compact_card_gains_a_local_diff_pill() {
        let mut stage = Stage::new(true);
        stage.app.dirs[0].local_diff = Some(crate::localdiff::Summary {
            differing: 5,
            only_local: 2,
            only_server: 1,
            same: 300,
            samples: vec!["src/A.java".to_owned()],
            ..Default::default()
        });
        let texts = stage.step(None);
        let diff = Stage::rect_of(&texts, "差异 8").expect("绑定过本地端就该画出差异那枚");
        let update = Stage::rect_of(&texts, "可更新 5").expect("可更新那枚还得在");
        let commit = Stage::rect_of(&texts, "待提交 3").expect("待提交那枚还得在");
        // 竖排顺序：可更新 → 待提交 → 差异；三枚加间距 59，正好在 92 高的卡里垂直居中
        assert!(update.max.y <= commit.min.y, "待提交要排在可更新下面");
        assert!(commit.max.y <= diff.min.y, "差异要排在待提交下面");
        let name = Stage::rect_of(&texts, "HRP").expect("卡片名称");
        assert!(
            diff.min.x > name.max.x,
            "计数列要整个排在名称右边：{diff:?} vs {name:?}"
        );
        // 没绑定本地端的卡片不画这枚，更不能画一枚「差异 0」冒充「已经比过了」
        assert!(Stage::rect_of(&texts, "差异 0").is_none(), "没绑定本地端的卡片不该有这枚");
    }

    /// 详细行末尾那六个按钮在最小窗口宽度下只剩一百来像素余量，
    /// 「本地差异 N 项」整句写开之后既不能压住按钮，也不能被挤出屏幕
    #[test]
    fn detailed_row_local_mark_survives_the_narrowest_window() {
        let mut stage = Stage::new(false);
        stage.app.dirs[0].local_diff = Some(crate::localdiff::Summary {
            differing: 540,
            only_local: 6,
            ..Default::default()
        });
        let Stage { ctx, app } = &mut stage;
        let mut input = base_input();
        input.screen_rect = Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(900.0, 800.0)));
        let mut out = ctx.run_ui(input, |ui| app.main_page(ui));
        out.textures_delta.clear();
        let painted: Vec<(String, Rect)> = out
            .shapes
            .iter()
            .filter_map(|item| match &item.shape {
                egui::Shape::Text(text) => Some((
                    text.galley.text().to_string(),
                    item.shape.visual_bounding_rect(),
                )),
                _ => None,
            })
            .filter(|(text, _)| !text.trim().is_empty())
            .collect();
        let rect_of = |label: &str| {
            painted
                .iter()
                .find(|(text, _)| text == label)
                .map(|(_, rect)| *rect)
                .unwrap_or_else(|| panic!("900 宽的一帧里没画出「{label}」"))
        };
        let local = rect_of("本地差异 546 项");
        let upload = rect_of("上传");
        assert!(
            !local.intersects(upload),
            "本地差异标记压到了行尾按钮：{local:?} vs {upload:?}"
        );
        assert!(local.max.x <= 900.0, "本地差异标记被挤出了屏幕：{local:?}");
        // 整句写开之后还要确认它没被折到第二行去：别名和它在同一条水平线上
        let alias = rect_of("HRP");
        assert!(
            local.min.y < alias.max.y && local.max.y > alias.min.y,
            "本地差异标记掉到了第二行：{local:?} vs {alias:?}"
        );
    }

    /// 详细行的计数措辞：工作副本侧是「待提交 N 项」/「无待提交项」，本地侧比完没有差异就是
    /// 「本地无差异」，两组之间用一个中点隔开。只有「未核对」没比完时不能报无差异——
    /// 那样看着像已经确认过了
    #[test]
    fn detailed_row_counts_read_as_todo_and_local_difference() {
        let mut stage = Stage::new(false);
        stage.app.dirs[0].local_diff = Some(crate::localdiff::Summary {
            same: 120,
            ..Default::default()
        });
        stage.app.dirs[1].local_diff = Some(crate::localdiff::Summary::default());
        let texts: Vec<String> = stage.step(None).into_iter().map(|(text, _)| text).collect();
        let shows = |label: &str| texts.iter().any(|text| text == label);
        assert!(shows("待提交 3 项"), "第一行没写待提交：{texts:?}");
        assert!(shows("无待提交项"), "第二行没写无待提交项：{texts:?}");
        assert!(shows("本地无差异"), "比完没差异要写本地无差异：{texts:?}");
        assert!(shows("·"), "两组计数之间没有分隔中点：{texts:?}");
        assert!(!shows("本地差异 0 项"), "{texts:?}");

        // 只剩「没核对」的：总数是 0，但内容没读完就不能报无差异
        stage.app.dirs[0].local_diff = Some(crate::localdiff::Summary {
            unverified: 3,
            ..Default::default()
        });
        let second: Vec<String> = stage.step(None).into_iter().map(|(text, _)| text).collect();
        assert!(
            second.iter().any(|text| text == "本地差异 0 项"),
            "还有没核对的文件时不能报无差异：{second:?}"
        );
    }

    /// 收一帧里所有图元的包围盒，用来比对悬停前后有没有东西变形
    fn painted_bounds(stage: &mut Stage, hover: Option<Pos2>) -> Vec<Rect> {
        let mut input = base_input();
        if let Some(pos) = hover {
            input.events = vec![egui::Event::PointerMoved(pos)];
        }
        let Stage { ctx, app } = &mut *stage;
        let mut out = ctx.run_ui(input, |ui| app.main_page(ui));
        out.textures_delta.clear();
        out.shapes
            .iter()
            .map(|item| item.shape.visual_bounding_rect())
            .collect()
    }

    /// 鼠标移进移出都不许让卡片上的任何东西改尺寸：控件的三态 visuals（`Button` 的
    /// hovered / active 会换一套底色、描边、圆角）最容易干这件事，所以计数标记改成自画。
    #[test]
    fn hovering_changes_no_size() {
        let near = |a: f32, b: f32| (a - b).abs() < 0.6;
        for theme in [ThemePreference::Dark, ThemePreference::Light] {
            let mut stage = Stage::new(true);
            stage.ctx.set_theme(theme);
            let calm = painted_bounds(&mut stage, None);
            let chip = *calm
                .iter()
                .find(|rect| (rect.height() - MARK_H).abs() < 1.0 && rect.width() < PILL_W + 2.0)
                .expect("没画出计数标记");
            for (name, target) in [
                ("计数标记", chip.center()),
                ("卡片空白", chip.center() + egui::vec2(-40.0, 60.0)),
            ] {
                let hot = painted_bounds(&mut stage, Some(target));
                // 只认「左上角还是那个左上角、宽或高却变了」的图元；
                // 悬停新长出来的提示框是另一个位置，不会被算进来
                let jumps: Vec<(Rect, Rect)> = calm
                    .iter()
                    .flat_map(|before| {
                        hot.iter().filter_map(move |after| {
                            let same_place =
                                near(before.left(), after.left()) && near(before.top(), after.top());
                            let resized =
                                !near(before.width(), after.width())
                                    || !near(before.height(), after.height());
                            (same_place && resized).then(|| (*before, *after))
                        })
                    })
                    .collect();
                assert!(jumps.is_empty(), "{theme:?}：鼠标停在{name}上时尺寸变了 {jumps:?}");
            }
        }
    }

    /// 标记颜色的解析：合法 hex 原样取色，没存 / 坏值 / 长度不对都回退默认蓝；
    /// 取色器的往返是 u8 进、"#RRGGBB" 出
    #[test]
    fn tag_color_parsing_falls_back_to_default_blue() {
        let mut dir = config::DirConfig::default();
        assert_eq!(tag_color(&dir), Color32::from_rgb(90, 160, 240), "没存颜色按默认蓝");
        dir.tag_color = "#FF8040".to_owned();
        assert_eq!(tag_color(&dir), Color32::from_rgb(0xFF, 0x80, 0x40));
        dir.tag_color = "bad!!".to_owned();
        assert_eq!(tag_color(&dir), Color32::from_rgb(90, 160, 240), "坏值也按默认蓝");
        dir.tag_color = "#12345".to_owned();
        assert_eq!(tag_color(&dir), Color32::from_rgb(90, 160, 240), "长度不对也算坏值");
        dir.tag_color = "  #5AA0F0  ".to_owned();
        assert_eq!(tag_color(&dir), Color32::from_rgb(90, 160, 240), "两侧空白与 # 都要能容忍");
        assert_eq!(tag_color_hex([0x12, 0x34, 0xAB]), "#1234AB");
        assert_eq!(tag_color_rgb("#1234AB"), [0x12, 0x34, 0xAB]);
    }

    /// 详细模式：连续同组名共享一枚可折叠标题，收起后整段目录行都不画，
    /// 未分组的行不受影响
    #[test]
    fn group_header_renders_and_collapses_detailed_rows() {
        let mut stage = Stage::new(false);
        stage.app.cfg.dirs[0].group = "生产".to_owned();
        stage.app.cfg.dirs[1].group = "生产".to_owned();
        let texts = stage.step(None);
        assert!(
            Stage::rect_of(&texts, "▼ 生产（2）").is_some(),
            "展开状态该画「▼ 生产（2）」，实际有：{:?}",
            texts.iter().map(|(t, _)| t).collect::<Vec<_>>()
        );
        assert!(Stage::rect_of(&texts, "HRP").is_some(), "组里的行照常画");
        assert!(Stage::rect_of(&texts, "empty").is_some(), "未分组的行不受影响");

        stage.app.cfg.collapsed_groups = vec!["生产".to_owned()];
        let texts = stage.step(None);
        assert!(Stage::rect_of(&texts, "▶ 生产（2）").is_some(), "收起状态标题还在");
        assert!(Stage::rect_of(&texts, "HRP").is_none(), "收起的组整段不画");
        assert!(Stage::rect_of(&texts, "CRM").is_none(), "同组第二行也不画");
        assert!(Stage::rect_of(&texts, "empty").is_some(), "未分组的行照常画");
    }

    /// 点一下标题就收起、再点一下就展开，收起状态写进配置文件
    #[test]
    fn clicking_group_header_toggles_and_persists() {
        let _appdata = crate::testbed::lock_appdata();
        std::env::set_var(
            "APPDATA",
            std::env::temp_dir().join("repo_manager_group_toggle_test"),
        );
        let mut stage = Stage::new(false);
        stage.app.cfg.dirs[0].group = "生产".to_owned();
        stage.app.cfg.dirs[1].group = "生产".to_owned();
        let texts = stage.step(None);
        let header = Stage::rect_of(&texts, "▼ 生产（2）").expect("先画出展开的标题");
        stage.step(Some(header.center()));
        assert!(
            stage
                .app
                .cfg
                .collapsed_groups
                .iter()
                .any(|group| group == "生产"),
            "点一下该记进折叠列表"
        );
        let saved = std::fs::read_to_string(config::config_file()).unwrap_or_default();
        assert!(saved.contains("collapsed_groups"), "折叠状态要写进配置文件");
        let texts = stage.step(None);
        assert!(Stage::rect_of(&texts, "▶ 生产（2）").is_some(), "收起后标题换成 ▶");
        assert!(Stage::rect_of(&texts, "HRP").is_none(), "收起后行不画");

        let header = Stage::rect_of(&texts, "▶ 生产（2）").expect("收起状态的标题");
        stage.step(Some(header.center()));
        assert!(
            stage.app.cfg.collapsed_groups.is_empty(),
            "再点一下该从折叠列表里移除"
        );
        let texts = stage.step(None);
        assert!(Stage::rect_of(&texts, "HRP").is_some(), "展开后行回来了");
    }

    /// 标记胶囊：详细行画在路径后面，简约卡片画在名称正下方
    #[test]
    fn tag_pill_shows_in_detailed_row_and_compact_card() {
        let mut detailed = Stage::new(false);
        detailed.app.cfg.dirs[0].tag = "冻结".to_owned();
        let texts = detailed.step(None);
        assert!(
            Stage::rect_of(&texts, "冻结").is_some(),
            "详细行该画得出标记胶囊，实际有：{:?}",
            texts.iter().map(|(t, _)| t).collect::<Vec<_>>()
        );

        let mut compact = Stage::new(true);
        compact.app.cfg.dirs[0].tag = "冻结".to_owned();
        let texts = compact.step(None);
        assert!(
            Stage::rect_of(&texts, "冻结").is_some(),
            "卡片上该画得出标记胶囊，实际有：{:?}",
            texts.iter().map(|(t, _)| t).collect::<Vec<_>>()
        );
    }

    /// 简约形态：分组标题独占一整行，收起的组整段跳过
    #[test]
    fn compact_grid_gives_group_headers_their_own_row() {
        let mut stage = Stage::new(true);
        stage.app.cfg.dirs[0].group = "生产".to_owned();
        stage.app.cfg.dirs[1].group = "生产".to_owned();
        let texts = stage.step(None);
        assert!(
            Stage::rect_of(&texts, "▼ 生产（2）").is_some(),
            "卡片网格里也要有组标题，实际有：{:?}",
            texts.iter().map(|(t, _)| t).collect::<Vec<_>>()
        );
        stage.app.cfg.collapsed_groups = vec!["生产".to_owned()];
        let texts = stage.step(None);
        assert!(Stage::rect_of(&texts, "▶ 生产（2）").is_some());
        assert!(Stage::rect_of(&texts, "HRP").is_none(), "收起的组卡片不画");
        assert!(Stage::rect_of(&texts, "empty").is_some(), "未分组的卡片照常画");
    }

    /// 同名组散落在列表各处也只归并出一枚标题（显示层拉同组），未分组行插空照画；
    /// 组内行比未分组行整体右缩一截
    #[test]
    fn interleaved_same_name_groups_merge_into_one_header() {
        let mut stage = Stage::new(false);
        // dirs[0]=HRP（生产） dirs[1]=CRM（未分组） dirs[2]=empty（生产）
        stage.app.cfg.dirs[0].group = "生产".to_owned();
        stage.app.cfg.dirs[2].group = "生产".to_owned();
        let texts = stage.step(None);
        let headers = texts
            .iter()
            .filter(|(text, _)| text == "▼ 生产（2）")
            .count();
        assert_eq!(
            headers, 1,
            "同名组只该有一枚标题，实际有：{:?}",
            texts.iter().map(|(t, _)| t).collect::<Vec<_>>()
        );
        let hrp = Stage::rect_of(&texts, "HRP").expect("HRP 行");
        let empty = Stage::rect_of(&texts, "empty").expect("empty 行");
        let crm = Stage::rect_of(&texts, "CRM").expect("CRM 行");
        assert!(
            hrp.top() < empty.top() && empty.top() < crm.top(),
            "拉同组后该按 HRP → empty → CRM 显示：HRP={hrp:?} empty={empty:?} CRM={crm:?}"
        );
        assert!(
            hrp.left() - crm.left() >= 8.0,
            "组内行要比未分组行右缩一截：HRP={:?} CRM={crm:?}",
            hrp.left()
        );
    }

    /// 拉同组之后配置顺序 ≠ 显示顺序，上移/下移要按看到的顺序换邻居
    #[test]
    fn move_follows_display_order() {
        let _appdata = crate::testbed::lock_appdata();
        std::env::set_var(
            "APPDATA",
            std::env::temp_dir().join("repo_manager_move_display_test"),
        );
        let mut stage = Stage::new(false);
        // dirs[0]=HRP（旧） dirs[1]=CRM（未分组） dirs[2]=empty（旧）→ 显示 HRP → empty → CRM
        stage.app.cfg.dirs[0].group = "旧".to_owned();
        stage.app.cfg.dirs[2].group = "旧".to_owned();
        let texts = stage.step(None);
        let hrp = Stage::rect_of(&texts, "HRP").expect("HRP 行");
        let empty = Stage::rect_of(&texts, "empty").expect("empty 行");
        let crm = Stage::rect_of(&texts, "CRM").expect("CRM 行");
        assert!(
            hrp.top() < empty.top() && empty.top() < crm.top(),
            "先确认显示顺序是 HRP → empty → CRM"
        );

        // empty（配置里的第 3 行）在显示上紧挨着 HRP：上移该跟 HRP 换位，而不是没反应
        stage.app.move_directory(2, -1);
        assert_eq!(stage.app.selected, Some(0), "选中该跟着换过去的邻居走");
        let texts = stage.step(None);
        let hrp = Stage::rect_of(&texts, "HRP").expect("HRP 行");
        let empty = Stage::rect_of(&texts, "empty").expect("empty 行");
        let crm = Stage::rect_of(&texts, "CRM").expect("CRM 行");
        assert!(
            empty.top() < hrp.top() && hrp.top() < crm.top(),
            "上移后 empty 应排到 HRP 前面：HRP={hrp:?} empty={empty:?} CRM={crm:?}"
        );
    }

    /// 走一遍完整手势：按下 → 挪出 6 像素 → 移到落点 → 松手。
    /// 无头跑帧的 predicted_dt 是 0，i.time 不会自己走，所以每帧把时刻显式喂进去：
    /// 「按住够 1 秒」这条判据只有推了时间才测得到
    fn drag_gesture(stage: &mut Stage, from: Pos2, to: Pos2, hold: f64) {
        let press = |pos: Pos2, pressed: bool| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        stage.timed(0.0, vec![egui::Event::PointerMoved(from), press(from, true)]);
        stage.timed(
            hold * 0.2,
            vec![egui::Event::PointerMoved(from + Vec2::new(0.0, 12.0))],
        );
        stage.timed(hold * 0.9, vec![egui::Event::PointerMoved(to)]);
        stage.timed(hold, vec![egui::Event::PointerMoved(to), press(to, false)]);
    }

    /// 从上一帧登记的落点里取一块的中心；没登记就直接失败，免得测试悄悄跑空
    fn zone_center(app: &RepoApp, want: &DropZone) -> Pos2 {
        app.drop_zones
            .iter()
            .find(|(zone, _)| zone == want)
            .map(|(_, rect)| rect.center())
            .unwrap_or_else(|| panic!("没有登记落点 {want:?}，实际有 {:?}", app.drop_zones))
    }

    /// 详细模式：把 HRP 拖到 CRM 的下半区 = 插到 CRM 之后。
    /// HRP 本来在「旧」组里、CRM 没分组，所以这一次落点同时把它移出分组
    #[test]
    fn dragging_a_row_below_another_reorders_and_adopts_its_group() {
        let _appdata = crate::testbed::lock_appdata();
        std::env::set_var(
            "APPDATA",
            std::env::temp_dir().join("repo_manager_drag_row_test"),
        );
        let mut stage = Stage::new(false);
        stage.app.cfg.dirs[0].group = "旧".to_owned();
        stage.run(vec![]);
        let from = zone_center(&stage.app, &DropZone::Row(0, Edge::Before));
        let to = zone_center(&stage.app, &DropZone::Row(1, Edge::After));
        drag_gesture(&mut stage, from, to, 1.2);

        let paths: Vec<String> = stage.app.cfg.dirs.iter().map(|d| d.path.clone()).collect();
        assert_eq!(
            paths,
            vec![r"C:\wc\crm", r"C:\wc\hrp", r"C:\wc\empty", r"C:\wc\long"],
            "该把 HRP 插到 CRM 之后"
        );
        assert!(stage.app.cfg.dirs[1].group.is_empty(), "落到未分组的 CRM 后面就该移出分组");
        assert_eq!(stage.app.selected, Some(1), "选中要跟着搬走的那行");
        assert_eq!(stage.app.dirs[0].remote_rev, "200", "视图状态没跟着行走");
        assert_eq!(stage.app.dirs[1].remote_rev, "128", "视图状态没跟着行走");
        assert!(stage.app.drag.is_none(), "松手之后拖动状态要清掉");
        let tail = stage.app.log_tail();
        assert!(tail.contains("移出分组"), "要在日志里说清动了什么：{tail}");
    }

    /// 拖到分组标题条上 = 并入那一组、排到同组末尾；收着的组接住落点要顺手展开，
    /// 否则刚拖进去的行看不见，像是拖失败了
    #[test]
    fn dragging_a_row_onto_a_group_header_joins_and_expands_it() {
        let _appdata = crate::testbed::lock_appdata();
        std::env::set_var(
            "APPDATA",
            std::env::temp_dir().join("repo_manager_drag_header_test"),
        );
        let mut stage = Stage::new(false);
        stage.app.cfg.dirs[1].group = "后端".to_owned();
        stage.app.cfg.dirs[2].group = "后端".to_owned();
        stage.app.cfg.collapsed_groups = vec!["后端".to_owned()];
        stage.run(vec![]);
        let from = zone_center(&stage.app, &DropZone::Row(0, Edge::Before));
        let to = zone_center(&stage.app, &DropZone::Header("后端".to_owned()));
        drag_gesture(&mut stage, from, to, 1.2);

        let paths: Vec<String> = stage.app.cfg.dirs.iter().map(|d| d.path.clone()).collect();
        assert_eq!(
            paths,
            vec![r"C:\wc\crm", r"C:\wc\empty", r"C:\wc\hrp", r"C:\wc\long"],
            "并入的组要排在同组末尾"
        );
        assert_eq!(stage.app.cfg.dirs[2].group, "后端", "分组要跟着落点改");
        assert!(stage.app.cfg.collapsed_groups.is_empty(), "落进收起的组该把它展开");
        let texts = stage.step(None);
        assert!(Stage::rect_of(&texts, "HRP").is_some(), "展开后这一行要看得见");
        let tail = stage.app.log_tail();
        assert!(tail.contains("并入分组"), "要在日志里说清动了什么：{tail}");
    }

    /// 简约形态一样能拖：卡片按左、右半区判定，把第一张拖到第二张的右半边 = 排到它后面
    #[test]
    fn dragging_a_card_to_the_right_of_another_reorders_them() {
        let _appdata = crate::testbed::lock_appdata();
        std::env::set_var(
            "APPDATA",
            std::env::temp_dir().join("repo_manager_drag_card_test"),
        );
        let mut stage = Stage::new(true);
        stage.run(vec![]);
        let from = zone_center(&stage.app, &DropZone::Row(0, Edge::Before));
        let to = zone_center(&stage.app, &DropZone::Row(1, Edge::After));
        drag_gesture(&mut stage, from, to, 1.2);

        let paths: Vec<String> = stage.app.cfg.dirs.iter().map(|d| d.path.clone()).collect();
        assert_eq!(
            paths,
            vec![r"C:\wc\crm", r"C:\wc\hrp", r"C:\wc\empty", r"C:\wc\long"],
            "卡片拖到右半边该换到它后面"
        );
    }

    /// 没按住 1 秒就松手 = 只当点了一下：顺序、分组都不该动
    #[test]
    fn releasing_before_the_hold_is_only_a_click() {
        let _appdata = crate::testbed::lock_appdata();
        std::env::set_var(
            "APPDATA",
            std::env::temp_dir().join("repo_manager_drag_short_test"),
        );
        let mut stage = Stage::new(false);
        stage.run(vec![]);
        let from = zone_center(&stage.app, &DropZone::Row(0, Edge::Before));
        let to = zone_center(&stage.app, &DropZone::Row(1, Edge::After));
        drag_gesture(&mut stage, from, to, 0.4);

        let paths: Vec<String> = stage.app.cfg.dirs.iter().map(|d| d.path.clone()).collect();
        assert_eq!(
            paths,
            vec![r"C:\wc\hrp", r"C:\wc\crm", r"C:\wc\empty", r"C:\wc\long"],
            "按住不够久不该真的拖动"
        );
        assert!(stage.app.drag.is_none(), "拖动状态没起来");
        assert!(stage.app.drag_pending.is_none(), "松手要把按下记录清掉");
    }

    /// 弹层压在卡片身上时长按，不该把底下那张卡片拽走（他报的「在弹出窗口的地方长按会触发目录拖动」）
    #[test]
    fn holding_inside_a_card_menu_does_not_drag_the_card_behind_it() {
        let _appdata = crate::testbed::lock_appdata();
        std::env::set_var(
            "APPDATA",
            std::env::temp_dir().join("repo_manager_drag_popup_test"),
        );
        let mut stage = Stage::new(true);
        // 卡片网格一行七张，多加四个把网格顶出第二行：弹层才会压在卡片身上
        for index in 0..4 {
            stage.app.cfg.dirs.push(config::DirConfig {
                path: format!(r"C:\wc\extra{index}"),
                label: format!("补{index}"),
                ..Default::default()
            });
            stage.app.dirs.push(DirView::default());
        }
        let texts = stage.run(vec![]);
        let card = Stage::rect_of(&texts, "HRP").expect("第一张卡片");
        stage.right_click(card.center());
        // 弹层要第二帧才画得出来
        let texts = stage.step(None);
        // 按第三项「历史」：宽行卡片一行放五张，一级菜单的第三项纵向正好压在
        // 第二行那张卡片上（第二项「上传」会落进两行之间的缝隙，这一测就空跑了）
        let item = Stage::rect_of(&texts, "历史").expect("卡片菜单里的「历史」");
        assert!(
            stage
                .app
                .drop_zones
                .iter()
                .any(|(_, zone)| zone.contains(item.center())),
            "菜单项 {item:?} 没压在任何一张卡片上，这一测等于空跑"
        );

        let press = |pressed: bool| egui::Event::PointerButton {
            pos: item.center(),
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        stage.timed(0.0, vec![egui::Event::PointerMoved(item.center()), press(true)]);
        stage.timed(1.2, vec![egui::Event::PointerMoved(item.center())]);
        // 只按不放：松手会真点了这一项，而起不起拖在两帧里就看得出来
        assert!(stage.app.drag_pending.is_none(), "按在弹层上不该登记拖动按下");
        assert!(stage.app.drag.is_none(), "按在弹层上满 1 秒也不该起拖");
    }

    /// 在用分组 / 标记的取值口径：去重、排序、排除自己当前的那份；
    /// 标记取首次出现时的颜色，套用旧标记时颜色能一并沿用
    #[test]
    fn existing_groups_and_tags_dedupe_and_exclude() {
        let mut app = stub_app(false);
        app.cfg.dirs[0].group = "生产".to_owned();
        app.cfg.dirs[0].tag = "核心".to_owned();
        app.cfg.dirs[0].tag_color = "#FF0000".to_owned();
        app.cfg.dirs[1].group = "生产".to_owned();
        app.cfg.dirs[1].tag = "备份".to_owned();
        app.cfg.dirs[2].group = "测试".to_owned();

        assert_eq!(app.existing_groups("生产"), vec!["测试".to_owned()]);
        assert_eq!(
            app.existing_groups(""),
            vec!["测试".to_owned(), "生产".to_owned()]
        );
        // 标记按名字去重，颜色取首次出现那份
        let tags = app.existing_tags("");
        assert_eq!(
            tags,
            vec![
                ("备份".to_owned(), String::new()),
                ("核心".to_owned(), "#FF0000".to_owned()),
            ]
        );
        // 排除掉自己正在编辑的标记，列表里就不再出现「点了也是白点」的那项
        assert!(app.existing_tags("核心").iter().all(|(name, _)| name != "核心"));
    }

    /// 分组编辑行打开时输入框下方挂着在用分组名，点一下直接套上（不用再手敲）
    #[test]
    fn group_editor_suggestion_applies_on_click() {
        let _appdata = crate::testbed::lock_appdata();
        std::env::set_var(
            "APPDATA",
            std::env::temp_dir().join("repo_manager_group_suggest_test"),
        );
        let mut stage = Stage::new(true);
        stage.app.cfg.dirs[1].group = "生产".to_owned();
        // 直接开 HRP 的分组编辑行：真实入口在右键菜单，菜单链路另有测试覆盖
        stage.app.edit_group = Some(0);
        let mut texts = stage.step(None);
        // 弹层第一帧是 sizing pass，形状可能不出，再稳一帧
        texts = stage.step(None);
        let chip = Stage::rect_of(&texts, "生产")
            .unwrap_or_else(|| panic!("编辑行下方应挂在用分组名「生产」：{texts:?}"));
        stage.step(Some(chip.center()));
        assert_eq!(
            stage.app.cfg.dirs[0].group, "生产",
            "点在用分组要直接套上"
        );
        assert!(stage.app.edit_group.is_none(), "套用后编辑行要关掉");
    }

    /// 同一个选中态在两种样式里要叠出同一个颜色：选中色和卡片那圈投影都是半透明的，
    /// 不先垫一层不透光的底，卡片就被投影拽暗，和详细模式的选中行看着就不像一个东西
    #[test]
    fn selected_card_and_row_end_up_the_same_color() {
        /// 选中第一个目录，把穿过名称左侧同一处的每一层底色按画序取出来，最前面补上面板底。
        /// 主题要连着 tune_style 一起换：`Stage::new` 只 set_theme，选中色停在 egui 出厂的
        /// 不透明值上，叠在投影里的差就复现不出来
        fn stacked(compact: bool, theme: egui::Theme) -> Vec<Color32> {
            let mut stage = Stage::new(compact);
            stage.ctx.set_theme(theme);
            let accent = stage.app.accent();
            stage
                .ctx
                .style_mut_of(theme, |style| crate::ui::tune_style(style, accent));
            let Stage { ctx, app } = &mut stage;
            let mut out = ctx.run_ui(base_input(), |ui| app.main_page(ui));
            out.textures_delta.clear();
            let texts: Vec<(String, Rect)> = out
                .shapes
                .iter()
                .filter_map(|item| match &item.shape {
                    egui::Shape::Text(t) => {
                        Some((t.galley.text().to_string(), item.shape.visual_bounding_rect()))
                    }
                    _ => None,
                })
                .collect();
            let name = Stage::rect_of(&texts, "HRP").expect("卡片名称");
            let at = name.center() - egui::vec2(name.width() / 2.0 + 12.0, 0.0);
            app.selected = Some(0);
            let mut panels = None;
            let mut out = ctx.run_ui(base_input(), |ui| {
                panels = Some((ui.visuals().panel_fill, ui.visuals().selection.bg_fill));
                app.main_page(ui);
            });
            out.textures_delta.clear();
            let (panel, selected) = panels.expect("视觉");
            let mut layers: Vec<Color32> = out
                .shapes
                .iter()
                .filter_map(|item| match &item.shape {
                    egui::Shape::Rect(r)
                        if r.rect.contains(at)
                            && r.rect.height() > 20.0
                            && r.fill != Color32::TRANSPARENT =>
                    {
                        Some(r.fill)
                    }
                    _ => None,
                })
                .collect();
            assert_eq!(
                *layers.last().expect("这一处没垫任何底色"),
                selected,
                "最上面那层要就是选中色"
            );
            // 面板底是这一切的最下面那层
            layers.insert(0, panel);
            layers
        }

        /// 从底往上按 over 合一遍。Color32 在 egui 里按预乘存放，通道要先除回 alpha
        fn composite(layers: &[Color32]) -> Color32 {
            let [r, g, b, _] = layers[0].to_array();
            let (mut ar, mut ag, mut ab) = (r as f32, g as f32, b as f32);
            for c in &layers[1..] {
                let [sr, sg, sb, sa] = c.to_array();
                let a = sa as f32 / 255.0;
                let (ur, ug, ub) = if a > 0.0 {
                    (sr as f32 / a, sg as f32 / a, sb as f32 / a)
                } else {
                    (0.0, 0.0, 0.0)
                };
                ar = ur * a + ar * (1.0 - a);
                ag = ug * a + ag * (1.0 - a);
                ab = ub * a + ab * (1.0 - a);
            }
            Color32::from_rgb(ar as u8, ag as u8, ab as u8)
        }

        for theme in [egui::Theme::Dark, egui::Theme::Light] {
            let row = composite(&stacked(false, theme));
            let card = composite(&stacked(true, theme));
            assert_eq!(row, card, "{theme:?}：选中行的 {row:?} 对上选中卡片的 {card:?}");
        }
    }
}
