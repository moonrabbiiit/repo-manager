//! 无头跑帧用的测试台：一个不启动任何 svn 任务的 `RepoApp`，外加把真实页面跑一帧、
//! 把画出来的文字与矩形收下来的小驱动器。只有测试用得到，整个模块挂在 cfg(test) 下。

use std::path::PathBuf;
use std::time::Instant;

use egui::{Pos2, RawInput, Rect, ThemePreference, Vec2};

use crate::config::{Config, DirConfig};
use crate::jobs::Pool;
use crate::svn::{Svn, WcInfo};
use crate::{DirView, Page, RepoApp};

/// 按 APPDATA 读写用户数据的用例都要先拿这把锁跑完整段：环境变量是进程级的，
/// 十几个用例各自 `set_var("APPDATA", 临时目录)`，并行时会把别人正在读的那份顶掉
/// （表现是「配置写进去了却读不到」这类莫名其妙的红）。
/// 锁被前面的 panic 弄脏也照常发出去，一个用例失败不该连累排队的其它用例。
pub(crate) fn lock_appdata() -> std::sync::MutexGuard<'static, ()> {
    static APPDATA_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    APPDATA_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 只填字段、不起任何 svn 任务的 app。卡片渲染和点击链要在无头里跑，就得先有个能用的实例。
pub fn stub_app(compact: bool) -> RepoApp {
    let info = |url: &str, revision: &str| WcInfo {
        is_wc: true,
        url: url.to_owned(),
        repos_root: url.to_owned(),
        revision: revision.to_owned(),
        branch: String::new(),
        node_kind: "dir".to_owned(),
        last_rev: revision.to_owned(),
        last_author: "dev".to_owned(),
        last_date: "2026-09-10 09:12".to_owned(),
    };
    let cfg = Config {
        dirs: vec![
            DirConfig { path: r"C:\wc\hrp".to_owned(), label: "HRP".to_owned(), ..Default::default() },
            DirConfig { path: r"C:\wc\crm".to_owned(), label: "CRM".to_owned(), ..Default::default() },
            DirConfig { path: r"C:\wc\empty".to_owned(), label: String::new(), ..Default::default() },
            DirConfig {
                path: r"C:\wc\long".to_owned(),
                label: "这是一个故意写得很长的目录别名用来验证截断".to_owned(),
                ..Default::default()
            },
        ],
        home_compact: compact,
        ..Default::default()
    };
    let dirs = vec![
        // 服务器上有 5 项没更新、本地有 3 项没提交：两枚标记都该亮
        DirView {
            info: Some(info("https://192.168.1.251/svn/hrp", "120")),
            remote: Some(true),
            remote_rev: "128".to_owned(),
            out_of_date: Some(5),
            changed: Some(3),
            checked_at: "09:30".to_owned(),
            ..Default::default()
        },
        // 没有待更新项：本地版本号跟远端一致，标记都是 0
        DirView {
            info: Some(info("https://192.168.1.251/svn/crm", "198")),
            remote: Some(true),
            remote_rev: "200".to_owned(),
            out_of_date: Some(0),
            changed: Some(0),
            ..Default::default()
        },
        // 没检测到信息：两行版本都该是占位的破折号，而不是 r
        DirView {
            changed: Some(1287),
            ..Default::default()
        },
        // 有冲突：边框走冲突色，同时验证超长别名不会把标记挤走
        DirView {
            info: Some(info("https://192.168.1.251/svn/long", "44")),
            remote: Some(true),
            remote_rev: "44".to_owned(),
            out_of_date: Some(0),
            conflicts: Some(2),
            ..Default::default()
        },
    ];
    RepoApp {
        bc: PathBuf::new(),
        version: None,
        candidates: Vec::new(),
        probing: false,
        refresh_after_detect: false,
        // 无头跑帧没有真窗口，DWM 那条路直接跳过
        hwnd: None,
        caption_dark: None,
        corner_maximized: None,
        svn_user: "dev".to_owned(),
        user_probed: true,
        pool: Pool::default(),
        output: Vec::new(),
        page: Page::Main,
        commit: None,
        history: None,
        file_diff: None,
        file_log: None,
        selected: None,
        new_path: String::new(),
        new_label: String::new(),
        edit_label: None,
        label_buf: String::new(),
        label_focus: false,
        edit_group: None,
        group_buf: String::new(),
        group_focus: false,
        edit_tag: None,
        tag_buf: String::new(),
        tag_focus: false,
        drag: None,
        drag_pending: None,
        drag_pos: None,
        drag_target: None,
        drop_zones: Vec::new(),
        relocate: None,
        upload_all: None,
        batch_relocate: None,
        batch_label: None,
        rollback: None,
        resolve: None,
        merge: None,
        commit_rules: None,
        show_settings: false,
        show_plugins: false,
        // 无头测试默认按「插件已装」跑：绝大多数界面测试关心的是入口本身，
        // 「插件没装」的那几条自己把它置 false
        plugin_present: Some(true),
        confirm_remove: None,
        auto_scroll: true,
        next_refresh: Instant::now(),
        pending_refresh: Vec::new(),
        notify_queue: Vec::new(),
        font_note: String::new(),
        update_info: None,
        update_ready: false,
        update_check_at: None,
        // 无头跑帧不该触发定时检查：给个远未来的时刻
        next_update_check: Instant::now() + std::time::Duration::from_secs(3600),
        show_update_confirm: false,
        update_error: None,
        ai_picks: Vec::new(),
        ai_pick_dir: 0,
        show_worklog: false,
        ai_mode: false,
        ai_extra: String::new(),
        ai_result: String::new(),
        ai_error: String::new(),
        // 无头跑帧没有渲染器，贴图不加载（None = 不画）
        mascot: None,
        logo: None,
        // 同理：SVG 图标也不加载，顶栏按钮区整块跳过
        icons: None,
        cfg,
        svn: Svn::default(),
        git: crate::git::Git::default(),
        git_version: None,
        dirs,
    }
}

