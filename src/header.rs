//! 顶部状态栏（自绘标题栏）：仓库连接状态、图标按钮与窗口控制。
//!
//! 系统标题栏已由 titlebar.rs 在原生层抹掉，这一整条就是标题栏：
//! * 空白天区可拖动 / 双击最大化 / 贴边分屏——每帧把「天区到哪、按钮区从哪开始」
//!   写给 titlebar.rs，原生的 WM_NCHITTEST 据此区分 拖动 / 缩放 / 按钮点击；
//! * 右上角一排按钮全部换成 SVG 图标（icons.rs 启动时栅格化），文字说明进悬停提示；
//! * 最右是窗口控制三键：最小化 / 最大化还原 / 关闭，关闭键悬停变红（Windows 惯例）。

use crate::icons;
use crate::repo::RepoKind;
use crate::{APP_TITLE, DirView, RepoApp};
use egui::{Align, Color32, Layout, RichText, Ui, Vec2};
use crate::jobs::Kind;
use crate::ui::AlignedRow;

/// 图标按钮的尺寸：工具键近方形；窗口控制键略宽（Windows 的惯例比例）
const TOOL_SIZE: Vec2 = Vec2::new(30.0, 26.0);
const CAPTION_SIZE: Vec2 = Vec2::new(34.0, 26.0);

impl RepoApp {
    // ------------------------------------------------------------ 顶部状态栏

    /// 连接状态的三元组：颜色 / 一句话 / 悬停原因。顶栏不再展示它，
    /// 由 app.rs 的输出面板顶行显示（所以开放给同 crate 的 app 模块调用）
    pub(crate) fn connection(&self) -> (Color32, String, String) {
        let green = crate::ui::OK;
        let amber = crate::ui::AMBER;
        let red = crate::ui::ERR;
        let gray = Color32::from_gray(150);
        if self.probing || self.pool.has(Kind::DetectSvn, usize::MAX) {
            return (amber, "正在寻找 svn.exe".into(), "自动探测 SVN 命令行工具".into());
        }
        if !self.svn.available() {
            return (
                red,
                "未找到 svn.exe".into(),
                format!("路径无效：{}", self.svn.exe.display()),
            );
        }
        if self.cfg.dirs.is_empty() {
            return (
                gray,
                format!("svn {} 可用，尚未添加目录", self.version.clone().unwrap_or_default()),
                "添加工作副本目录后会自动检测连接".into(),
            );
        }
        let total = self.cfg.dirs.len();
        let checked: Vec<&DirView> = self.dirs.iter().filter(|d| d.remote.is_some()).collect();
        if checked.is_empty() {
            return (gray, "等待检测".into(), "尚未完成连接检测".into());
        }
        let ok = checked.iter().filter(|d| d.remote == Some(true)).count();
        // 后端字样：列表里只有一种仓库时带名字（SVN / Git），混着就不细分
        let git = checked.iter().filter(|d| d.kind == RepoKind::Git).count();
        let backend = match (git, checked.len() - git) {
            (0, _) => " SVN",
            (_, 0) => " Git",
            _ => "",
        };
        if ok == total {
            (green, format!("已连接{backend}（{ok}/{total}）"), "全部目录均可访问仓库".into())
        } else if ok == 0 {
            let reason = checked
                .iter()
                .map(|d| d.remote_msg.as_str())
                .find(|m| !m.is_empty())
                .unwrap_or("无法访问仓库")
                .to_owned();
            (red, format!("未连接{backend}（0/{total}）"), reason)
        } else {
            (amber, format!("部分连接（{ok}/{total}）"), "存在无法访问的目录".into())
        }
    }

