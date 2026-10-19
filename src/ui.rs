//! 各处共用的绘制小工具。`main.rs` 把它们 `pub use` 出去，所以其它模块照旧写 `crate::ink`。

use egui::text::LayoutJob;
use egui::{Color32, FontId, NumExt, TextFormat, Ui, Vec2};

// ------------------------------------------------------------ 语义色板
//
// 界面里反复出现的几种颜色收成常量：新代码一律用常量，不再各处散 Color32::from_rgb。
// 存量代码里的字面量不动（值本来就一致，机械替换没有视觉收益）。

/// 主色（靛蓝）：选中、链接、焦点、active 态这类「主色调」的地方。
/// 用户可在设置页换掉（配置里存 "#RRGGBB"），所以界面取色走 `RepoApp::accent`，
/// 这支只是默认值与读不出配置时的兜底；白底那一档由 [`on_white`] 换算，不再另存常量。
pub const ACCENT: Color32 = Color32::from_rgb(108, 140, 255);
/// 成功 / 正常（状态绿灯、已是最新）
pub const OK: Color32 = Color32::from_rgb(80, 200, 120);
/// 待处理提醒（可更新、待提交、忙碌）
pub const AMBER: Color32 = Color32::from_rgb(240, 190, 70);
/// 错误 / 冲突
pub const ERR: Color32 = Color32::from_rgb(240, 100, 100);
/// 两步确认红按钮的底与字（碰服务器、删东西之前那一下）
pub const DANGER_FILL: Color32 = Color32::from_rgb(150, 60, 60);
pub const DANGER_TEXT: Color32 = Color32::from_rgb(255, 240, 240);
/// 浅色主题的正文黑。egui 的 `.strong()` 取的是 `widgets.active.fg_stroke`，
/// 那个位置一旦涂成主色，全页面所有粗体字会一起变蓝——所以「active」只由
/// 底色和描边表达，粗体字始终留在这支近黑上。
pub const INK: Color32 = Color32::from_rgb(24, 27, 34);

// ------------------------------------------------------------ 主题色（可换）

/// 配置里存的 "#RRGGBB" → 主色；留空或读不出来都回退默认那支靛蓝
pub fn accent_rgb(hex: &str) -> [u8; 3] {
    let text = hex.trim().trim_start_matches('#');
    if text.len() == 6 {
        if let Ok(value) = u32::from_str_radix(text, 16) {
            return [
                (value >> 16) as u8,
                (value >> 8) as u8,
                value as u8,
            ];
        }
    }
    let [r, g, b, _] = ACCENT.to_array();
    [r, g, b]
}

/// 取色器吐出的 [r, g, b] → 配置里存的 "#RRGGBB"
pub fn accent_hex(rgb: [u8; 3]) -> String {
    format!("#{:02X}{:02X}{:02X}", rgb[0], rgb[1], rgb[2])
}

/// 对纯白的对比度（1 = 白对白，21 = 黑对白）。ecolor 没带现成的亮度 / 对比度 API，这里自算
fn contrast_on_white(color: Color32) -> f32 {
    let [r, g, b, _] = color.to_array();
    let channel = |value: u8| {
        let c = value as f32 / 255.0;
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    let l = 0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b);
    1.05 / (l + 0.05)
}

/// 浅色主题（白底）用的那一档：饱和度提半成、亮度压一成。
/// 默认那支靛蓝按这条规则换算得 #5275E5，和原来手调的 #546CE6 基本重合。
/// 在 sRGB 伽马空间里做，跟眼睛看到的明暗一致
pub fn on_white(accent: Color32) -> Color32 {
    let mut hsv = egui::ecolor::HsvaGamma::from(accent);
    hsv.s = (hsv.s * 1.05).min(1.0);
    hsv.v *= 0.90;
    // 主题色由用户自己挑，挑到很淡的一档时压一成还不够——描边和链接在白底上要看得见，
    // 于是接着按档压暗，直到对纯白有 3:1（非文字元素的最低对比度）
    let mut color = Color32::from(hsv);
    while contrast_on_white(color) < 3.0 && hsv.v > 0.05 {
        hsv.v *= 0.9;
        color = Color32::from(hsv);
    }
    color
}

/// 白底上的极浅主色面：主色占 `keep` 那一成，其余是白（keep 越小越接近白）
pub fn pale(color: Color32, keep: f32) -> Color32 {
    let [r, g, b, _] = color.to_array();
    let mix = |channel: u8| (255.0 - (255.0 - channel as f32) * keep) as u8;
    Color32::from_rgb(mix(r), mix(g), mix(b))
}

/// 主色的淡底：只换个 alpha，比另算一档实色更跟手
pub fn tint(color: Color32, alpha: u8) -> Color32 {
    let [r, g, b, _] = color.to_array();
    Color32::from_rgba_unmultiplied(r, g, b, alpha)
}

