//! AI 请求本体：组装提示词、调 OpenAI 兼容接口、解析回包。
//!
//! 与主程序 `worklog.rs` 里原来的实现一致：密钥写进 curl 的 `--config` 文件
//! （不经过命令行，任务管理器看不到），用完即删；内网地址绕过系统代理。

use std::path::PathBuf;

/// 服务地址自动补全：OpenAI 兼容接口的完整地址固定以 /chat/completions 结尾，
/// 但用户经常只填 base 地址（如 https://dashscope.aliyuncs.com/compatible-mode/v1
/// 或 https://api.deepseek.com），少一段路径服务端就会报 url error，这里统一补上。
pub fn normalize_url(url: &str) -> String {
    let mut url = url.trim().trim_end_matches('/').to_owned();
    if !url.ends_with("/chat/completions") {
        url.push_str("/chat/completions");
    }
    url
}

/// 组装请求体（OpenAI 兼容 /chat/completions，非流式）。
pub fn build_body(model: &str, system: &str, user: &str) -> String {
    serde_json::json!({
        "model": model,
        "stream": false,
        "messages": [
            { "role": "system", "content": system },
            { "role": "user", "content": user }
        ]
    })
    .to_string()
}

/// 组装发给 AI 的两段提示词。口吻为空时用中性的默认说法。
pub fn build_prompt(tone: &str, extra: &str, content: &str) -> (String, String) {
    let system = "你是一名认真负责的软件开发工程师，现在要根据 SVN 提交记录为提交人撰写工作日志。\
                  只依据提供的提交记录归纳，不要编造记录之外的内容。"
        .to_owned();
    let mut user = String::from("请根据下面的 SVN 提交记录，生成一份工作日志。\n\n要求：\n\
        - 把提交记录归纳成若干条工作事项，每条一到两句话，突出完成了什么、改动了什么、解决了什么问题。\n\
        - 同一文件的多次相关改动尽量合并描述，不要逐版本流水账。\n\
        - 直接输出日志正文，不要解释，不要用 Markdown 代码块包裹。\n");
    let tone = tone.trim();
    if tone.is_empty() {
        user.push_str("- 语言简洁、正式。\n");
    } else {
        user.push_str(&format!("- 口吻与身份：{tone}\n"));
    }
    let extra = extra.trim();
    if !extra.is_empty() {
        user.push_str(&format!("- 其他要求：{extra}\n"));
    }
    user.push_str("\n提交记录如下（每条包含目录、版本、修改内容与涉及文件）：\n\n");
    user.push_str(content);
    (system, user)
}

/// 调 AI 接口，返回日志正文。地址 / Key / 模型任一为空都直接报错，不发起请求。
pub fn request(
    url: &str,
    key: &str,
    model: &str,
    tone: &str,
    extra: &str,
    content: &str,
) -> Result<String, String> {
    let url = url.trim();
    if url.is_empty() {
        return Err("尚未配置 AI 服务地址（设置 → AI 日志）".to_owned());
    }
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err(format!("AI 服务地址要以 http:// 或 https:// 开头：{url}"));
    }
    let url = normalize_url(url);
    let key = key.trim();
    if key.is_empty() {
        return Err("尚未配置 API Key（设置 → AI 日志）".to_owned());
    }
    let model = model.trim();
    if model.is_empty() {
        return Err("尚未配置模型名（设置 → AI 日志）".to_owned());
    }
    let (system, user) = build_prompt(tone, extra, content);
    let body = build_body(model, &system, &user);
    let response = post_json(&url, key, &body)?;
    parse_response(&response)
}

