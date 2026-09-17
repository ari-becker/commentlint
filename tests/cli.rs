//! End-to-end tests that run the binary against a local mock of the
//! TypeSafe.ai endpoint.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::thread::{self, JoinHandle};

/// Serves canned Noul answers. Each request gets the next probability from
/// `answers` for every question it contains. Returns the server URL and a
/// handle that yields the captured request bodies.
fn mock_server(answers: Vec<f64>) -> (String, JoinHandle<Vec<serde_json::Value>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/v1/systemone", listener.local_addr().unwrap());
    let handle = thread::spawn(move || {
        let mut bodies = Vec::new();
        for (i, p) in answers.into_iter().enumerate() {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            let mut auth = None;
            let mut len = 0usize;
            loop {
                line.clear();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" || line.is_empty() {
                    break;
                }
                let lower = line.to_ascii_lowercase();
                if let Some(v) = lower.strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap();
                }
                if let Some(v) = line
                    .strip_prefix("Authorization:")
                    .or_else(|| line.strip_prefix("authorization:"))
                {
                    auth = Some(v.trim().to_string());
                }
            }
            assert_eq!(
                auth.as_deref(),
                Some("Bearer test-key-123"),
                "request {i} lacked the bearer key"
            );
            let mut body = vec![0u8; len];
            reader.read_exact(&mut body).unwrap();
            let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
            let mut answers = serde_json::Map::new();
            for q in json["questions"].as_object().unwrap().keys() {
                answers.insert(q.clone(), serde_json::json!({ "type": "noul", "noul": p }));
            }
            bodies.push(json);
            let resp = serde_json::json!({
                "model": "jev-1.13.0",
                "answers": answers,
                "usage": { "input_tokens": 10, "output_tokens": 1 }
            })
            .to_string();
            let mut stream = reader.into_inner();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                resp.len(),
                resp
            )
            .unwrap();
            stream.flush().unwrap();
        }
        bodies
    });
    (url, handle)
}

fn bin() -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_commentlint"));
    c.env("COMMENTLINT_API_KEY", "test-key-123");
    c.env_remove("XDG_CONFIG_HOME");
    c
}

#[test]
fn stdin_passing_text_exits_zero() {
    let (url, server) = mock_server(vec![0.93]);
    let mut child = bin()
        .args(["--from-stdin"])
        .env("COMMENTLINT_API_URL", &url)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"The parser reads the file and emits tokens.\n")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "stdout: {}", String::from_utf8_lossy(&out.stdout));
    assert!(out.stdout.is_empty());
    let bodies = server.join().unwrap();
    assert_eq!(bodies.len(), 1);
    assert_eq!(bodies[0]["state"], "The parser reads the file and emits tokens.");
    assert_eq!(bodies[0]["model"], "jev-latest");
    assert_eq!(bodies[0]["questions"]["active_voice"]["type"], "noul");
    assert_eq!(
        bodies[0]["questions"]["active_voice"]["instructions"],
        "Is the text written in the active voice?"
    );
}

#[test]
fn stdin_failing_text_reports_and_exits_one() {
    let (url, server) = mock_server(vec![0.12]);
    let mut child = bin()
        .args(["--from-stdin"])
        .env("COMMENTLINT_API_URL", &url)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"The file is read by the parser.")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.starts_with("<stdin>:1:1: error: comment fails rule `active_voice`"),
        "{stdout}"
    );
    assert!(stdout.contains("The file is read by the parser."));
    server.join().unwrap();
}