/// 弹窗摆位：中心点对齐屏幕中心。
///
/// 只锚打开的头两帧——第一帧 egui 在做 sizing pass（窗口不可见、尺寸是估的），
/// 居中要等第二帧那个量准的尺寸。第三帧起撤锚：位置留在正中，标题栏也重新能拖，
/// 不至于为了居中把弹窗钉死在屏幕上。
///
/// 顺带给窗口一个稳定 id：标题里带目录名 / 版本号的弹窗，换 target 重开时才算同一个窗口。
pub trait WindowCentered {
    fn centered(self, ctx: &egui::Context, slug: &'static str) -> Self;
}

impl WindowCentered for egui::Window<'_> {
    fn centered(self, ctx: &egui::Context, slug: &'static str) -> Self {
        let id = egui::Id::new(slug);
        let layer = egui::LayerId::new(egui::Order::Middle, id);
        // 上一帧没画过 = 这一帧刚出现（含关掉后重开），计数从头再来
        let held = if ctx.memory(|m| m.areas().visible_last_frame(&layer)) {
            ctx.data(|d| d.get_temp::<u8>(id)).unwrap_or(0)
        } else {
            0
        };
        if held < 2 {
            ctx.data_mut(|d| d.insert_temp(id, held + 1));
            self.id(id).anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
        } else {
            self.id(id)
        }
    }
}

/// 有固定默认尺寸的窗口用的居中矩形：这类窗口（改动明细 / 提交记录）的位置由 `Zoom`
/// 自己记账，还要留最大化—还原，所以只把它的默认矩形摆正，不上 anchor。
pub fn centered_rect(ctx: &egui::Context, size: Vec2) -> egui::Rect {
    let area = ctx.content_rect();
    egui::Rect::from_center_size(area.center(), size.at_most(area.size()))
}

impl crate::RepoApp {
    /// 当前主题色（深色主题直接用这支，浅色主题用 `on_white` 压暗的那档）
    pub fn accent(&self) -> Color32 {
        let [r, g, b] = accent_rgb(&self.cfg.accent);
        Color32::from_rgb(r, g, b)
    }

    /// 主色在白底上的版本：各处描边、拖拽高亮、选中文字都取这一档
    pub fn accent_ink(&self) -> Color32 {
        on_white(self.accent())
    }

    /// 换色要立刻见效：深浅两套样式各重调一遍。两套都得调——
    /// 现在用的是其中一套，切主题时切的是另一套，漏掉它就还是旧色
    pub fn apply_accent(&self, ctx: &egui::Context) {
        let accent = self.accent();
        for theme in [egui::Theme::Dark, egui::Theme::Light] {
            ctx.style_mut_of(theme, |style| tune_style(style, accent));
        }
        ctx.request_repaint();
    }
}

