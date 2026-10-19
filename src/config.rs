use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// `DirConfig::merge_include` 留空时用的内置白名单：代码与文本类扩展名，裸写法（`Excludes` 的
/// 包含模式会自动补 `+`）。构建产物、图片、压缩包默认不参与合并——把 zip 静默搬来搬去不是
/// 这里该做的事，他要合非文本文件得自己在白名单里写明。
pub const DEFAULT_MERGE_INCLUDE: &str = "*.java;*.xml;*.sql;*.vue;*.js;*.ts;*.json;*.properties;\
*.yml;*.yaml;*.html;*.htm;*.css;*.md;*.txt;*.py;*.cs;*.go;*.sh;*.bat;*.conf;*.ini";

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DirConfig {
    /// 工作副本目录
    pub path: String,
    /// 列表里显示的别名，可为空
    pub label: String,
    /// 「更多 → 绑定对比对象」里绑定的另一个目录，Beyond Compare 拿它当对比的另一侧
    pub bc_target: String,
    /// 文件夹对比的名称筛选（如 `-*.iml;-*.classpath`），由本程序自己保存，
    /// 不再依赖本机 Beyond Compare 的对比记录，换台机器、换个人也能一致地带出
    pub bc_filter: String,
    /// 本目录的「屏蔽可提交项」，提交页弹窗里那一框，一行一条（`bin\`、`*.log`、`#` 注释）。
    /// 命中的条目默认从提交页消失，也不进提交清单、不计进主页「待提交 N 项」；
    /// 写法与上面的 `bc_filter` 同一套（那边只管差异比较，两边各存各的）
    pub commit_excludes: String,
    /// 本目录的「参与合并的文件」白名单，规则窗口第二页的上格，写法与 `bc_filter` 一致（一行一条）。
    /// 只划定「本地开发侧 ↔ 服务器端」双向合并的范围：白名单外的差异原样不动，
    /// 不参与合并、也不改写第一页两格的任何口径。留空按内置的代码 / 文本扩展名清单走，
    /// 写了内容就只按写的走（非文本文件要他显式加进来）。要在此之上再排除，见下面的 `merge_exclude`
    pub merge_include: String,
    /// 本目录的「不参与合并的文件」黑名单，规则窗口第二页的下格，写法与「屏蔽可提交项」
    /// 一致（一行一条，裸写法自动补 `-`）。它在白名单之后再生效一次，只收窄合并范围：
    /// 命中的差异原样不动，既不改差异数也不管提交。留空表示不额外排除
    /// （`.svn`、`.git` 与落盘临时文件本来就不参与）
    pub merge_exclude: String,
    /// 双向合并的 LCS 行数规模上限（祖先行数 × 变化侧行数）。超过就退化为「字节级单方改写」
    /// 兜底（见 merge.rs 的 too_big 处理），不跑精确行合并。0 = 用内置 60_000_000
    #[serde(default)]
    pub lcs_cell_limit: usize,
    /// 手动分组名，空 = 未分组（列表里不画组标题）
    pub group: String,
    /// 目录标记的文字，空 = 无标记
    pub tag: String,
    /// 标记颜色，形如 "#RRGGBB"；空或解析失败时按默认蓝 #5AA0F0 显示
    pub tag_color: String,
    /// 更新通知订阅：开启后自动检测发现该目录在服务器上有新版本时，发送一次 Windows 系统通知
    pub notify: bool,
    /// 上一次已通知过的远端版本号（SVN 为 revision，Git 为上游 tip 提交 SHA）。
    /// 与当前远端版本相同就不再通知，保证同一个版本只通知一次（跨重启仍然有效）
    pub notified_rev: String,
}

/// 简约卡片上显示哪些内容：设置页逐项开关，四块区域共用这一份配置
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CardShow {
    /// 检测状态灯（●）
    pub lamp: bool,
    /// 目录标记胶囊
    pub tag: bool,
    /// 本地版本号
    pub ver_local: bool,
    /// 远端版本号
    pub ver_remote: bool,
    /// 服务器可更新数
    pub pending: bool,
    /// 本地待提交数
    pub changed: bool,
    /// 本地端差异数（只对绑定过对比对象的目录生效）
    pub diff: bool,
}

