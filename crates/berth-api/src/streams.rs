//! Every WebSocket of the API, in one table. berthd and `berth serve` register
//! their stream routes from it and the AsyncAPI document is generated from it,
//! so a stream cannot exist undescribed.

use serde_json::{Value, json};

pub struct Frame {
    pub name: &'static str,
    pub description: &'static str,
    /// JSON for a text frame; empty for a binary one.
    pub example: &'static str,
}

pub struct Stream {
    /// The key handlers are found by.
    pub id: &'static str,
    pub path: &'static str,
    pub summary: &'static str,
    /// `host`, or `both` when the agent may open it too.
    pub level: &'static str,
    /// Query parameters: name and meaning.
    pub parameters: &'static [(&'static str, &'static str)],
    /// Takes `since=` and starts there.
    pub resumable: bool,
    pub server: &'static [Frame],
    pub client: &'static [Frame],
}

const fn text(name: &'static str, description: &'static str, example: &'static str) -> Frame {
    Frame {
        name,
        description,
        example,
    }
}

const fn binary(name: &'static str, description: &'static str) -> Frame {
    Frame {
        name,
        description,
        example: "",
    }
}

/// The streams of berthd.
pub const HOST: &[Stream] = &[
    Stream {
        id: "session",
        path: "/v1/sessions/{id}",
        summary: "A session, live: its output as it is produced, and input to it.",
        level: "both",
        parameters: &[(
            "since",
            "Byte offset to start from. Without it a terminal replays its current screen and a process starts at the beginning.",
        )],
        resumable: true,
        server: &[
            binary(
                "output",
                "Terminal: the terminal's bytes. Process: the first byte is the stream (1 stdout, 2 stderr), the rest its bytes.",
            ),
            text(
                "exited",
                "The command ended. The last frame.",
                r#"{"exited":{"exit_code":0}}"#,
            ),
            text(
                "dropped",
                "This viewer was too slow and missed output; read it back with the output call.",
                r#"{"dropped":{"bytes":65536}}"#,
            ),
        ],
        client: &[
            binary(
                "input",
                "Terminal: bytes typed. Process: first byte 0, the rest goes to standard input.",
            ),
            text(
                "resize",
                "A terminal's new size.",
                r#"{"resize":{"rows":40,"cols":120}}"#,
            ),
            text(
                "signal",
                "INT, TERM, HUP or KILL to the session's process group.",
                r#"{"signal":"INT"}"#,
            ),
            text(
                "eof",
                "Close a process's standard input.",
                r#"{"eof":true}"#,
            ),
        ],
    },
    Stream {
        id: "logs",
        path: "/v1/logs",
        summary: "Journal entries, then what is logged.",
        level: "host",
        parameters: &[
            ("unit", "Only this unit."),
            (
                "since",
                "A cursor from an earlier entry, or a time journalctl understands.",
            ),
        ],
        resumable: true,
        server: &[text(
            "entry",
            "One journal entry.",
            r#"{"time":"2026-11-12T10:00:00Z","unit":"ssh.service","priority":6,"message":"Server listening on 0.0.0.0 port 22.","cursor":"s=…"}"#,
        )],
        client: &[],
    },
    Stream {
        id: "channel",
        path: "/v1/channel",
        summary: "Messages on the channel: the kept ones after `since`, then what arrives.",
        level: "both",
        parameters: &[
            ("topic", "Only topics starting with this."),
            (
                "since",
                "Kept messages after this id come first. Without it, only each topic's `last` message does.",
            ),
        ],
        resumable: true,
        server: &[text(
            "message",
            "One message.",
            r#"{"id":4812,"topic":"myplugin/progress","body":{"percent":40},"keep":"log","from":{"level":"agent","uid":1001},"time":"2026-11-12T10:00:00Z"}"#,
        )],
        client: &[text(
            "publish",
            "Publish a message.",
            r#"{"topic":"myplugin/progress","body":{"percent":40},"keep":"none"}"#,
        )],
    },
    Stream {
        id: "serve",
        path: "/v1/services/{service}/serve",
        summary: "For the program that answers a service's calls. While it is open the service is up.",
        level: "both",
        parameters: &[],
        resumable: false,
        server: &[text(
            "call",
            "A call to answer.",
            r#"{"id":7,"method":"start","body":{},"from":{"level":"agent","uid":1001}}"#,
        )],
        client: &[
            text(
                "result",
                "The answer to a call.",
                r#"{"id":7,"result":{"ok":true}}"#,
            ),
            text(
                "error",
                "A call that failed.",
                r#"{"id":7,"error":{"code":"bad_request","message":"no such thing"}}"#,
            ),
        ],
    },
];