/// 样式微调：`RepoApp::new` 与换主题色时各调一次，深浅两套都要调。
///
/// 这是一次集中换肤：egui 默认皮只调了两行间距，按钮、窗口、选区全是出厂样，
/// 这里统一接管——控件圆角、三态观感、窗口阴影、面板分层、字号层级都在这一处定。
///
/// 观感目标是「干净的扁平底 + 看得见的边界」：底是纯白 / 冷黑，不铺渐变也不做玻璃；
/// 分层全靠发丝描边、极浅的一档表面色和柔和投影。文字全部走中性色，
/// 主色只留给选中态、焦点框和小标记。
///
/// `accent` 是用户选的主题色（默认那支靛蓝），浅色主题另用 [`on_white`] 压暗一档。
pub fn tune_style(style: &mut egui::Style, accent: Color32) {
    let dark = style.visuals.dark_mode;
    // 白底主题的主色档：同一色相压暗一成、饱和度提一档
    let ink = on_white(accent);

    // 间距：按钮更饱满、留白更松，滚动条收细
    style.spacing.item_spacing = Vec2::new(8.0, 6.0);
    style.spacing.button_padding = Vec2::new(10.0, 5.0);
    style.spacing.scroll.bar_width = 8.0;

    // 字号层级：正文与按钮抬到 13（各处显式 .size() 的地方不受影响）；
    // 弹窗标题走 Heading，出厂 20 在一扇 560 宽的窗里太压人，也会把标题栏撑高、
    // 让右上角那枚 14 的叉只剩正中一小点能点着，收到 14 正好占满标题那一带
    for (text_style, size) in [
        (egui::TextStyle::Heading, 14.0),
        (egui::TextStyle::Body, 13.0),
        (egui::TextStyle::Button, 13.0),
        (egui::TextStyle::Small, 12.0),
    ] {
        if let Some(font) = style.text_styles.get_mut(&text_style) {
            font.size = size;
        }
    }

    // 控件三态：圆角统一、不外扩。描边留着——上一版把 bg_stroke 全抹成 NONE，
    // 结果浅底上按钮只剩一层淡灰、读不出边界，这里改回「浅底 + 发丝描边」
    let w = &mut style.visuals.widgets;
    w.noninteractive.corner_radius = 5.into();
    for state in [&mut w.inactive, &mut w.hovered, &mut w.active, &mut w.open] {
        state.corner_radius = 7.into();
        state.expansion = 0.0;
    }

    // 窗口与弹层：圆角 + 柔和投影，让弹窗浮在面板上，而不是和底色平贴
    let v = &mut style.visuals;
    v.window_corner_radius = 10.into();
    v.menu_corner_radius = 8.into();

    if dark {
        // 深色：底是冷黑，控件是一块半透明石板
        v.panel_fill = Color32::from_rgb(17, 19, 26);
        v.window_fill = Color32::from_rgba_unmultiplied(32, 37, 50, 236);
        v.extreme_bg_color = Color32::from_rgba_unmultiplied(9, 11, 17, 190);
        v.window_stroke = egui::Stroke::new(1.0, Color32::from_rgba_unmultiplied(255, 255, 255, 30));
        v.window_shadow = egui::Shadow {
            offset: [0, 14],
            blur: 32,
            spread: 2,
            color: Color32::from_rgba_unmultiplied(0, 0, 0, 140),
        };
        v.popup_shadow = egui::Shadow {
            offset: [0, 6],
            blur: 16,
            spread: 1,
            color: Color32::from_rgba_unmultiplied(0, 0, 0, 110),
        };
        // 选中底色：主题色往暗底上叠。透明度不能太低——低到三成时主题色被近黑的
        // 底色吃掉大半，选中态呈发闷的深蓝，看上去比设置里选的主题色深一大截；
        // 提到四成半，色相能读得出来，又仍够暗、压得住上面的浅色文字
        v.selection.bg_fill = tint(accent, 118);
        v.selection.stroke = egui::Stroke::new(1.0, accent);
        v.hyperlink_color = accent;
        v.text_cursor.stroke.color = accent;
        // 弱化字给死色，不走 weak_text_alpha：那个系数是 gamma_multiply（直接乘通道），
        // 只会把字越乘越黑，浅色主题想要的是「更淡」，正好反了
        v.weak_text_color = Some(Color32::from_rgb(146, 154, 172));

        let w = &mut v.widgets;
        w.noninteractive.fg_stroke = egui::Stroke::new(1.0, Color32::from_rgb(200, 206, 218));
        w.noninteractive.bg_stroke = egui::Stroke::new(1.0, Color32::from_rgba_unmultiplied(255, 255, 255, 34));
        w.inactive.fg_stroke = egui::Stroke::new(1.0, Color32::from_rgb(216, 222, 234));
        w.inactive.weak_bg_fill = Color32::from_rgba_unmultiplied(150, 165, 205, 34);
        w.inactive.bg_stroke = egui::Stroke::new(1.0, Color32::from_rgba_unmultiplied(255, 255, 255, 38));
        w.hovered.fg_stroke = egui::Stroke::new(1.0, Color32::from_rgb(238, 242, 250));
        w.hovered.weak_bg_fill = Color32::from_rgba_unmultiplied(160, 176, 216, 60);
        w.hovered.bg_stroke = egui::Stroke::new(1.0, Color32::from_rgba_unmultiplied(255, 255, 255, 76));
        w.active.fg_stroke = egui::Stroke::new(1.0, Color32::from_rgb(244, 246, 252));
        w.active.weak_bg_fill = tint(accent, 92);
        w.active.bg_stroke = egui::Stroke::new(1.0, accent);
        // open 态那一档 egui 是给「活动窗口的标题栏」用的（Window 的 header frame），
        // 顺带也是展开的菜单 / 折叠头：染一层主色淡底，标题栏就不再是一条死灰带
        w.open.fg_stroke = egui::Stroke::new(1.0, Color32::from_rgb(232, 236, 246));
        w.open.weak_bg_fill = tint(accent, 44);
        w.open.bg_stroke = egui::Stroke::new(1.0, Color32::from_rgba_unmultiplied(255, 255, 255, 44));
    } else {
        // 浅色：底就是纯白，一档灰都不掺；分层全靠描边、极浅的表面色和投影
        v.panel_fill = Color32::from_rgb(255, 255, 255);
        v.window_fill = Color32::from_rgb(255, 255, 255);
        v.extreme_bg_color = Color32::from_rgb(255, 255, 255);
        v.window_stroke = egui::Stroke::new(1.0, Color32::from_rgba_unmultiplied(96, 118, 168, 84));
        v.window_shadow = egui::Shadow {
            offset: [0, 12],
            blur: 30,
            spread: 1,
            color: Color32::from_rgba_unmultiplied(58, 76, 126, 52),
        };
        v.popup_shadow = egui::Shadow {
            offset: [0, 5],
            blur: 14,
            spread: 0,
            color: Color32::from_rgba_unmultiplied(58, 76, 126, 40),
        };
        v.selection.bg_fill = tint(accent, 62);
        v.selection.stroke = egui::Stroke::new(1.0, ink);
        v.hyperlink_color = ink;
        v.text_cursor.stroke.color = ink;
        v.weak_text_color = Some(Color32::from_rgb(96, 104, 120));

        // 底是纯白，控件平时也是一块白，边界只能由描边来说——所以描边比深色主题更实一点。
        // 悬停必须换成一档看得见的冷灰：白底上「白变白」等于没有反馈。
        let w = &mut v.widgets;
        w.noninteractive.fg_stroke = egui::Stroke::new(1.0, INK);
        w.noninteractive.bg_stroke = egui::Stroke::new(1.0, Color32::from_rgb(196, 204, 222));
        w.inactive.fg_stroke = egui::Stroke::new(1.0, INK);
        w.inactive.weak_bg_fill = Color32::from_rgb(255, 255, 255);
        w.inactive.bg_stroke = egui::Stroke::new(1.0, Color32::from_rgb(172, 182, 206));
        w.hovered.fg_stroke = egui::Stroke::new(1.0, INK);
        w.hovered.weak_bg_fill = Color32::from_rgb(238, 243, 251);
        w.hovered.bg_stroke = egui::Stroke::new(1.0, Color32::from_rgb(126, 140, 176));
        // 粗体字取的就是这一档：留回近黑。「按下 / 选中」交给底色和主色描边去说
        w.active.fg_stroke = egui::Stroke::new(1.0, INK);
        w.active.weak_bg_fill = pale(accent, 0.20);
        w.active.bg_stroke = egui::Stroke::new(1.0, ink);
        w.open.fg_stroke = egui::Stroke::new(1.0, INK);
        w.open.weak_bg_fill = pale(accent, 0.12);
        w.open.bg_stroke = egui::Stroke::new(1.0, Color32::from_rgb(172, 182, 206));
    }
}