impl Default for CardShow {
    fn default() -> Self {
        Self {
            lamp: true,
            tag: true,
            ver_local: true,
            ver_remote: true,
            pending: true,
            changed: true,
            diff: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub svn_exe: String,
    /// git.exe 路径，留空则自动寻找（给 .git 目录行用）
    pub git_exe: String,
    /// Beyond Compare 路径，留空则自动寻找
    pub bc_exe: String,
    /// 调用对比时服务器端放在哪一侧：left / right
    pub bc_server_side: String,
    /// 留空则使用 svn 凭据缓存
    pub auth_user: String,
    pub auth_pass: String,
    pub dirs: Vec<DirConfig>,
    /// 提交记录读取条数
    pub log_limit: i64,
    /// 自动刷新间隔（秒），0 表示关闭
    pub auto_refresh: u64,
    /// 主题：system / light / dark
    pub theme: String,
    /// 主题色 "#RRGGBB"（选中态、链接、焦点框、拖拽高亮都跟着它走）。
    /// 留空或读不出来都用默认那支靛蓝，见 `ui::accent_rgb`
    pub accent: String,
    /// 界面字体族：「自动」= 按本机现成的中文字体挑，其余是 `fonts::available()` 里的族名
    pub ui_font: String,
    /// 界面字号：小 / 标准 / 大 / 特大（整屏缩放，见 `fonts::SCALES`）
    pub ui_scale: String,
    /// 打开「提交记录」页时默认只看本机登录人的记录（关掉就是默认看所有人的）
    pub history_only_mine: bool,
    /// 更新源：official = GitHub 仓库的最新发布，custom = 下面这个自建服务端地址
    pub update_source: String,
    /// 版本更新服务端地址（服务根目录），留空则不检查更新
    pub update_server: String,
    /// 启动时自动检查版本更新
    pub check_update_on_start: bool,
    /// 运行中定期自动检查版本更新（每 5 分钟一次，见 `update::AUTO_CHECK_EVERY_SECS`）
    pub auto_update_check: bool,
    /// 提交成功后自动补跑一次 `svn update`，把整棵工作副本树的版本号推到最新
    pub update_after_commit: bool,
    /// 开机自动启动（实际生效写在注册表 HKCU Run 键里，这里只做记录）
    pub auto_start: bool,
    /// 主页目录列表的样式：false = 详细（整行 + 一排按钮），true = 简约（宽行卡片 + 「+」菜单）
    pub home_compact: bool,
    /// 简约卡片显示哪些内容（状态灯 / 标记 / 版本 / 计数 / 「+」），设置页逐项开关
    pub card_show: CardShow,
    /// AI 生成工作日志：接口地址（OpenAI 兼容的 /chat/completions 完整地址）
    pub ai_url: String,
    /// AI 生成工作日志：接口密钥（明文保存在本机配置文件）
    pub ai_key: String,
    /// AI 生成工作日志：模型名（如 deepseek-chat / gpt-4o-mini）
    pub ai_model: String,
    /// AI 生成工作日志：口吻说明（生成窗口打开时预填进输入框，随改随存）
    pub ai_tone: String,
    /// 主页里收起的分组名列表（按名字记，组删了残留也无害）
    pub collapsed_groups: Vec<String>,
    /// 日志面板是否收起：None = 用户没手动动过，跟着主页样式走（简约收起、详细展开）
    pub log_collapsed: Option<bool>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            svn_exe: String::new(),
            // git.exe 由探测链现找（PATH / 注册表 / 标准安装位置），找到后写回配置
            git_exe: String::new(),
            bc_exe: String::new(),
            // 默认服务器端在左：文件对比的 BASE 版本一直在左，用户的 BC 记录也是 server 在左
            bc_server_side: "left".to_owned(),
            auth_user: String::new(),
            auth_pass: String::new(),
            dirs: Vec::new(),
            log_limit: 100,
            auto_refresh: 120,
            theme: "system".to_owned(),
            // 主题色留空 = 用默认靛蓝，用户在设置页取色后才写 hex
            accent: String::new(),
            history_only_mine: true,
            // 默认查官方 GitHub 发布：不用填任何地址就能检查更新；要用内网服务端在设置里切
            update_source: "official".to_owned(),
            update_server: String::new(),
            check_update_on_start: true,
            // 默认关闭：每 5 分钟联网查一次，要不要开由用户决定
            auto_update_check: false,
            // 默认开启：提交后补一次 update，列表里的「本地 r」会立刻跟上新版本
            update_after_commit: true,
            // 默认关闭：开机自启要用户自己决定，开了就写 HKCU Run 键
            auto_start: false,
            // 默认详细样式：老用户升级后看到的列表跟以前一样，要卡片自己切过去
            home_compact: false,
            // 卡片内容默认全显示：新样式刚上手时和旧版信息量对齐，想精简自己在设置里关
            card_show: CardShow::default(),
            ui_font: crate::fonts::AUTO.to_owned(),
            ui_scale: crate::fonts::SCALE_STANDARD.to_owned(),
            // AI 日志的地址 / 密钥 / 模型都由用户自己提供，默认全空
            ai_url: String::new(),
            ai_key: String::new(),
            ai_model: String::new(),
            ai_tone: String::new(),
            // 分组默认全展开：组名一个都不在折叠列表里
            collapsed_groups: Vec::new(),
            // 收起状态没被手动动过：默认跟随主页样式
            log_collapsed: None,
        }
    }
}

