//! 双向合并插件：整个合并算法（含 svn 读取、三路合并、备份还原）都装在这个 DLL 里，
//! 主程序只通过 `rm_plugin_call` 的 JSON 接口调用，不再编译任何合并逻辑。
//!
//! 与主程序的契约见 `abi.rs`：`abi` 版本号对不上、或返回的 JSON 解不动，主程序会拒绝加载。

/// 差异核对报告里会写一行程序版本；插件自己的版本号
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

mod childproc;
mod localdiff;
mod merge;
mod merge_io;
mod svn;

mod abi;

/// 主程序 `config.rs` 里那份白名单默认值；插件自己留一份，免得依赖主程序的 crate
mod config {
    pub const DEFAULT_MERGE_INCLUDE: &str = "*.java;*.xml;*.sql;*.vue;*.js;*.ts;*.json;*.properties;\
*.yml;*.yaml;*.html;*.htm;*.css;*.md;*.txt;*.py;*.cs;*.go;*.sh;*.bat;*.conf;*.ini";

    /// 只在 localdiff 那条 `#[ignore]` 的本机实测里出现：插件里没有配置概念，
    /// 给个空壳让它编得过即可（真跑那条测试请回主程序里跑）
    pub struct Config {
        pub dirs: Vec<Dir>,
    }

    pub struct Dir {
        pub bc_target: String,
        pub path: String,
        pub bc_filter: String,
        pub label: String,
    }

    impl Config {
        pub fn load() -> Self {
            Self { dirs: Vec::new() }
        }
    }
}

/// 规则归一化：主程序 `commit.rs` 里那套「一行一条 → BC 分号串」的缝合规则，
/// 合并白名单/黑名单要用同一份口径，所以这里原样带一份
mod commit {
    fn normalize_rules(raw: &str, sign: char) -> String {
        let opposite = if sign == '-' { '+' } else { '-' };
        let mut parts = Vec::new();
        for line in raw.split(['\n', '\r', ';']).map(str::trim) {
            if line.is_empty() || line.starts_with('#') || line.starts_with(opposite) {
                continue;
            }
            let body = line.strip_prefix(sign).unwrap_or(line).trim();
            if body.is_empty() {
                continue;
            }
            let anchored = body.starts_with(".\\")
                || body.starts_with("./")
                || body.starts_with('\\')
                || body.starts_with('/');
            let segments = body.split(['\\', '/']).filter(|s| !s.is_empty()).count();
            let pattern = if anchored || segments < 2 {
                body.to_owned()
            } else {
                format!("./{body}")
            };
            parts.push(format!("{sign}{pattern}"));
        }
        parts.join(";")
    }

    pub(crate) fn normalize_includes(raw: &str) -> String {
        normalize_rules(raw, '+')
    }

    pub(crate) fn normalize_excludes(raw: &str) -> String {
        normalize_rules(raw, '-')
    }
}