/// 轻量按钮：只把「平时」那一层底调薄一档，发丝描边保留。
/// 上一版这里连描边一起抹了，结果行内、顶栏一排按钮在浅底上
/// 完全看不出边界——现在要的是「淡但不隐形」。悬停 / 按下仍走全局三态。
/// 不新建子 Ui（那会改变控件在当前布局里的分配矩形），只是加这一个控件期间临时改样式。
pub fn quiet<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    let dark = ui.visuals().dark_mode;
    let previous = ui.style_mut().visuals.widgets.inactive.weak_bg_fill;
    ui.style_mut().visuals.widgets.inactive.weak_bg_fill = if dark {
        Color32::from_rgba_unmultiplied(150, 165, 205, 20)
    } else {
        Color32::TRANSPARENT
    };
    let result = add(ui);
    ui.style_mut().visuals.widgets.inactive.weak_bg_fill = previous;
    result
}

/// 按钮那一档的高度：一行正文 + 上下 `button_padding`（实测默认按钮 26、输入框 20）。
/// 输入框、取色块、以及整行的垂直居中都以它为准，缩放时跟着 spacing 一起放大。
pub fn control_height(ui: &Ui) -> f32 {
    ui.text_style_height(&egui::TextStyle::Body) + 2.0 * ui.spacing().button_padding.y
}

/// 单行输入框要和同一行的按钮一样高：TextEdit 自带上下 2 像素，按钮吃的是 `button_padding`
/// （实测默认框高 20、按钮 26），所以纵向按按钮那一档补齐。
/// 取整得四舍五入：字号「大」时 button_padding.y 是 5.75，直接截断成 5 会让框比按钮矮一截
pub fn field_margin(ui: &Ui) -> egui::Margin {
    egui::Margin::symmetric(4, ui.spacing().button_padding.y.round() as i8)
}

/// 取色块。egui 的 color_button 按 `interact_size` 精确分配大小（出厂 40×18），
/// 既不吃 add_sized 也不吃 margin，只能在这一格里把纵向垫到按钮那一档，画完立刻还原
pub fn color_swatch(ui: &mut Ui, rgb: &mut [u8; 3]) -> egui::Response {
    let previous = ui.spacing().interact_size;
    ui.spacing_mut().interact_size.y = control_height(ui);
    let response = ui.color_edit_button_srgb(rgb);
    ui.spacing_mut().interact_size = previous;
    response
}

/// 行高按「加进来那一刻」定档，后面更高的按钮不会把先加的小字带下来，文字就浮在按钮上沿。
/// 替 `Ui::horizontal` 用：先把整行垫到按钮那一档，行里所有东西按同一档垂直居中
/// （这一档正是按钮本来就撑起来的高度，所以行高本身不变）
pub trait AlignedRow {
    fn aligned_row<R>(&mut self, add: impl FnOnce(&mut Ui) -> R) -> egui::InnerResponse<R>;
}

impl AlignedRow for Ui {
    fn aligned_row<R>(&mut self, add: impl FnOnce(&mut Ui) -> R) -> egui::InnerResponse<R> {
        let tall = control_height(self);
        self.horizontal(|ui| {
            ui.set_min_height(tall);
            add(ui)
        })
    }
}

/// 面板 / 目录行 / 卡片共用的表面色，一处定档，分层关系才不会各处各调一遍。
/// 浅色主题不能再用半透明白——纯白底上「白叠白」看不出来，得是一档极浅的冷灰
pub fn pane_fill(ui: &Ui) -> Color32 {
    if ui.visuals().dark_mode {
        Color32::from_rgba_unmultiplied(150, 165, 205, 26)
    } else {
        Color32::from_rgb(248, 250, 253)
    }
}

/// 表面被悬停时：同一块面再深一档（比全局按钮的 hover 更克制，整行悬停不该抢眼睛）
pub fn pane_hover(ui: &Ui) -> Color32 {
    if ui.visuals().dark_mode {
        Color32::from_rgba_unmultiplied(160, 176, 216, 46)
    } else {
        Color32::from_rgb(238, 243, 251)
    }
}

/// 表面的边界：浅色主题是一支冷灰线，深色主题是白色高光，都只有一像素的存在感
pub fn pane_stroke(ui: &Ui) -> Color32 {
    if ui.visuals().dark_mode {
        Color32::from_rgba_unmultiplied(255, 255, 255, 34)
    } else {
        Color32::from_rgb(228, 232, 240)
    }
}

/// 强调色都是按深色背景调的，浅色主题下压暗一点，保证白底上也能看清。
pub fn ink(ui: &Ui, color: Color32) -> Color32 {
    if ui.visuals().dark_mode {
        color
    } else {
        // 只压暗 RGB，不能动 alpha（gamma_multiply 会连 alpha 一起乘，字会变成半透明）
        let [r, g, b, a] = color.to_array();
        Color32::from_rgba_unmultiplied(
            (r as f32 * 0.55 + 0.5) as u8,
            (g as f32 * 0.55 + 0.5) as u8,
            (b as f32 * 0.55 + 0.5) as u8,
            a,
        )
    }
}