/// 用户在 Explorer 里看到的本程序数据目录名
const APP_DIR: &str = "RepoManager";
/// 改名前的数据目录名，只用于把老用户数据搬过来（见 `migrate_legacy_dir`）
const LEGACY_APP_DIR: &str = "SVNManager";

fn app_data_base() -> PathBuf {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
}

pub fn config_dir() -> PathBuf {
    app_data_base().join(APP_DIR)
}

/// 改名前的老目录：`%APPDATA%\SVNManager`
fn legacy_config_dir() -> PathBuf {
    app_data_base().join(LEGACY_APP_DIR)
}

pub fn config_file() -> PathBuf {
    config_dir().join("config.json")
}

/// 递归复制一棵目录树；`std::fs::copy` 只处理文件，老目录里的 `bc-store-backup-*` 是目录
fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    if from.is_dir() {
        std::fs::create_dir_all(to)?;
        for entry in std::fs::read_dir(from)? {
            let entry = entry?;
            copy_tree(&entry.path(), &to.join(entry.file_name()))?;
        }
        Ok(())
    } else {
        std::fs::copy(from, to).map(|_| ())
    }
}

/// 机器名从 svn_manager 改成 repo-manager 之后第一次启动时，把老目录里的用户数据搬进新目录：
/// 目录列表、分组、标记、更新服务端地址都在 config.json 里，不搬就等于让他重配一遍。
pub fn migrate_legacy_dir() {
    copy_legacy_data(&config_dir(), &legacy_config_dir());
}

/// 搬家实际做的那一步，两个目录直接给进来：测试并发 `set_var("APPDATA")`，
/// 让它自己读环境会互相踩。只在 new 目录还没有 config.json、old 目录有的时候动手；
/// 搬完**保留**旧目录：中途崩了下次启动会再搬一遍，而他要是想换回旧版 exe，老配置还是原样。
/// 返回这一趟到底搬了没有。
fn copy_legacy_data(new_dir: &Path, old_dir: &Path) -> bool {
    if new_dir.join("config.json").is_file() || !old_dir.join("config.json").is_file() {
        return false;
    }
    if std::fs::create_dir_all(new_dir).is_err() {
        return false;
    }
    let Ok(entries) = std::fs::read_dir(old_dir) else {
        return false;
    };
    for entry in entries.flatten() {
        let _ = copy_tree(&entry.path(), &new_dir.join(entry.file_name()));
    }
    true
}

impl Config {
    pub fn load() -> Self {
        let text = std::fs::read_to_string(config_file()).unwrap_or_default();
        // 用记事本等编辑器手工改过的配置常带 UTF-8 BOM，先去掉再解析
        serde_json::from_str(text.trim_start_matches('\u{feff}')).unwrap_or_default()
    }

