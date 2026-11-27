//! A stand-in for `qwen --acp` that replays a recorded fixture.
//!
//! Usage: `qwen-replay <fixture.jsonl>`. The fixture is the agent's raw
//! stdout from one real run. It is split into segments, each ending at one
//! response line; a segment is played when the client sends the matching
//! method (recognised from the response's shape). Inside a segment, lines go
//! out in order. An agent request for permission waits for the client's
//! answer; a final `cancelled` stop reason waits for `session/cancel`, and an
//! answer of `cancelled` skips to the end of the turn as a cancelled one. A
//! final stop reason of `hang` is never answered (an agent that ignores cancel).
//!
//! Debug methods, for tests: `stub/received` returns every client message
//! seen so far (method and params, or the result of a response);
//! `stub/env` returns the sorted NAMES of the environment variables (never
//! values).

use serde_json::{Value, json};
use std::io::{BufRead, Write};

struct Segment {
    methods: &'static [&'static str],
    lines: Vec<Value>,
    used: bool,
}

fn methods_for(result: &Value) -> &'static [&'static str] {
    if result.get("protocolVersion").is_some() {
        &["initialize"]
    } else if result.get("stopReason").is_some() {
        &["session/prompt"]
    } else if result.get("sessionId").is_some() {
        &["session/new"]
    } else if result.pointer("/_meta/qwenModelSwitch").is_some() {
        &["session/set_model"]
    } else if result.get("modes").is_some() {
        &["session/load", "session/resume"]
    } else {
        &[]
    }
}

struct Stub {
    stdin: std::io::StdinLock<'static>,
    out: std::io::Stdout,
    received: Vec<Value>,
    saw_cancel: bool,
}

impl Stub {
    fn send(&mut self, v: &Value) {
        let mut o = self.out.lock();
        let _ = writeln!(o, "{v}");
        let _ = o.flush();
    }

    fn read(&mut self) -> Option<Value> {
        loop {
            let mut line = String::new();
            if self.stdin.read_line(&mut line).ok()? == 0 {
                return None;
            }
            let Ok(v) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let entry = match v.get("method") {
                Some(m) => json!({"method": m, "params": v.get("params")}),
                None => json!({"response": v.get("result"), "error": v.get("error")}),
            };
            self.received.push(entry);
            if v.get("method").and_then(Value::as_str) == Some("session/cancel") {
                self.saw_cancel = true;
            }
            return Some(v);
        }
    }

    fn play(&mut self, lines: Vec<Value>, request_id: &Value) {
        let mut cancelled = false;
        for mut line in lines {
            let is_response = line.get("method").is_none();
            let is_permission =
                line.get("method").and_then(Value::as_str) == Some("session/request_permission");
            if is_response {
                line["id"] = request_id.clone();
                if line.pointer("/result/stopReason").is_some() {
                    if line.pointer("/result/stopReason") == Some(&json!("hang")) {
                        while self.read().is_some() {}
                        return;
                    }
                    if cancelled {
                        line["result"]["stopReason"] = json!("cancelled");
                    } else if line.pointer("/result/stopReason") == Some(&json!("cancelled")) {
                        while !self.saw_cancel {
                            if self.read().is_none() {
                                return;
                            }
                        }
                    }
                }
                self.send(&line);
            } else if cancelled {
                continue;
            } else if is_permission {
                let id = line["id"].clone();
                self.send(&line);
                loop {
                    let Some(m) = self.read() else { return };
                    if m.get("method").is_none() && m.get("id") == Some(&id) {
                        if m.pointer("/result/outcome/outcome") == Some(&json!("cancelled")) {
                            cancelled = true;
                        }
                        break;
                    }
                }
            } else {
                self.send(&line);
            }
        }
    }
}

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: qwen-replay <fixture.jsonl>");
    let text = std::fs::read_to_string(&path).expect("fixture unreadable");
    let mut segments = Vec::new();
    let mut cur = Vec::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let v: Value = serde_json::from_str(line).expect("fixture line is not JSON");
        let end = v.get("method").is_none();
        let methods = if end { methods_for(&v["result"]) } else { &[] };
        cur.push(v);
        if end {
            segments.push(Segment {
                methods,
                lines: std::mem::take(&mut cur),
                used: false,
            });
        }
    }
    let stdin: &'static std::io::Stdin = Box::leak(Box::new(std::io::stdin()));
    let mut stub = Stub {
        stdin: stdin.lock(),
        out: std::io::stdout(),
        received: vec![],
        saw_cancel: false,
    };
    while let Some(msg) = stub.read() {
        let (Some(method), Some(id)) = (msg.get("method").and_then(Value::as_str), msg.get("id"))
        else {
            continue;
        };
        let id = id.clone();
        match method {
            "stub/received" => {
                // The request itself is the last entry; leave it out.
                let seen: Vec<Value> = stub.received[..stub.received.len() - 1].to_vec();
                stub.send(&json!({"jsonrpc": "2.0", "id": id, "result": seen}));
            }
            "stub/env" => {
                let mut names: Vec<String> = std::env::vars_os()
                    .map(|(k, _)| k.to_string_lossy().into_owned())
                    .collect();
                names.sort();
                stub.send(&json!({"jsonrpc": "2.0", "id": id, "result": names}));
            }
            _ => match segments
                .iter_mut()
                .find(|s| !s.used && s.methods.contains(&method))
            {
                Some(seg) => {
                    seg.used = true;
                    let lines = seg.lines.clone();
                    stub.play(lines, &id);
                }
                None => stub.send(&json!({"jsonrpc": "2.0", "id": id,
                    "error": {"code": -32601, "message": "no recorded answer for this method"}})),
            },
        }
    }
}