/// 多个关键字在 `text` 里的命中区间（原文字节位置，已排序、重叠已合并）。
/// 大小写按 `char::to_lowercase` 逐字符折叠，与 `str::to_lowercase` 同一个口径——提交页的
/// 过滤判定走的就是 `to_lowercase`，两边共用一套才不会出现「过滤留下了这条、高亮却不标」。
/// 端点必然落在字符边界上：折叠是逐字符做的，一个原文字符折出几个字符都归给同一段。
/// 关键字按原样匹配，调用方负责先转小写（提交页的 `filter_needles` 就是干这个的）。
pub fn hit_ranges(text: &str, needles: &[String]) -> Vec<(usize, usize)> {
    let mut folded = String::new();
    // folded 的每个字节 → 它来自原文哪个字符的字节区间（按字节记，命中偏移直接可查）
    let mut owners: Vec<(usize, usize)> = Vec::new();
    for (start, c) in text.char_indices() {
        let span = (start, start + c.len_utf8());
        for lower in c.to_lowercase() {
            folded.push(lower);
            owners.extend(std::iter::repeat(span).take(lower.len_utf8()));
        }
    }
    let mut hits: Vec<(usize, usize)> = Vec::new();
    for needle in needles {
        let key = needle.as_str();
        if key.is_empty() {
            continue;
        }
        for (at, _) in folded.match_indices(key) {
            let last = at + key.len() - 1;
            hits.push((owners[at].0, owners[last].1));
        }
    }
    hits.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (start, end) in hits {
        // 相邻也算连成一段：两个关键字挨着时中间不该留一条没涂色的缝
        if let Some(last) = merged.last_mut() {
            if start <= last.1 {
                last.1 = last.1.max(end);
                continue;
            }
        }
        merged.push((start, end));
    }
    merged
}

/// 搜索高亮：把命中的片段用醒目底色标出来，其余文字保持原样。关键字可以有多个（任一命中就标）。
pub fn highlight(ui: &Ui, text: &str, needles: &[String], font: FontId, color: Color32) -> LayoutJob {
    let plain = TextFormat::simple(font, color);
    let mut job = LayoutJob::default();
    let ranges = hit_ranges(text, needles);
    if ranges.is_empty() {
        job.append(text, 0.0, plain);
        return job;
    }
    let dark = ui.visuals().dark_mode;
    let mark = mark_format(dark, &plain);
    let mut cursor = 0usize;
    for (start, end) in ranges {
        job.append(&text[cursor..start], 0.0, plain.clone());
        job.append(&text[start..end], 0.0, mark.clone());
        cursor = end;
    }
    if cursor < text.len() {
        job.append(&text[cursor..], 0.0, plain);
    }
    job
}

/// 标记命中片段的样式：深浅两套主题各一组，字色与底色配套。
fn mark_format(dark: bool, plain: &TextFormat) -> TextFormat {
    TextFormat {
        color: if dark {
            Color32::from_rgb(255, 216, 100)
        } else {
            Color32::from_rgb(64, 44, 0)
        },
        background: if dark {
            Color32::from_rgb(96, 76, 18)
        } else {
            Color32::from_rgb(255, 234, 140)
        },
        ..plain.clone()
    }
}

/// 整词的边界：前后不能是字母、数字或下划线（中文算字母，所以整词对中文串很严格）。
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// 逐字符比较的相等判断。不用 `to_lowercase`：它会把字节数改掉（`İ` 展开成两个字节），
/// 拿折叠后的偏移回原文定位会把高亮标错位。
fn slice_eq(a: &str, b: &str, case: bool) -> bool {
    if case {
        return a == b;
    }
    a.chars().flat_map(char::to_lowercase).eq(b.chars().flat_map(char::to_lowercase))
}

/// 一段搜索条件：普通子串 / 大小写敏感 / 正则 / 整词。
///
/// 正则在构建时就编译好，逐条记录判定时只是查一次；放在循环里编译的话，六千多条记录
/// 能把这一帧拖成几秒。
#[derive(Clone)]
pub struct Search {
    /// 界面上原样保留的输入（已去首尾空格）
    pub needle: String,
    regex: Option<regex::Regex>,
    /// 正则写错时的说明，界面直接显示出来
    pub error: String,
    case: bool,
    word: bool,
}

impl Search {
    /// 空输入 = 不过滤也不高亮。正则模式下表达式不合法时 `error` 有说明、且一律不命中。
    pub fn build(needle: &str, case: bool, regex: bool, word: bool) -> Self {
        let needle = needle.trim().to_owned();
        if needle.is_empty() || !regex {
            return Self { needle, regex: None, error: String::new(), case, word };
        }
        // 整词 + 正则：先把用户的式子包成 `(?:…)` 再两头加 `\b`。直接拼在最外层会被
        // 式子里的 `|` 抢走结合性（`add|del` 变成「整词 add」或「del」）
        let pattern = if word {
            format!(r"\b(?:{needle})\b")
        } else {
            needle.clone()
        };
        match regex::RegexBuilder::new(&pattern)
            .case_insensitive(!case)
            .build()
        {
            Ok(re) => Self { needle, regex: Some(re), error: String::new(), case, word },
            Err(e) => Self {
                needle,
                regex: None,
                error: format!("正则不合法：{e}"),
                case,
                word,
            },
        }
    }

    pub fn is_empty(&self) -> bool {
        self.needle.is_empty()
    }