    pub fn save(&self) -> Result<(), String> {
        let path = config_file();
        std::fs::create_dir_all(config_dir()).map_err(|e| e.to_string())?;
        let text = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(path, text.as_bytes()).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 老配置文件缺字段时按默认值补齐；已经不存在的全局「忽略空白」开关（改成按文件类型
    /// 逐条决定）留在旧配置里也必须能忽略，否则升级后配置直接解析失败。
    #[test]
    fn old_config_without_the_flag_keeps_the_default() {
        let parsed: Config =
        serde_json::from_str(r#"{"svn_exe":"svn","dirs":[{"path":"D:\\a","label":"A"}],"diff_ignore_white":true}"#).unwrap();
        assert_eq!(parsed.log_limit, 100);
        // 老配置里的目录条目没有 bc_target，也要按空值补齐而不是整个配置解析失败
        assert!(parsed.dirs[0].bc_target.is_empty());
        // 后加的「名称筛选」字段：老配置没有也要按空值补齐，且能写回
        assert!(parsed.dirs[0].bc_filter.is_empty());
        // 「屏蔽可提交项」同样是后加字段：老配置没有要按空值补齐（= 一条都不屏蔽）
        assert!(parsed.dirs[0].commit_excludes.is_empty());
        // 「参与合并的文件」也是后加字段：老配置没有按空值补齐，而空值就是「按内置的代码 /
        // 文本清单参与合并」，所以老配置升级后不必迁移也直接能用
        assert!(parsed.dirs[0].merge_include.is_empty());
        // 黑名单同理：老配置没有就按空值补齐，空值等于黑名单一条都不挡
        assert!(parsed.dirs[0].merge_exclude.is_empty());
        assert!(crate::merge::blacklist(&parsed.dirs[0].merge_exclude).is_none());
        let scope = crate::merge::scope(&parsed.dirs[0].merge_include);
        assert!(scope.hits(r"src\main\Foo.java"), "老配置要能默认合代码");
        assert!(!scope.hits("bin/tool.exe"), "内置白名单不碰构建产物");
        // 服务器端所在侧是后加的设置，老配置没有也要按默认值（左侧）补齐
        assert_eq!(parsed.bc_server_side, "left");
        assert_eq!(parsed.theme, "system");
        // 历史页「默认只看本人」保持开启，老用户的使用习惯不变
        assert!(parsed.history_only_mine);
        let off: Config = serde_json::from_str(r#"{"history_only_mine":false}"#).unwrap();
        assert!(!off.history_only_mine);
        // 版本更新是后加的设置：老配置没有也要按默认值补齐（不检查、地址为空）
        assert!(parsed.update_server.is_empty());
        assert!(parsed.check_update_on_start);
        // 定期自动检查是后加的设置：老配置没有要按默认值（关闭）补齐
        assert!(!parsed.auto_update_check);
        assert!(
            serde_json::from_str::<Config>(r#"{"auto_update_check":true}"#)
                .unwrap()
                .auto_update_check
        );
        // 更新源也是后加的：老配置没有要按默认值（官方 GitHub）补齐，写了 custom 才用自建地址
        assert_eq!(parsed.update_source, "official");
        assert_eq!(
            serde_json::from_str::<Config>(r#"{"update_source":"custom"}"#)
                .unwrap()
                .update_source,
            "custom"
        );
        // 提交后自动更新是后加的设置：老配置没有也要按默认值（开启）补齐
        assert!(parsed.update_after_commit);
        // 开机自启是后加的设置：老配置没有也要按默认值（关闭）补齐
        assert!(!parsed.auto_start);
        // 主页样式是后加的设置：老配置没有要按默认值（详细）补齐，写了 true 才用卡片
        assert!(!parsed.home_compact);
        assert!(
            serde_json::from_str::<Config>(r#"{"home_compact":true}"#)
                .unwrap()
                .home_compact
        );
        // 卡片内容开关是后加的设置：老配置没有要按「全显示」补齐，写了 false 才隐藏
        assert!(parsed.card_show == CardShow::default());
        let hidden: Config = serde_json::from_str(r#"{"card_show":{"tag":false}}"#).unwrap();
        assert!(!hidden.card_show.tag);
        assert!(hidden.card_show.lamp && hidden.card_show.ver_local);
        // AI 日志是后加的设置：老配置没有也要按空值补齐
        assert!(parsed.ai_url.is_empty());
        assert!(parsed.ai_key.is_empty());
        assert!(parsed.ai_model.is_empty());
        assert!(parsed.ai_tone.is_empty());
        // git.exe 是双后端改造后加的设置：老配置没有要按空值补齐（启动时自动探测）
        assert!(parsed.git_exe.is_empty());
        // 界面字体、字号是后加的设置：老配置没有也要按默认值（自动挑字体、标准字号）补齐
        assert_eq!(parsed.ui_font, crate::fonts::AUTO);
        assert_eq!(parsed.ui_scale, crate::fonts::SCALE_STANDARD);
        let picked: Config =
            serde_json::from_str(r#"{"ui_font":"微软雅黑","ui_scale":"大"}"#).unwrap();
        assert_eq!(picked.ui_font, "微软雅黑");
        assert_eq!(picked.ui_scale, "大");
        // 分组 / 标记是后加的目录属性：老配置没有要按空值补齐（未分组、无标记）
        assert!(parsed.dirs[0].group.is_empty());
        assert!(parsed.dirs[0].tag.is_empty());
        assert!(parsed.dirs[0].tag_color.is_empty());
        // 更新通知订阅是后加的目录属性：老配置没有要按默认值（关闭、无已通知版本）补齐
        assert!(!parsed.dirs[0].notify);
        assert!(parsed.dirs[0].notified_rev.is_empty());
        // 分组折叠列表是后加的全局设置：老配置没有要按空列表补齐
        assert!(parsed.collapsed_groups.is_empty());
    }

    /// 分组 / 标记字段要能原样写回再读出（往返不丢）。
    #[test]
    fn group_and_tag_fields_roundtrip() {
        let text = r##"{"svn_exe":"svn","collapsed_groups":["生产","测试"],"dirs":[
            {"path":"D:\\a","label":"A","group":"生产","tag":"冻结","tag_color":"#FF8040"},
            {"path":"D:\\b","label":"B"}
        ]}"##;
        let parsed: Config = serde_json::from_str(text).unwrap();
        assert_eq!(parsed.dirs[0].group, "生产");
        assert_eq!(parsed.dirs[0].tag, "冻结");
        assert_eq!(parsed.dirs[0].tag_color, "#FF8040");
        assert!(parsed.dirs[1].group.is_empty());
        assert_eq!(parsed.collapsed_groups, vec!["生产".to_owned(), "测试".to_owned()]);
        let back: Config = serde_json::from_str(&serde_json::to_string(&parsed).unwrap()).unwrap();
        assert_eq!(back, parsed);
    }

    /// 每条用例一个独立的临时根目录，跑完自己清掉，不碰 APPDATA
    fn migration_root(case: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("repo_manager_{case}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        root
    }

    /// 改名后第一次启动：老目录里的东西整棵搬进新目录，旧目录留着
    #[test]
    fn data_moves_from_the_legacy_dir_once() {
        let root = migration_root("migration_case");
        let old = root.join(LEGACY_APP_DIR);
        let new = root.join(APP_DIR);
        std::fs::create_dir_all(old.join("bc-store-backup-20260903")).unwrap();
        std::fs::write(
            old.join("config.json"),
            r#"{"svn_exe":"svn","dirs":[{"path":"D:\\a","label":"A","group":"生产"}]}"#,
        )
        .unwrap();
        std::fs::write(old.join("panic.log"), b"boom").unwrap();
        std::fs::write(old.join("bc-store-backup-20260903/BCSessions.xml"), b"<xml/>").unwrap();

        assert!(copy_legacy_data(&new, &old), "老目录有配置就该搬");
        let text = std::fs::read_to_string(new.join("config.json")).unwrap();
        let moved: Config = serde_json::from_str(&text).unwrap();
        assert_eq!(moved.dirs[0].label, "A");
        assert_eq!(moved.dirs[0].group, "生产");
        assert_eq!(std::fs::read(new.join("panic.log")).unwrap(), b"boom".to_vec());
        assert!(new.join("bc-store-backup-20260903/BCSessions.xml").is_file(), "子目录也要跟着搬");
        // 保留旧目录：想换回旧版 exe 时老配置还是原样
        assert!(old.join("config.json").is_file(), "旧目录不该被删");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 新目录已经有 config.json 就说明搬过了（或者他是全新用户），绝不能拿旧目录盖回去
    #[test]
    fn migration_never_overwrites_a_populated_new_dir() {
        let root = migration_root("migration_keep");
        let old = root.join(LEGACY_APP_DIR);
        let new = root.join(APP_DIR);
        std::fs::create_dir_all(&old).unwrap();
        std::fs::create_dir_all(&new).unwrap();
        std::fs::write(old.join("config.json"), r#"{"dirs":[{"path":"D:\\old","label":"旧"}]}"#)
            .unwrap();
        std::fs::write(new.join("config.json"), r#"{"dirs":[{"path":"D:\\new","label":"新"}]}"#)
            .unwrap();

        assert!(!copy_legacy_data(&new, &old), "新目录有配置就不该再搬");
        let kept: Config =
            serde_json::from_str(&std::fs::read_to_string(new.join("config.json")).unwrap())
                .unwrap();
        assert_eq!(kept.dirs[0].label, "新");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 没有老目录（全新安装）不是错误，也不该顺手建出一个空的新目录
    #[test]
    fn nothing_to_move_leaves_no_empty_dir() {
        let root = migration_root("migration_absent");
        let old = root.join(LEGACY_APP_DIR);
        let new = root.join(APP_DIR);

        assert!(!copy_legacy_data(&new, &old), "老目录不在就什么都不做");
        assert!(!new.exists(), "不该留下空的新目录");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 钉住目录名本身：万一以后有人手滑改回旧名，这条会当场红
    #[test]
    fn data_dir_is_named_after_the_project() {
        assert_eq!(config_file().file_name().unwrap(), "config.json");
        assert_eq!(config_dir().file_name().unwrap(), APP_DIR);
        assert_eq!(APP_DIR, "RepoManager");
        assert_eq!(legacy_config_dir().file_name().unwrap(), LEGACY_APP_DIR);
    }
}
