//! 右上角与窗口控制键的 SVG 图标。
//!
//! 图源是 `assets/icons/` 下的手写 24×24 线稿（stroke 风格，一律画成白色），
//! 启动时用 resvg 栅格化成 64×64 的贴图，绘制时按当前主题的前景色 tint——
//! 一套贴图深浅两个主题通用，整屏缩放不糊。
//!
//! resvg 关掉了 text / 系统字体等重特性：图标里只有 path / circle / line，
//! 纯 Rust，不给 exe 添多少体积，也不会拖慢构建。

use egui::{pos2, Color32, Rect, Sense, TextureHandle, Ui, Vec2};

/// 栅格化分辨率：64 够 3x 屏用，缩小走 LINEAR 不会糊
const PX: usize = 64;

#[derive(Clone)]
pub struct IconSet {
    pub settings: TextureHandle,
    /// 插件面板入口（拼图块）
    pub plugins: TextureHandle,
    /// 主题键的三态图源：随当前主题设置切换（跟随系统 / 浅色 / 深色）
    pub theme_system: TextureHandle,
    pub theme_light: TextureHandle,
    pub theme_dark: TextureHandle,
    pub compare: TextureHandle,
    pub ai_log: TextureHandle,
    pub feedback: TextureHandle,
    pub update: TextureHandle,
    pub minimize: TextureHandle,
    pub maximize: TextureHandle,
    pub restore: TextureHandle,
    pub close: TextureHandle,
}

impl IconSet {
    pub fn load(ctx: &egui::Context) -> Self {
        Self {
            settings: texture(ctx, "icon-settings", include_str!("../assets/icons/settings.svg")),
            plugins: texture(ctx, "icon-plugins", include_str!("../assets/icons/plugins.svg")),
            theme_system: texture(
                ctx,
                "icon-theme-system",
                include_str!("../assets/icons/theme_system.svg"),
            ),
            theme_light: texture(
                ctx,
                "icon-theme-light",
                include_str!("../assets/icons/theme_light.svg"),
            ),
            theme_dark: texture(
                ctx,
                "icon-theme-dark",
                include_str!("../assets/icons/theme_dark.svg"),
            ),
            compare: texture(ctx, "icon-compare", include_str!("../assets/icons/compare.svg")),
            ai_log: texture(ctx, "icon-ai-log", include_str!("../assets/icons/ai_log.svg")),
            feedback: texture(ctx, "icon-feedback", include_str!("../assets/icons/feedback.svg")),
            update: texture(ctx, "icon-update", include_str!("../assets/icons/update.svg")),
            minimize: texture(ctx, "icon-minimize", include_str!("../assets/icons/minimize.svg")),
            maximize: texture(ctx, "icon-maximize", include_str!("../assets/icons/maximize.svg")),
            restore: texture(ctx, "icon-restore", include_str!("../assets/icons/restore.svg")),
            close: texture(ctx, "icon-close", include_str!("../assets/icons/close.svg")),
        }
    }
}

/// 一段 svg → 64×64 白色图标贴图；解析失败给一张全透明占位（少一枚图标，不该崩程序）
fn texture(ctx: &egui::Context, name: &str, svg: &str) -> TextureHandle {
    let image = rasterize(svg)
        .unwrap_or_else(|| egui::ColorImage::new([1, 1], vec![Color32::TRANSPARENT]));
    ctx.load_texture(name, image, egui::TextureOptions::LINEAR)
}

/// svg 文本 → 预乘 RGBA 图。单独拎出来给测试用：十张图源都得能解、不能是空的
pub(crate) fn rasterize(svg: &str) -> Option<egui::ColorImage> {
    let tree = resvg::usvg::Tree::from_str(svg, &resvg::usvg::Options::default()).ok()?;
    let size = tree.size();
    let mut pixmap = resvg::tiny_skia::Pixmap::new(PX as u32, PX as u32)?;
    let scale = resvg::tiny_skia::Transform::from_scale(
        PX as f32 / size.width(),
        PX as f32 / size.height(),
    );
    resvg::render(&tree, scale, &mut pixmap.as_mut());
    Some(egui::ColorImage::from_rgba_premultiplied([PX, PX], pixmap.data()))
}

/// 一枚图标按钮：平时通明，悬停 / 按下铺全局按钮同款底色，图标染前景色。
///
/// `danger` = 悬停整块变红、图标转白（关闭键专用，Windows 惯例）；
/// `active` = 常亮主色（AI 日志开着时用），None = 跟随前景色。
pub fn button(
    ui: &mut Ui,
    tex: &TextureHandle,
    tooltip: &str,
    size: Vec2,
    danger: bool,
    active: Option<Color32>,
) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, tooltip));
    let visuals = ui.style().interact(&resp);
    let pressed = resp.is_pointer_button_down_on();
    if resp.hovered() || pressed {
        let fill = if danger && resp.hovered() && !pressed {
            // 关闭键悬停变红：Windows 十年的惯例，不需要用户学
            Color32::from_rgb(232, 17, 35)
        } else {
            visuals.weak_bg_fill
        };
        ui.painter().rect_filled(rect, 4.0, fill);
    }
    let tint = if danger && resp.hovered() && !pressed {
        Color32::WHITE
    } else {
        active.unwrap_or(visuals.fg_stroke.color)
    };
    let side = rect.width().min(rect.height()) * 0.62;
    let icon_rect = Rect::from_center_size(rect.center(), Vec2::splat(side));
    let uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
    ui.painter().image(tex.id(), icon_rect, uv, tint);
    resp.on_hover_text(tooltip)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 十张内嵌图源都得能解成 64×64 且有内容（白色线稿 = 有不透明像素）。
    /// 解不出来只是少一枚图标，跑起来才看得见，所以在测试里先解一遍
    #[test]
    fn embedded_svgs_rasterize_to_visible_icons() {
        for (name, svg) in [
            ("settings", include_str!("../assets/icons/settings.svg")),
            ("plugins", include_str!("../assets/icons/plugins.svg")),
            ("theme_system", include_str!("../assets/icons/theme_system.svg")),
            ("theme_light", include_str!("../assets/icons/theme_light.svg")),
            ("theme_dark", include_str!("../assets/icons/theme_dark.svg")),
            ("compare", include_str!("../assets/icons/compare.svg")),
            ("ai_log", include_str!("../assets/icons/ai_log.svg")),
            ("feedback", include_str!("../assets/icons/feedback.svg")),
            ("update", include_str!("../assets/icons/update.svg")),
            ("minimize", include_str!("../assets/icons/minimize.svg")),
            ("maximize", include_str!("../assets/icons/maximize.svg")),
            ("restore", include_str!("../assets/icons/restore.svg")),
            ("close", include_str!("../assets/icons/close.svg")),
        ] {
            let image = rasterize(svg).unwrap_or_else(|| panic!("{name}.svg 解析失败"));
            assert_eq!(image.size, [PX, PX], "{name} 的栅格尺寸不对");
            let solid = image.pixels.iter().filter(|p| p.a() > 200).count();
            assert!(solid > 50, "{name} 几乎是空的：不透明只占 {solid} 像素");
        }
    }
}