/// 无头跑帧用的基础输入：给个够放卡片网格的窗口尺寸，并让窗口始终有焦点
pub fn base_input() -> RawInput {
    RawInput {
        screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1400.0, 800.0))),
        focused: true,
        ..Default::default()
    }
}

/// 把主页跑一帧，返回这一帧画出来的「文字 + 矩形」。弹层的开关状态存在内存里，
/// 点下去那一帧还画不出弹层，要多跑一帧稳定的空帧才有。
pub struct Stage {
    pub ctx: egui::Context,
    pub app: RepoApp,
}

impl Stage {
    pub fn new(compact: bool) -> Self {
        let ctx = egui::Context::default();
        // 颜色断言要确定：ink() 在浅色主题下会把强调色压暗，所以固定深色
        ctx.set_theme(ThemePreference::Dark);
        crate::fonts::install(&ctx, crate::fonts::AUTO);
        Self { ctx, app: stub_app(compact) }
    }

    pub fn step(&mut self, click: Option<Pos2>) -> Vec<(String, Rect)> {
        let mut events = Vec::new();
        if let Some(pos) = click {
            let press = |pressed: bool| egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            };
            events.push(egui::Event::PointerMoved(pos));
            events.push(press(true));
            events.push(press(false));
        }
        self.run(events)
    }

    /// 只把指针挪过去、不按：二级菜单靠悬停展开，发点击反而会先把整层菜单收掉
    pub fn hover(&mut self, pos: Pos2) -> Vec<(String, Rect)> {
        self.run(vec![egui::Event::PointerMoved(pos)])
    }

    /// 右键：卡片菜单现在挂在右键上（「+」按钮已移除）
    pub fn right_click(&mut self, pos: Pos2) -> Vec<(String, Rect)> {
        let press = |pressed: bool| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Secondary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        self.run(vec![egui::Event::PointerMoved(pos), press(true), press(false)])
    }

    pub fn run(&mut self, events: Vec<egui::Event>) -> Vec<(String, Rect)> {
        let mut input = base_input();
        input.events = events;
        self.frame_runner(input)
    }

    /// 指定这一帧的输入时刻（`RawInput.time`）再跑一帧。
    /// `base_input()` 的 predicted_dt 是 0，egui 的 i.time 不会自己往前走，
    /// 「按住满 1 秒才算拖动」这类按时间判定的行为只能这样喂时间
    pub fn timed(&mut self, time: f64, events: Vec<egui::Event>) -> Vec<(String, Rect)> {
        let mut input = base_input();
        input.time = Some(time);
        input.events = events;
        self.frame_runner(input)
    }

    fn frame_runner(&mut self, input: egui::RawInput) -> Vec<(String, Rect)> {
        let Stage { ctx, app } = self;
        let mut out = ctx.run_ui(input, |ui| app.main_page(ui));
        // 测试里没有渲染器，字体增量不应用就得显式丢弃，否则 epaint 在 Drop 时 panic
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

    pub fn rect_of(texts: &[(String, Rect)], label: &str) -> Option<Rect> {
        texts
            .iter()
            .find(|(text, _)| text == label)
            .map(|(_, rect)| *rect)
    }
}