/// 用系统 curl 发 POST。请求头（含 API Key）写进 curl 的 `--config` 配置文件，
/// 不走命令行参数——命令行在任务管理器里谁都能看到，配置文件用完即删。
fn post_json(url: &str, key: &str, body: &str) -> Result<String, String> {
    let curl = find_curl()
        .ok_or("未找到系统 curl.exe（Windows 10 1803+ 自带），无法调用 AI 接口")?;
    let dir = std::env::temp_dir().join("RepoManager").join("ai");
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建临时目录失败：{e}"))?;
    let body_path = dir.join("request.json");
    let cfg_path = dir.join("curl.cfg");
    let resp_path = dir.join("response.json");
    let to_curl = |path: &PathBuf| path.display().to_string().replace('\\', "/");
    std::fs::write(&body_path, body.as_bytes()).map_err(|e| format!("写请求体失败：{e}"))?;
    let cfg_text = format!(
        "request = \"POST\"\n\
         header = \"Content-Type: application/json\"\n\
         header = \"Authorization: Bearer {key}\"\n\
         data-binary = \"@{}\"\n\
         output = \"{}\"\n\
         silent\nshow-error\n\
         connect-timeout = \"20\"\nmax-time = \"300\"\n",
        to_curl(&body_path),
        to_curl(&resp_path)
    );
    std::fs::write(&cfg_path, cfg_text.as_bytes()).map_err(|e| format!("写 curl 配置失败：{e}"))?;

    let mut cmd = std::process::Command::new(&curl);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // 界面程序：不能让 curl 弹出黑框
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd.arg("--config").arg(&cfg_path).arg(url);
    // 内网地址绕过系统代理（http_proxy 环境变量会把内网请求转发出去连不回来）
    if is_private_host(url) {
        cmd.arg("--noproxy").arg("*");
    }
    let output = cmd.output();
    // 密钥已经用完了，请求体也发完了：不管成败都先删掉这两个文件
    let _ = std::fs::remove_file(&cfg_path);
    let _ = std::fs::remove_file(&body_path);
    let output = output.map_err(|e| format!("无法启动 {}：{e}", curl.display()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "curl 退出码 {}：{}",
            output.status.code().unwrap_or(-1),
            if stderr.trim().is_empty() {
                "无错误输出".to_owned()
            } else {
                stderr.trim().to_owned()
            }
        ));
    }
    let text = std::fs::read_to_string(&resp_path)
        .map_err(|e| format!("读取 AI 返回失败：{e}（服务端可能没有返回内容）"))?;
    let _ = std::fs::remove_file(&resp_path);
    Ok(text)
}

/// 解析 AI 返回：成功取 `choices[0].message.content`，失败把 `error.message`
/// 或原文前 300 字挑出来给人看。
pub fn parse_response(text: &str) -> Result<String, String> {
    let trimmed = text.trim();
    let value: serde_json::Value = serde_json::from_str(trimmed)
        .map_err(|_| format!("AI 返回的不是 JSON：{}", preview(trimmed)))?;
    if let Some(content) = value
        .pointer("/choices/0/message/content")
        .and_then(|c| c.as_str())
    {
        if !content.trim().is_empty() {
            return Ok(content.to_owned());
        }
    }
    if let Some(message) = value.pointer("/error/message").and_then(|m| m.as_str()) {
        let mut text = format!("AI 接口报错：{message}");
        // 阿里云百炼对错误路径 / 端点不匹配统一报 url error（见其错误码文档）
        if message.to_ascii_lowercase().contains("url error") {
            text.push_str(
                "\n提示：一般是服务地址不对。OpenAI 兼容地址要以 /chat/completions 结尾，\n\
                 通义百炼填 https://dashscope.aliyuncs.com/compatible-mode/v1 即可（程序会自动补全）；\n\
                 若地址本身完整，请检查模型名与端点类型是否匹配（多模态模型不能用纯文本端点）。",
            );
        }
        return Err(text);
    }
    Err(format!(
        "无法从 AI 返回里取到日志内容：{}",
        preview(trimmed)
    ))
}

fn preview(text: &str) -> String {
    let taken: String = text.chars().take(300).collect();
    if text.chars().count() > 300 {
        format!("{taken}…")
    } else {
        taken
    }
}

/// 界面程序不弹控制台窗口
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 找系统自带的 curl.exe（Win10 1803+）。
/// 32 位进程访问 System32 会被重定向到 SysWOW64（同样带 curl），Sysnative 兜底。
fn find_curl() -> Option<PathBuf> {
    for path in [
        PathBuf::from(r"C:\Windows\System32\curl.exe"),
        PathBuf::from(r"C:\Windows\Sysnative\curl.exe"),
    ] {
        if path.is_file() {
            return Some(path);
        }
    }
    None
}

/// 取 URL 里的主机名（剥掉 scheme、userinfo、端口与路径）。
fn host_of(url: &str) -> &str {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    let host = authority
        .rsplit_once('@')
        .map(|(_, h)| h)
        .unwrap_or(authority);
    if let Some(stripped) = host.strip_prefix('[') {
        stripped.split(']').next().unwrap_or(host)
    } else {
        host.split(':').next().unwrap_or(host)
    }
}

/// 更新地址是否指向内网 / 本机。内网地址绝不该走系统代理。
pub fn is_private_host(url: &str) -> bool {
    let host = host_of(url).to_ascii_lowercase();
    if host == "localhost" || host.starts_with("127.") || host == "::1" {
        return true;
    }
    if host.starts_with("192.168.") || host.starts_with("10.") {
        return true;
    }
    // 172.16.0.0 - 172.31.255.255
    if let Some(rest) = host.strip_prefix("172.") {
        if let Ok(second) = rest.split('.').next().unwrap_or("").parse::<u32>() {
            if (16..=31).contains(&second) {
                return true;
            }
        }
    }
    // 不带点的主机名（http://nas:8666 这类）也当内网
    !host.contains('.') && !host.contains(':')
}