    /// 命中区间（原文字节偏移）。判定与高亮都走这里，两边看到的永远是同一批字。
    pub fn ranges(&self, text: &str) -> Vec<(usize, usize)> {
        if self.needle.is_empty() {
            return Vec::new();
        }
        match &self.regex {
            Some(re) => re
                .find_iter(text)
                .map(|m| (m.start(), m.end()))
                .filter(|(start, end)| !self.word || self.boundary_ok(text, *start, *end))
                .collect(),
            // 表达式写错时不命中任何东西（也不能当成「全部命中」把列表清空）
            None if !self.error.is_empty() => Vec::new(),
            None => self.literal_ranges(text),
        }
    }

    /// 是否命中；空输入算命中（不过滤）。
    pub fn hits(&self, text: &str) -> bool {
        if self.needle.is_empty() {
            return true;
        }
        // 表达式写错不等于「谁都不匹配」：列表保持原样，界面上浮一条说明就好。
        // 高亮那边 ranges 返回空，所以不会有任何涂色，两边不矛盾
        if !self.error.is_empty() {
            return true;
        }
        match &self.regex {
            // 正则且没开整词：`is_match` 找到第一处就返回，不必把命中全数出来
            Some(re) if !self.word => re.is_match(text),
            _ => !self.ranges(text).is_empty(),
        }
    }

    /// 子串模式：从每个字符边界起试一次，忽略大小写时按 Unicode 简单折叠比较。
    fn literal_ranges(&self, text: &str) -> Vec<(usize, usize)> {
        let bytes = text.as_bytes();
        let width = self.needle.len();
        let mut out = Vec::new();
        let mut start = 0;
        while start + width <= bytes.len() {
            // 起点落在字符中间时 `get` 给 None，跳过就是
            if let Some(span) = text.get(start..start + width) {
                let end = start + width;
                if slice_eq(span, &self.needle, self.case)
                    && (!self.word || self.boundary_ok(text, start, end))
                {
                    out.push((start, end));
                }
            }
            let step = text[start..].chars().next().map_or(1, |c| c.len_utf8());
            start += step;
        }
        out
    }

    fn boundary_ok(&self, text: &str, start: usize, end: usize) -> bool {
        let before = text[..start].chars().next_back();
        let after = text[end..].chars().next();
        !before.is_some_and(is_word_char) && !after.is_some_and(is_word_char)
    }
}

