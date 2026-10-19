//! AI 工作日志插件：把提交记录交给 OpenAI 兼容接口，拿回日志正文。
//!
//! 插件只负责「组装提示词 + 发请求 + 解析回包」；配置读取、界面、
//! 勾选状态都在主程序里。移除 dll 后这些入口随之消失。

/// 插件自己的版本号（info 里上报）
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

mod abi;
mod ai;

#[cfg(test)]
mod tests {
    use crate::ai;

    #[test]
    fn url_normalization() {
        assert_eq!(
            ai::normalize_url("https://api.deepseek.com/"),
            "https://api.deepseek.com/chat/completions"
        );
        assert_eq!(
            ai::normalize_url("https://x/v1/chat/completions"),
            "https://x/v1/chat/completions"
        );
    }

    #[test]
    fn prompt_carries_tone_and_extra() {
        let (system, user) = ai::build_prompt("第一人称", "不超过 200 字", "【a】r1 …");
        assert!(system.contains("工作日志"));
        assert!(user.contains("口吻与身份：第一人称"));
        assert!(user.contains("其他要求：不超过 200 字"));
        assert!(user.contains("【a】r1"));
    }

    #[test]
    fn body_is_valid_openai_json() {
        let body = ai::build_body("qwen-plus", "sys", "usr");
        let value: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(value["model"], "qwen-plus");
        assert_eq!(value["stream"], false);
        assert_eq!(value["messages"][0]["role"], "system");
        assert_eq!(value["messages"][1]["content"], "usr");
    }

    #[test]
    fn response_parsing() {
        let ok = r#"{"choices":[{"message":{"content":"今天做了三件事"}}]}"#;
        assert_eq!(ai::parse_response(ok).unwrap(), "今天做了三件事");

        let bad = r#"{"error":{"message":"Incorrect API key provided"}}"#;
        let err = ai::parse_response(bad).unwrap_err();
        assert!(err.contains("Incorrect API key"));

        assert!(ai::parse_response("not json").unwrap_err().contains("不是 JSON"));
    }

    #[test]
    fn private_hosts_take_the_direct_route() {
        assert!(ai::is_private_host("http://127.0.0.1:8080/v1"));
        assert!(ai::is_private_host("http://192.168.1.5/v1"));
        assert!(ai::is_private_host("http://nas:8666/v1"));
        assert!(!ai::is_private_host("https://api.deepseek.com/v1"));
    }
}