#[test]
fn files_are_parsed_and_config_layers_apply() {
    let dir = tempdir();
    std::fs::write(
        dir.join("a.rs"),
        "// The file is read by the parser.\nfn main() {}\n// ok\n",
    )
    .unwrap();
    std::fs::create_dir(dir.join("sub")).unwrap();
    std::fs::write(
        dir.join("sub").join(".commentlint.toml"),
        "disable = [\"active_voice\"]\n[rules.custom]\ninstructions = \"Custom?\"\ncriteria = { true = \"y\", false = \"n\" }\nthreshold = 0.9\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("sub").join("b.py"),
        "# The tokens were emitted by the lexer.\nx = 1\n",
    )
    .unwrap();
    std::fs::write(dir.join("c.txt"), "not parsed\n").unwrap();

    // One answer per comment: a.rs fails active_voice; sub/b.py gets 0.8,
    // which is below the custom threshold of 0.9.
    let (url, server) = mock_server(vec![0.2, 0.8]);
    let out = bin()
        .args(["-j", "1", "a.rs", "sub/b.py", "c.txt"])
        .current_dir(&dir)
        .env("COMMENTLINT_API_URL", &url)
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(1),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("a.rs:1:1: error: comment fails rule `active_voice`"),
        "{stdout}"
    );
    assert!(
        stdout.contains("sub/b.py:1:1: error: comment fails rule `custom`"),
        "{stdout}"
    );
    assert!(stdout.contains("commentlint: 2 issue(s) found"), "{stdout}");
    let bodies = server.join().unwrap();
    let mut states: Vec<String> = bodies
        .iter()
        .map(|b| b["state"].as_str().unwrap().to_string())
        .collect();
    states.sort();
    assert_eq!(
        states,
        vec![
            "The file is read by the parser.",
            "The tokens were emitted by the lexer."
        ]
    );
    let sub = bodies
        .iter()
        .find(|b| b["state"] == "The tokens were emitted by the lexer.")
        .unwrap();
    let qs = sub["questions"].as_object().unwrap();
    assert_eq!(qs.len(), 1);
    assert!(qs.contains_key("custom"));
}

#[test]
fn list_comments_needs_no_key() {
    let dir = tempdir();
    std::fs::write(
        dir.join("a.go"),
        "package main\n// Prints a greeting.\nfunc main() {}\n",
    )
    .unwrap();
    let out = bin()
        .args(["--list-comments", "a.go"])
        .current_dir(&dir)
        .env_remove("COMMENTLINT_API_KEY")
        .env("HOME", dir.to_str().unwrap())
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout), "a.go:2:1: Prints a greeting.\n");
}

#[test]
fn missing_key_is_an_error() {
    let dir = tempdir();
    std::fs::write(
        dir.join("a.go"),
        "package main\n// Prints a greeting.\nfunc main() {}\n",
    )
    .unwrap();
    let out = bin()
        .args(["a.go"])
        .current_dir(&dir)
        .env_remove("COMMENTLINT_API_KEY")
        .env("XDG_CONFIG_HOME", dir.to_str().unwrap())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("API key not found"), "{stderr}");
    assert!(stderr.contains("commentlint/key.txt"), "{stderr}");
}

#[test]
fn nearest_repo_config_has_final_say() {
    let dir = tempdir();
    // The outer config lowers the threshold and adds `r`; the nested config
    // disables the default rule and `r`, and adds `s`.
    std::fs::write(
        dir.join(".commentlint.toml"),
        "threshold = 0.1\nmin_words = 1\n[rules.r]\ninstructions = \"Q?\"\ncriteria = { true = \"y\", false = \"n\" }\n",
    )
    .unwrap();
    std::fs::create_dir(dir.join("sub")).unwrap();
    std::fs::write(
        dir.join("sub").join(".commentlint.toml"),
        "disable = [\"active_voice\", \"r\"]\n[rules.s]\ninstructions = \"S?\"\ncriteria = { true = \"y\", false = \"n\" }\n",
    )
    .unwrap();
    std::fs::write(dir.join("a.go"), "package main\n// top level\n").unwrap();
    std::fs::write(dir.join("sub").join("b.go"), "package main\n// nested\n").unwrap();

    // 0.3 passes the outer threshold of 0.1 but would fail the default 0.5.
    let (url, server) = mock_server(vec![0.3, 0.3]);
    let out = bin()
        .env("COMMENTLINT_API_URL", &url)
        .args(["-j", "1", "a.go", "sub/b.go"])
        .current_dir(&dir)
        .output()
        .unwrap();
    assert!(out.status.success(), "stdout: {}", String::from_utf8_lossy(&out.stdout));
    let bodies = server.join().unwrap();
    let questions_for = |state: &str| -> Vec<String> {
        let b = bodies.iter().find(|b| b["state"] == state).unwrap();
        b["questions"].as_object().unwrap().keys().cloned().collect()
    };
    assert_eq!(questions_for("top level"), vec!["active_voice", "r"]);
    assert_eq!(questions_for("nested"), vec!["s"]);
}

fn tempdir() -> std::path::PathBuf {
    let base = std::env::temp_dir().join(format!(
        "commentlint-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&base).unwrap();
    base
}