/// 按 `Search` 实际命中的区间高亮：开关一开，标出来的就该是真正匹配上的那几段，
/// 而不是不分大小写的同一串字。
pub fn highlight_with(
    ui: &Ui,
    text: &str,
    search: &Search,
    font: FontId,
    color: Color32,
) -> LayoutJob {
    let plain = TextFormat::simple(font, color);
    let mark = mark_format(ui.visuals().dark_mode, &plain);
    let mut job = LayoutJob::default();
    let mut cursor = 0usize;
    for (start, end) in search.ranges(text) {
        if start < cursor || end > text.len() {
            continue;
        }
        job.append(&text[cursor..start], 0.0, plain.clone());
        job.append(&text[start..end], 0.0, mark.clone());
        cursor = end;
    }
    job.append(&text[cursor..], 0.0, plain);
    job
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{Context, RawInput, ThemePreference};

    /// 跑一遍高亮，把结果拆成「未命中的原文」与「命中的片段」，方便断言。
    fn split(dark: bool, text: &str, needles: &[&str]) -> (String, Vec<String>) {
        let ctx = Context::default();
        ctx.set_theme(if dark {
            ThemePreference::Dark
        } else {
            ThemePreference::Light
        });
        let needles: Vec<String> = needles.iter().map(|n| n.to_lowercase()).collect();
        let mut plain = String::new();
        let mut marks: Vec<String> = Vec::new();
        let mut output = ctx.run_ui(RawInput::default(), |ui| {
            let job = highlight(ui, text, &needles, FontId::proportional(12.0), Color32::WHITE);
            for section in &job.sections {
                let range = section.byte_range.start.0..section.byte_range.end.0;
                let piece = &job.text[range];
                if piece.is_empty() {
                    continue;
                }
                if section.format.background == Color32::TRANSPARENT {
                    plain.push_str(piece);
                } else {
                    marks.push(piece.to_owned());
                }
            }
        });
        // 测试不渲染纹理，显式丢弃字体增量，否则 epaint 在 Drop 时按未应用增量 panic
        output.textures_delta.clear();
        (plain, marks)
    }

    #[test]
    fn empty_needle_keeps_text_untouched() {
        let (plain, marks) = split(true, "svn log -r HEAD:1", &["   "]);
        assert_eq!(plain, "svn log -r HEAD:1");
        assert!(marks.is_empty(), "{marks:?}");
    }

    #[test]
    fn marks_every_match_case_insensitively() {
        let (plain, marks) = split(true, "SVN svn svn/HRP.Hr", &["svn"]);
        assert_eq!(marks, vec!["SVN", "svn", "svn"]);
        assert_eq!(plain, "  /HRP.Hr");
    }

    /// 多个关键字一起给：各自命中都要标，区间重叠的合并成一段（同一片字涂两遍底色会变深）。
    #[test]
    fn overlapping_needles_merge_into_one_mark() {
        let (plain, marks) = split(true, r"src\svn\SvnFoo.java", &["svn", "svnfoo"]);
        assert_eq!(marks, vec!["svn", "SvnFoo"], "{marks:?}");
        assert_eq!(plain, r"src\\.java");
    }

    /// 命中判定要和 `str::to_lowercase` 完全同口径：过滤依据用的就是它，两边不一致就会出现
    /// 「这一条留下了、高亮却不标」。顺带验证区间端点都落在字符边界上（切中文不 panic）。
    #[test]
    fn hit_ranges_agree_with_to_lowercase_contains() {
        for text in [r"src\视图\SVN报告.java", r"报告\Report.txt", "纯中文没有英文"] {
            for needle in ["svn", "报告", "视图", "repor", "中文"] {
                let hits = hit_ranges(text, &[needle.to_lowercase()]);
                assert_eq!(
                    !hits.is_empty(),
                    text.to_lowercase().contains(needle),
                    "{text} / {needle}"
                );
                let mut rebuilt = String::new();
                let mut cursor = 0;
                for (start, end) in &hits {
                    rebuilt.push_str(&text[cursor..*start]);
                    rebuilt.push_str(&text[*start..*end]);
                    cursor = *end;
                }
                rebuilt.push_str(&text[cursor..]);
                assert_eq!(rebuilt, text, "区间必须能拼回原文：{hits:?}");
            }
        }
    }

    /// 相对亮度（WCAG 2.x）：对比度断言用，和视觉上的明暗感受一致
    fn luminance(color: Color32) -> f32 {
        let [r, g, b, _] = color.to_array();
        let channel = |value: u8| {
            let c = value as f32 / 255.0;
            if c <= 0.04045 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b)
    }

    /// 前景与底色的对比度（1 ~ 21）
    fn contrast(fg: Color32, bg: Color32) -> f32 {
        let (fg, bg) = (luminance(fg), luminance(bg));
        let (hi, lo) = if fg > bg { (fg, bg) } else { (bg, fg) };
        (hi + 0.05) / (lo + 0.05)
    }

    /// 两套主题的灰字都要看得清：弱化文字与正文对面板底色各自达到门槛，且正文比弱文字亮。
    /// 浅色主题另加两条：底色是纯白，粗体字与正文同色（近黑，不许是主色）。
    #[test]
    fn gray_text_is_readable() {
        let ctx = Context::default();
        let before = ctx.style_of(egui::Theme::Dark).visuals.weak_text_color();
        ctx.style_mut_of(egui::Theme::Dark, |style| crate::ui::tune_style(style, crate::ui::ACCENT));
        ctx.style_mut_of(egui::Theme::Light, |style| crate::ui::tune_style(style, crate::ui::ACCENT));
        let dark = ctx.style_of(egui::Theme::Dark).visuals.clone();
        let weak = dark.weak_text_color();
        assert!(
            contrast(weak, dark.panel_fill) >= 4.0,
            "深色主题的弱化文字看不清：{weak:?} 对 {:?} 只有 {:.1}:1",
            dark.panel_fill,
            contrast(weak, dark.panel_fill)
        );
        assert!(
            contrast(dark.text_color(), dark.panel_fill) >= 5.0,
            "深色主题的正文对比度不够"
        );
        assert!(
            luminance(weak) < luminance(dark.text_color()),
            "弱化文字该比正文暗，层次别拉平"
        );
        assert!(
            contrast(before, dark.panel_fill) < 3.0,
            "egui 默认的灰字本来就低（2.3:1），这条是这次要改的理由"
        );
        let light = ctx.style_of(egui::Theme::Light).visuals.clone();
        assert_eq!(
            light.weak_text_color,
            Some(Color32::from_rgb(96, 104, 120)),
            "浅色主题的弱化文字是写死的，不走 weak_text_alpha（那是乘法，只会把灰字压得更淡）"
        );
        assert!(
            contrast(light.weak_text_color(), light.panel_fill) >= 5.0,
            "浅色主题的灰字：{:?} 对 {:?} 只有 {:.1}:1",
            light.weak_text_color(),
            light.panel_fill,
            contrast(light.weak_text_color(), light.panel_fill)
        );
        assert_eq!(
            light.panel_fill,
            Color32::from_rgb(255, 255, 255),
            "浅色主题的底色就是纯白，一档灰都不掺：{:?}",
            light.panel_fill
        );
        assert_eq!(
            light.strong_text_color(),
            light.text_color(),
            "粗体字走的是 widgets.active 的前景，一旦涂成主色全页面粗体一起变蓝"
        );
        assert!(
            contrast(light.strong_text_color(), light.window_fill) >= 8.0,
            "浅色主题的正文要贴近黑"
        );
    }

    /// 主题色：配置里存的是 "#RRGGBB"，读不出来一律回退默认靛蓝；
    /// 白底那一档由 on_white 换算，默认色落在 #5275E5（原来手调的是 #546CE6，差在色相没跟着漂）
    #[test]
    fn accent_parsing_and_the_white_theme_variant() {
        assert_eq!(accent_rgb(""), [108, 140, 255], "留空回退默认靛蓝");
        assert_eq!(accent_rgb("#ff8040"), [255, 128, 64]);
        assert_eq!(accent_rgb("zz"), [108, 140, 255], "坏值也是默认");
        assert_eq!(accent_hex([255, 128, 64]), "#FF8040");
        assert_eq!(
            on_white(ACCENT).to_array(),
            [82, 117, 229, 255],
            "默认色的白底档变了：说明 on_white 的倍率被动过"
        );
        // 用户能挑到的几档典型颜色：淡黄最容易在白底上看不见
        for pick in [
            Color32::from_rgb(255, 128, 64),
            Color32::from_rgb(120, 220, 200),
            Color32::from_rgb(250, 240, 150),
            Color32::from_rgb(230, 160, 220),
        ] {
            let ink = on_white(pick);
            let [_, _, _, a] = ink.to_array();
            assert_eq!(a, 255, "只该动明暗，不能把颜色变成半透明");
            assert!(
                luminance(ink) <= luminance(pick),
                "{pick:?} 的白底档 {ink:?} 反而更亮了"
            );
            assert!(
                contrast(ink, Color32::WHITE) >= 3.0,
                "{pick:?} 的白底档 {ink:?} 对白只有 {:.1}:1，描边和链接会看不见",
                contrast(ink, Color32::WHITE)
            );
        }
    }

    #[test]
    fn multibyte_needle_splits_on_char_boundaries() {
        // 前后夹着中文，确认命中区间的端点都落在字符边界上（切坏 UTF-8 序列会直接 panic）
        let (plain, marks) = split(false, "修复 药品明细 保存 药品", &["药品"]);
        assert_eq!(marks, vec!["药品", "药品"]);
        assert_eq!(plain, "修复 明细 保存 ");
    }
}

