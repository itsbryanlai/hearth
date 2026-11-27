//! Replays recorded Claude stdout while verifying host stdin controls.

use serde_json::{Value, json};
use std::{
    env,
    io::{self, BufRead, Write},
};

fn main() {
    let args: Vec<_> = env::args().skip(1).collect();
    if args == ["--version"] {
        println!("{} (Claude Code)", env::var("REPLAY_VERSION").unwrap());
        return;
    }

    if let Ok(id) = env::var("REPLAY_RESUME") {
        assert!(args.windows(2).any(|pair| pair == ["--resume", &id]));
    }
    for (name, value) in [
        ("--input-format", "stream-json"),
        ("--output-format", "stream-json"),
        ("--permission-prompt-tool", "stdio"),
    ] {
        assert!(args.windows(2).any(|pair| pair == [name, value]));
    }

    let mut input = io::stdin().lock().lines();
    let init = receive(&mut input);
    assert_eq!(init["request"]["subtype"], "initialize");
    success(
        &init,
        json!({
            "account":{"email":"synthetic-private-account"},
            "models":[{"value":"haiku","displayName":"Haiku","resolvedModel":"claude-haiku-5-5"}]
        }),
    );

    loop {
        let host = receive(&mut input);
        if host["type"] == "user" {
            assert_eq!(host["message"]["content"], "Replay");
            break;
        }
        assert_eq!(host["request"]["subtype"], "set_model");
        let model = host["request"]["model"].as_str().unwrap();
        success(
            &json!({"request_id":"unrelated"}),
            json!({"model":"unrelated-model"}),
        );
        success(&host, json!({"model":format!("claude-{model}-5-5")}));
    }

    let scenario = env::var("REPLAY_SCENARIO").unwrap();
    let fixture = std::fs::read_to_string(env::var_os("REPLAY_FIXTURE").unwrap()).unwrap();
    for line in fixture.lines() {
        let mut frame: Value = serde_json::from_str(line).unwrap();
        if frame["type"] == "system" && frame["subtype"] == "init" && scenario == "privacy" {
            frame["memory_paths"] = json!(["$HOME/.claude/memory/synthetic-secret"]);
            send(&frame);
            send(&json!({"type":"future_event", "details":"synthetic-private-unknown"}));
            continue;
        }
        if frame["type"] == "control_response" && scenario == "interrupt" {
            let interrupt = receive(&mut input);
            assert_eq!(interrupt["request"]["subtype"], "interrupt");
            success(&interrupt, frame["response"]["response"].clone());
            continue;
        }

        if frame["type"] == "result" {
            match scenario.as_str() {
                "verbatim" => send_verbatim_message(),
                "privacy" => send_protected_read(),
                "allow" | "deny" => send_bash_result(scenario == "deny"),
                _ => {}
            }
        }

        if frame["type"] == "control_request" {
            if scenario == "privacy" {
                frame["request"]["input"]["file_path"] = json!("$HOME/.claude/synthetic-secret");
                frame["request"]["input"]["description"] = json!("synthetic-private-details");
            }
            send(&frame);
            let answer = receive(&mut input);
            assert_eq!(answer["response"]["request_id"], frame["request_id"]);
            let reply = &answer["response"]["response"];
            let behavior = if scenario == "deny" { "deny" } else { "allow" };
            assert_eq!(reply["behavior"], behavior);
            if behavior == "allow" {
                assert_eq!(reply["updatedInput"], frame["request"]["input"]);
            } else {
                assert!(reply.get("updatedInput").is_none());
            }
        } else {
            send(&frame);
        }
    }
    // Hold the process open like the streaming CLI until the host closes stdin.
    let _ = input.next();
}

fn receive(input: &mut impl Iterator<Item = io::Result<String>>) -> Value {
    serde_json::from_str(&input.next().expect("host closed stdin").unwrap()).unwrap()
}

fn send(frame: &Value) {
    let mut stdout = io::stdout().lock();
    writeln!(stdout, "{frame}").unwrap();
    stdout.flush().unwrap();
}

fn success(request: &Value, response: Value) {
    send(&json!({"type":"control_response", "response":{
        "subtype":"success", "request_id":request["request_id"], "response":response
    }}));
}

fn send_verbatim_message() {
    let text = "Inspect src/.gitignore.";
    let thinking = "I should check src/.gitignore.";
    for (index, delta) in [
        (0, json!({"type":"text_delta", "text":text})),
        (1, json!({"type":"thinking_delta", "thinking":thinking})),
    ] {
        send(
            &json!({"type":"stream_event","api_message_id":"synthetic-complete-message",
            "event":{"type":"content_block_delta","index":index,"delta":delta}}),
        );
    }
    send(
        &json!({"type":"assistant","message":{"id":"synthetic-complete-message","content":[
            {"type":"text","text":text},
            {"type":"thinking","thinking":thinking},
            {"type":"text","text":" Done."}
        ]}}),
    );
}

fn send_protected_read() {
    send(
        &json!({"type":"assistant","message":{"id":"synthetic-read-message","content":[{
            "type":"tool_use","id":"synthetic-protected-read","name":"Read",
            "input":{"file_path":"$HOME/.claude/synthetic-secret"}
        }]}}),
    );
    send(&json!({"type":"user","message":{"content":[{
        "type":"tool_result","tool_use_id":"synthetic-protected-read","content":"synthetic-private-file-body"
    }]}}));
}

fn send_bash_result(denied: bool) {
    send(
        &json!({"type":"assistant","message":{"id":"synthetic-bash-message","content":[{
            "type":"tool_use","id":"synthetic-bash","name":"Bash","input":{"command":"cat src/.gitignore"}
        }]}}),
    );
    send(&json!({"type":"user","message":{"content":[{
        "type":"tool_result","tool_use_id":"synthetic-bash","is_error":denied,
        "content":if denied { "Denied by user" } else { "Synthetic shell output" }
    }]}}));
}
