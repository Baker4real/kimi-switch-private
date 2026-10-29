//! `on-session-start` 入口（issue #197）：经真实二进制喂 SessionStart 事件，退出码恒 0、
//! stdout 为空，每个事件在审计日志恰好一行留痕。数据目录收口在 tempdir；总开关保持默认
//! 关闭，用例绝不探测、不拉起 GUI。

use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

/// 像 hook 执行方那样唤起 CLI：stdin 写入事件后关闭，stdout / stderr 接管道。
fn run_hook(home: &Path, stdin: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_kimi-switch-cli"))
        .arg("on-session-start")
        .env("KIMI_SWITCH_HOME", home)
        .env("KIMI_CODE_HOME", home.join("kimi-code"))
        .env_remove("RUST_LOG")
        .current_dir(home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn kimi-switch-cli");
    child
        .stdin
        .take()
        .expect("stdin pipe")
        .write_all(stdin.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

/// 入口契约：退出码恒 0；stdout 为空（SessionStart 的 stdout 可能被加进模型上下文），日志只走
/// stderr。设置文件损坏时，加载失败的 warn 也不得落到 stdout；总开关取默认关闭，记 disabled。
#[test]
fn on_session_start_exits_zero_with_empty_stdout() {
    let home = tempfile::tempdir().unwrap();
    let config_dir = home.path().join("config");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(config_dir.join("config.toml"), "not [ valid toml =\n").unwrap();

    let event = serde_json::json!({
        "hook_event_name": "SessionStart",
        "session_id": "7f3c9a2e-1b4d-4e5f-8a6b-0c1d2e3f4a5b",
        "session_title": "修一下 README",
        "client_type": "kimi_code_cli",
        "cwd": r"D:\work\secret-project",
        "source": "startup",
    })
    .to_string();
    for stdin in [event.as_str(), "not json", ""] {
        let output = run_hook(home.path(), stdin);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(0), "{stdin}\n{stderr}");
        assert!(
            output.stdout.is_empty(),
            "stdout 必须为空：{}",
            String::from_utf8_lossy(&output.stdout)
        );
        assert!(
            stderr.contains("load config failed"),
            "设置加载失败的 warn 应走 stderr：{stderr}"
        );
    }

    let log = std::fs::read_to_string(home.path().join("data").join("audit.log")).unwrap();
    let details: Vec<String> = log
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .filter(|event| event["action"] == "on_session_start")
        .map(|event| event["detail"].as_str().unwrap_or_default().to_string())
        .collect();
    assert_eq!(
        details,
        [
            "hook_event=SessionStart source=startup client_type=kimi_code_cli decision=disabled",
            "hook_event=- source=- client_type=- decision=disabled",
            "hook_event=- source=- client_type=- decision=disabled",
        ],
        "{log}"
    );
}