#[cfg(test)]
mod search_tests {
    use super::{highlight_with, Search};
    use egui::{Color32, Context, FontId, RawInput, ThemePreference};

    fn build(needle: &str, case: bool, regex: bool, word: bool) -> Search {
        let search = Search::build(needle, case, regex, word);
        assert!(search.error.is_empty(), "不该报错：{}", search.error);
        search
    }

    #[test]
    fn plain_text_search_ignores_case_until_switched_on() {
        assert!(build("fix", false, false, false).hits("Fixed 药品"));
        assert!(
            !build("fix", true, false, false).hits("Fixed 药品"),
            "开了大小写敏感就不该认 fix"
        );
        assert!(build("FIX", true, false, false).hits("FIX 掉了"), "同写法大小写敏感照样命中");
        assert!(!build("FIX", true, false, false).hits("Fixed 药品"), "大小写敏感时 FIX 不认 Fixed");
        assert!(build("Fix", true, false, false).hits("Fixed 药品"));
        // 中文没有大小写，两种开关状态下都按原样比
        assert!(build("药品", true, false, false).hits("修复 药品 明细"));
    }

    #[test]
    fn whole_word_skips_partial_matches() {
        assert!(build("add", false, false, true).hits("please add it"));
        assert!(
            !build("add", false, false, true).hits("address book"),
            "address 里的 add 不算整词"
        );
        assert!(build("add", false, false, true).hits("add-on"), "连字符算词边界");
        // 每个汉字都算「字母」，所以整词对中文子串更严格，标点隔开才算
        assert!(!build("药品", false, false, true).hits("修复药品明细"));
        assert!(build("药品", false, false, true).hits("修复「药品」明细"));
    }

    #[test]
    fn regex_mode_follows_the_same_two_switches() {
        assert!(build(r"r\d+", false, true, false).hits("本次 r1208 提交"));
        assert!(!build(r"r\d+", true, true, false).hits("本次 R1208 提交"));
        assert!(build(r"R\d+", true, true, false).hits("本次 R1208 提交"));
        assert!(build("药品|明细", false, true, false).hits("保存明细"));
        // 交替式配整词时 `\b` 必须包住整个式子：否则只剩「整词 药品」这一半管用。
        // 汉字都算词字符，所以「保存明细」里的明细没有词边界，被标点隔开才算整词
        assert!(build("药品|明细", false, true, true).hits("保存「明细」"));
        assert!(!build("药品|明细", false, true, true).hits("保存明细"));
        assert!(!build("add|del", false, true, true).hits("delete all"));
        assert_eq!(build(r"r\d+", false, true, false).ranges("本次 r1208 提交").len(), 1);
    }

    #[test]
    fn broken_regex_says_why_and_filters_nothing() {
        let search = Search::build("([", false, true, false);
        assert!(!search.error.is_empty(), "写错的表达式要给说明");
        // 列表保持原样（谁都不被筛掉），但也不会有任何涂色
        assert!(search.hits("任意文本"), "写错不等于「谁都不匹配」");
        assert!(search.ranges("任意文本").is_empty(), "写错时不该高亮任何东西");
        // 空输入不过滤
        assert!(Search::build("   ", false, false, false).is_empty());
        assert!(Search::build("", false, true, true).hits("任意文本"));
    }

    /// 高亮标的必须正是命中的那几段：大小写敏感时不能把另一种写法也涂上色
    #[test]
    fn highlight_marks_exactly_the_hits() {
        let ctx = Context::default();
        ctx.set_theme(ThemePreference::Dark);
        let search = build("fix", true, false, false);
        let mut marked = Vec::new();
        let mut output = ctx.run_ui(RawInput::default(), |ui| {
            let job = highlight_with(
                ui,
                "fix Fix FIX",
                &search,
                FontId::proportional(12.0),
                Color32::WHITE,
            );
            for section in &job.sections {
                let range = section.byte_range.start.0..section.byte_range.end.0;
                if section.format.background != Color32::TRANSPARENT {
                    marked.push(job.text[range].to_owned());
                }
            }
        });
        output.textures_delta.clear();
        assert_eq!(marked, vec!["fix".to_owned()], "实际标了 {marked:?}");
    }
}