    pub(crate) fn header(&mut self, ui: &mut Ui) {
        let icons = self.icons.clone();
        // 整条顶栏就是标题栏：高度定成 40，比普通控件行高一档才有标题栏的样子
        ui.set_min_height(40.0);
        // 内容在 40 高的栏里垂直居中：不垫这一段，行会贴着窗口上沿，
        // 底下空出一大截，看着就像工具栏和添加目录栏之间隔了一条缝
        let top_pad = ((40.0 - crate::ui::control_height(ui)) / 2.0).max(0.0);
        ui.add_space(top_pad);
        // 右侧按钮区的左缘（逻辑点）：每放一枚按钮就往左推一次，最后整批写给原生侧
        let mut buttons_left = f32::INFINITY;
        ui.aligned_row(|ui| {
            // 不贴窗口左缘：面板自带 10pt 边距，这里再让出一点呼吸位
            ui.add_space(4.0);
            // 软件标志：与窗口图标同一来源的兔子图，解不出来就只留标题
            if let Some(tex) = self.logo.as_ref() {
                let h = 18.0;
                let w = h * tex.aspect_ratio();
                let (rect, _) =
                    ui.allocate_exact_size(Vec2::new(w, h), egui::Sense::hover());
                ui.painter().image(
                    tex.id(),
                    rect,
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    Color32::WHITE,
                );
            }
            ui.label(RichText::new(APP_TITLE).size(16.0).strong());
            // 连接状态搬去了日志面板顶行的中间，这里直接接右侧按钮区
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let Some(set) = icons.as_ref() else {
                    // 无头测试没有贴图：按钮区整个跳过（工具键与窗口控制全在这里）
                    return;
                };

                // ---- 窗口控制（最右）：最小化 / 最大化还原 / 关闭
                let close = icons::button(ui, &set.close, "关闭", CAPTION_SIZE, true, None);
                buttons_left = buttons_left.min(close.rect.min.x);
                if close.clicked() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
                let maximized = ui.ctx().input(|i| i.viewport().maximized) == Some(true);
                let (max_icon, max_tip) = if maximized {
                    (&set.restore, "向下还原")
                } else {
                    (&set.maximize, "最大化")
                };
                let max = icons::button(ui, max_icon, max_tip, CAPTION_SIZE, false, None);
                buttons_left = buttons_left.min(max.rect.min.x);
                if max.clicked() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
                }
                let min = icons::button(ui, &set.minimize, "最小化", CAPTION_SIZE, false, None);
                buttons_left = buttons_left.min(min.rect.min.x);
                if min.clicked() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                }
                // 工具键与窗口控制键之间只留一段空隙，不画分隔线
                ui.add_space(14.0);

                // ---- 检查到新版本：更新图标常驻入口，整枚图标染上随时间流动的彩虹色相
                // （约 4 秒转一圈；有没有新版由 update_ready 定，后台检查那趟按文件
                // sha256 算好的结论：版本号没动但换了构建时，这里照样亮起）。
                // 深色主题用亮色，浅色主题（白底）压暗到一半亮度，否则会看不清
                if self.update_ready {
                    let latest = self
                        .update_info
                        .as_ref()
                        .map(|m| m.version.trim().to_owned())
                        .unwrap_or_default();
                    let base = (ui.input(|i| i.time) / 4.0) % 1.0;
                    let (sat, val) = if ui.visuals().dark_mode { (0.92, 0.85) } else { (0.95, 0.55) };
                    let tint = egui::Color32::from(egui::ecolor::Hsva::new(
                        base as f32,
                        sat,
                        val,
                        1.0,
                    ));
                    ui.ctx().request_repaint_after(std::time::Duration::from_millis(66));
                    let upd = icons::button(
                        ui,
                        &set.update,
                        &format!(
                            "新版本 V{latest}：点击确认后直接开始（下载新版本并覆盖重启）"
                        ),
                        TOOL_SIZE,
                        false,
                        Some(tint),
                    );
                    buttons_left = buttons_left.min(upd.rect.min.x);
                    if upd.clicked() {
                        self.show_update_confirm = true;
                    }
                }

                // ---- 工具键：SVG 图标 + 悬停提示（原来的文字全搬进提示里）
                let feedback = icons::button(
                    ui,
                    &set.feedback,
                    "在 GitHub 上为本项目提交 issue：反馈问题或提出功能建议",
                    TOOL_SIZE,
                    false,
                    None,
                );
                buttons_left = buttons_left.min(feedback.rect.min.x);
                if feedback.clicked() {
                    ui.ctx().open_url(egui::OpenUrl::same_tab(format!(
                        "{}/issues",
                        crate::update::OFFICIAL_PAGE
                    )));
                    self.hint("已在默认浏览器打开 GitHub 的 issue 页面");
                }

                // 插件没装时整块不画：日志模式、勾选框、窗口都跟着消失
                if self.plugin_installed(crate::plugins::PluginId::AiLog) {
                    // AI 日志：开关式入口。开着时图标常亮主色；数量进悬停提示。
                    // 第一次点开启「日志模式」（各目录提交记录页里本人提交的版本行出现勾选框）；
                    // 开着但没勾选时再点退出；有勾选后点击弹出「AI 工作日志」窗口（模式保持
                    // 开启，方便关掉窗口后去别的目录继续勾）。
                    let picks = self.ai_picks.len();
                    let (ai_hover, ai_active) = if !self.ai_mode {
                        (
                            "开启日志模式：各目录「提交记录」页中本人提交的版本行将出现勾选框，\n\
                             勾选后本按钮亮起，点击弹出「AI 工作日志」窗口"
                                .to_owned(),
                            false,
                        )
                    } else if picks == 0 {
                        (
                            "已开启日志模式：提交记录页显示勾选框（已勾选的提交将保留），点击退出"
                                .to_owned(),
                            true,
                        )
                    } else {
                        (
                            format!(
                                "打开「AI 工作日志」窗口：将已勾选的 {picks} 条提交交由 AI，\
                                 按「口吻」整理为工作日志（模式保持开启，可继续在其他目录勾选）"
                            ),
                            true,
                        )
                    };
                    let ai_tint = ai_active.then_some(ui.visuals().hyperlink_color);
                    let ai = icons::button(ui, &set.ai_log, &ai_hover, TOOL_SIZE, false, ai_tint);
                    buttons_left = buttons_left.min(ai.rect.min.x);
                    if ai.clicked() {
                        if !self.ai_mode {
                            self.ai_mode = true;
                        } else if picks == 0 {
                            self.ai_mode = false;
                        } else {
                            self.show_worklog = true;
                        }
                    }
                }

                let compare = icons::button(
                    ui,
                    &set.compare,
                    "启动 Beyond Compare",
                    TOOL_SIZE,
                    false,
                    None,
                );
                buttons_left = buttons_left.min(compare.rect.min.x);
                if compare.clicked() {
                    self.open_bcompare_matched();
                }

                let (theme_icon, theme_name) = match self.cfg.theme.as_str() {
                    "light" => (&set.theme_light, "浅色"),
                    "dark" => (&set.theme_dark, "深色"),
                    _ => (&set.theme_system, "跟随系统"),
                };
                let theme = icons::button(
                    ui,
                    theme_icon,
                    &format!("主题：{theme_name}（点击在 跟随系统 / 浅色 / 深色 之间循环切换）"),
                    TOOL_SIZE,
                    false,
                    None,
                );
                buttons_left = buttons_left.min(theme.rect.min.x);
                if theme.clicked() {
                    self.cfg.theme = match self.cfg.theme.as_str() {
                        "system" => "light",
                        "light" => "dark",
                        _ => "system",
                    }
                    .to_owned();
                    self.persist();
                }

                // 插件面板：独立入口，装了哪个插件就在面板里管哪个
                let plugins_open = self.show_plugins;
                let plugins = icons::button(
                    ui,
                    &set.plugins,
                    if plugins_open {
                        "收起“插件”面板"
                    } else {
                        "打开“插件”面板：安装或卸载可选功能模块"
                    },
                    TOOL_SIZE,
                    false,
                    plugins_open.then_some(ui.visuals().hyperlink_color),
                );
                buttons_left = buttons_left.min(plugins.rect.min.x);
                if plugins.clicked() {
                    self.show_plugins = !self.show_plugins;
                }

                let settings = icons::button(ui, &set.settings, "设置", TOOL_SIZE, false, None);
                buttons_left = buttons_left.min(settings.rect.min.x);
                if settings.clicked() {
                    self.show_settings = !self.show_settings;
                }
            });
        });
        // 天区与按钮分界写给原生命中测试：顶栏空白处 = 拖动区，这行以右 = 按钮
        crate::titlebar::set_drag_zone(
            ui.min_rect().height().max(40.0),
            buttons_left,
            ui.ctx().pixels_per_point(),
        );
        // 标题栏底下压一道发丝线，和内容分层
        let bar = ui.max_rect();
        ui.painter().hline(
            bar.left()..=bar.right(),
            bar.bottom(),
            egui::Stroke::new(1.0, crate::ui::pane_stroke(ui)),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testbed::stub_app;
    use egui::{Pos2, RawInput, Rect, ThemePreference};

    /// 顶栏单独跑一帧（贴图齐全、挂着一条新版本提示），收下这一帧画出来的文字与图片数。
    /// 测试里没有渲染器，字体增量不应用就得显式丢弃，否则 epaint 在 Drop 时 panic
    fn header_frame(dark: bool) -> (Vec<String>, usize) {
        header_frame_with(dark, None)
    }

    /// 同 `header_frame`，但可以指定「插件装没装」（None = 用测试桩的默认值）
    fn header_frame_with(dark: bool, plugin_present: Option<bool>) -> (Vec<String>, usize) {
        let ctx = egui::Context::default();
        ctx.set_theme(if dark { ThemePreference::Dark } else { ThemePreference::Light });
        crate::fonts::install(&ctx, crate::fonts::AUTO);
        let mut app = stub_app(false);
        if let Some(present) = plugin_present {
            app.plugin_present = Some(present);
        }
        app.update_ready = true;
        app.icons = Some(crate::icons::IconSet::load(&ctx));
        let mut out = ctx.run_ui(
            RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1200.0, 600.0))),
                ..Default::default()
            },
            |ui| app.header(ui),
        );
        out.textures_delta.clear();
        let texts = out
            .shapes
            .iter()
            .filter_map(|item| match &item.shape {
                egui::Shape::Text(text) => Some(text.galley.text().to_string()),
                _ => None,
            })
            .collect();
        // 图标走 Shape::Mesh（贴图三角网），顶栏里贴图网格只可能来自图标键
        let images = out
            .shapes
            .iter()
            .filter(|item| matches!(item.shape, egui::Shape::Mesh(_)))
            .count();
        (texts, images)
    }

    /// 右上角那一排文字按钮全部换成了 SVG 图标：旧文案不该再以按钮形式出现，
    /// 图标键（工具 6 + 窗口控制 3 + 新版本 1）一枚不少
    #[test]
    fn top_right_buttons_are_all_icons_now() {
        let (texts, images) = header_frame(true);
        for label in ["提意见", "设置", "主题：", "Beyond Compare", "AI 日志", "取消日志模式"] {
            assert!(
                !texts.iter().any(|text| text.contains(label)),
                "顶栏还有文字按钮「{label}」：{texts:?}"
            );
        }
        assert!(images >= 10, "顶栏该有 10 枚图标键，只画了 {images}");
    }

    /// AI 日志插件没装时，顶栏那枚图标整块不画（不是点下去才提示）
    #[test]
    fn the_ai_log_button_disappears_with_its_plugin() {
        let (_, with_plugin) = header_frame(true);
        let (_, without) = header_frame_with(true, Some(false));
        assert_eq!(
            with_plugin as i64 - without as i64,
            1,
            "未装 AI 日志插件时顶栏应正好少一枚图标（装 {with_plugin} / 未装 {without}）"
        );
    }

    /// 天区写给原生侧的数值得合理：顶栏有高度、按钮区左缘落在窗口里，
    /// 既不是 0（整窗可拖，按钮全废）也不是没写
    #[test]
    #[cfg(windows)]
    fn drag_zone_covers_the_bar_but_not_the_buttons() {
        let _ = header_frame(true);
        let (bar_h, btn_x) = crate::titlebar::drag_zone();
        assert!(bar_h > 0, "顶栏高度没写给原生侧");
        assert!(btn_x > 0 && btn_x < 1200, "按钮区左缘不合理：{btn_x}");
    }
}