/// The streams `berth serve` adds to each host's own.
pub const SERVE: &[Stream] = &[
    Stream {
        id: "job",
        path: "/v1/jobs/{id}",
        summary: "A job's output as it is produced.",
        level: "host",
        parameters: &[("since", "Byte offset to start from.")],
        resumable: true,
        server: &[
            binary("output", "The job's output."),
            text(
                "exited",
                "The job ended. The last frame.",
                r#"{"exited":{"exit_code":0}}"#,
            ),
        ],
        client: &[],
    },
    Stream {
        id: "inbox",
        path: "/v1/inbox",
        summary: "Messages as they are taken in from the hosts.",
        level: "host",
        parameters: &[
            ("host", "Only this host."),
            ("topic", "Only topics starting with this."),
        ],
        resumable: false,
        server: &[text(
            "message",
            "A stored message with its host.",
            r#"{"host":"server","id":4812,"topic":"myplugin/progress","body":{},"keep":"log","from":{"level":"agent","uid":1001},"time":"2026-11-12T10:00:00Z","done":false}"#,
        )],
        client: &[],
    },
    Stream {
        id: "kv",
        path: "/v1/kv/{namespace}",
        summary: "Changes to a namespace of the key-value store.",
        level: "host",
        parameters: &[],
        resumable: false,
        server: &[
            text(
                "set",
                "A key was written.",
                r#"{"key":"layout","value":{"columns":3},"revision":4}"#,
            ),
            text(
                "removed",
                "A key was removed.",
                r#"{"key":"layout","removed":true}"#,
            ),
        ],
        client: &[],
    },
    Stream {
        id: "events",
        path: "/v1/events",
        summary: "Channel messages of chosen hosts, jobs and key-value changes together. The first frame sent names what to follow; each frame received says where it came from.",
        level: "host",
        parameters: &[],
        resumable: false,
        server: &[
            text(
                "channel",
                "A message from a host's channel.",
                r#"{"source":"channel","host":"server","message":{"id":1,"topic":"a/b","body":{},"keep":"log","from":{"level":"host","uid":0},"time":"2026-11-12T10:00:00Z"}}"#,
            ),
            text(
                "job",
                "A job started or ended.",
                r#"{"source":"job","job":{"id":"j1","subject":"host:server","action":"apply","host":"server","started":"2026-11-12T10:00:00Z"}}"#,
            ),
            text(
                "kv",
                "A key changed.",
                r#"{"source":"kv","namespace":"ui","key":"layout","value":{},"revision":4}"#,
            ),
        ],
        client: &[text(
            "follow",
            "What to follow. Sent once, first.",
            r#"{"hosts":["server"],"topics":["myplugin/"],"jobs":true,"kv":["ui"]}"#,
        )],
    },
];

/// An AsyncAPI 3 document for a set of streams.
pub fn asyncapi(title: &str, description: &str, streams: &[&Stream]) -> Value {
    let mut channels = serde_json::Map::new();
    let mut operations = serde_json::Map::new();
    let mut messages = serde_json::Map::new();
    for stream in streams {
        let mut refs = [vec![], vec![]];
        for (side, frames) in [stream.server, stream.client].into_iter().enumerate() {
            for frame in frames {
                let key = format!("{}.{}", stream.id, frame.name);
                let mut message = json!({
                    "name": frame.name,
                    "summary": frame.description,
                });
                if frame.example.is_empty() {
                    message["contentType"] = json!("application/octet-stream");
                    message["payload"] = json!({"type": "string", "format": "binary"});
                } else {
                    let example: Value =
                        serde_json::from_str(frame.example).expect("frame example is JSON");
                    message["contentType"] = json!("application/json");
                    message["payload"] = json!({"type": "object"});
                    message["examples"] = json!([{"payload": example}]);
                }
                messages.insert(key.clone(), message);
                refs[side].push(key);
            }
        }
        let parameters: serde_json::Map<String, Value> = stream
            .path
            .split('/')
            .filter_map(|part| part.strip_prefix('{')?.strip_suffix('}'))
            .map(|name| (name.to_owned(), json!({"description": name})))
            .collect();
        let query: serde_json::Map<String, Value> = stream
            .parameters
            .iter()
            .map(|(name, meaning)| {
                (
                    (*name).to_owned(),
                    json!({"type": "string", "description": meaning}),
                )
            })
            .collect();
        let channel_messages: serde_json::Map<String, Value> = refs
            .concat()
            .iter()
            .map(|key| {
                (
                    key.clone(),
                    json!({"$ref": format!("#/components/messages/{key}")}),
                )
            })
            .collect();
        channels.insert(
            stream.id.into(),
            json!({
                "address": stream.path,
                "description": stream.summary,
                "parameters": parameters,
                "messages": channel_messages,
                "bindings": {"ws": {"query": {"type": "object", "properties": query}}},
                "x-berth-level": stream.level,
                "x-berth-resumable": stream.resumable,
            }),
        );
        for (action, keys) in [("send", &refs[0]), ("receive", &refs[1])] {
            if keys.is_empty() {
                continue;
            }
            let list: Vec<Value> = keys
                .iter()
                .map(|key| json!({"$ref": format!("#/channels/{}/messages/{key}", stream.id)}))
                .collect();
            operations.insert(
                format!("{}.{action}", stream.id),
                json!({
                    "action": action,
                    "channel": {"$ref": format!("#/channels/{}", stream.id)},
                    "messages": list,
                }),
            );
        }
    }
    json!({
        "asyncapi": "3.0.0",
        "info": {"title": title, "version": crate::VERSION, "description": description},
        "defaultContentType": "application/json",
        "channels": channels,
        "operations": operations,
        "components": {"messages": messages},
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tables_make_a_document() {
        let all: Vec<&Stream> = HOST.iter().chain(SERVE).collect();
        let document = asyncapi("berth", "streams", &all);
        assert_eq!(
            document["channels"]["session"]["address"],
            "/v1/sessions/{id}"
        );
        assert!(
            document["operations"]["session.receive"]["messages"]
                .as_array()
                .unwrap()
                .len()
                == 4
        );
        assert!(document["operations"].get("logs.receive").is_none());
    }
}
