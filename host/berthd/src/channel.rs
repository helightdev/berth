//! The channel: messages between programs on the host and clients of it.
//! berthd carries them and does not know what they mean.

use crate::error::{E, R};
use crate::state::{Caller, State};
use axum::{
    Json,
    extract::{Extension, Path, Query, State as AppState, ws},
    response::{IntoResponse, Response},
};
use berth_api::{self as api, Keep, Level, Message, Topic, code};
use futures_util::{SinkExt, StreamExt};
use std::{
    collections::{BTreeMap, VecDeque},
    fs::File,
    io::Write,
    path::{Path as FsPath, PathBuf},
    sync::{Arc, Mutex},
};
use tokio::sync::broadcast;

const BODY: usize = 64 << 10;
/// A listener this far behind is dropped.
const BEHIND: usize = 1000;
const DEFAULT: (u64, u64) = (200, 7);
const CEILING: (u64, u64) = (10_000, 90);
/// Ids are set aside in blocks, so a restart never hands one out twice.
const BLOCK: u64 = 1000;

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct Stored {
    #[serde(flatten)]
    message: Message,
    /// Seconds since 1970, for dropping by age.
    at: u64,
}

struct Inner {
    dir: PathBuf,
    next: u64,
    reserved: u64,
    topics: Vec<Topic>,
    last: BTreeMap<String, Stored>,
    logs: BTreeMap<String, VecDeque<Stored>>,
    file: Option<File>,
    file_lines: usize,
}

pub struct Channel {
    inner: Mutex<Inner>,
    live: broadcast::Sender<Arc<Message>>,
}

impl Inner {
    /// The rule of the longest configured prefix: default keep, most messages, most days.
    fn rule(&self, topic: &str) -> (Keep, u64, u64) {
        let rule = self
            .topics
            .iter()
            .filter(|rule| topic.starts_with(&rule.topic))
            .max_by_key(|rule| rule.topic.len());
        (
            rule.and_then(|rule| rule.keep).unwrap_or_default(),
            rule.and_then(|rule| rule.max)
                .unwrap_or(DEFAULT.0)
                .clamp(1, CEILING.0),
            rule.and_then(|rule| rule.days)
                .unwrap_or(DEFAULT.1)
                .clamp(1, CEILING.1),
        )
    }

    fn keep(&mut self, stored: Stored) {
        let topic = stored.message.topic.clone();
        match stored.message.keep {
            Keep::None => {}
            Keep::Last => {
                self.last.insert(topic, stored);
            }
            Keep::Log => {
                let (_, max, days) = self.rule(&topic);
                let oldest = stored.at.saturating_sub(days * 86400);
                let log = self.logs.entry(topic).or_default();
                log.push_back(stored);
                while log.len() as u64 > max || log.front().is_some_and(|first| first.at < oldest) {
                    log.pop_front();
                }
            }
        }
    }

    fn kept(&self) -> usize {
        self.last.len() + self.logs.values().map(VecDeque::len).sum::<usize>()
    }

    fn all(&self) -> Vec<&Stored> {
        let mut all: Vec<&Stored> = self
            .last
            .values()
            .chain(self.logs.values().flatten())
            .collect();
        all.sort_by_key(|stored| stored.message.id);
        all
    }

    /// Write the file again with only what is kept.
    fn compact(&mut self) -> std::io::Result<()> {
        let path = self.dir.join("messages.jsonl");
        let mut text = String::new();
        for stored in self.all() {
            text.push_str(&serde_json::to_string(stored).unwrap_or_default());
            text.push('\n');
        }
        let fresh = self.dir.join("messages.jsonl.new");
        std::fs::write(&fresh, text)?;
        std::fs::rename(&fresh, &path)?;
        self.file = Some(std::fs::OpenOptions::new().append(true).open(&path)?);
        self.file_lines = self.kept();
        Ok(())
    }

    fn reserve(&mut self) {
        self.reserved = self.next + BLOCK;
        let _ = std::fs::write(self.dir.join("next"), self.reserved.to_string());
    }
}

pub fn valid_topic(topic: &str) -> bool {
    !topic.is_empty()
        && topic.chars().count() <= 200
        && !topic.starts_with('/')
        && !topic.ends_with('/')
        && !topic.contains("//")
        && topic
            .chars()
            .all(|ch| !ch.is_whitespace() && !ch.is_control())
}

impl Channel {
    pub fn open(dir: &FsPath, topics: &[Topic]) -> std::io::Result<Channel> {
        std::fs::create_dir_all(dir)?;
        let reserved = std::fs::read_to_string(dir.join("next"))
            .ok()
            .and_then(|text| text.trim().parse().ok())
            .unwrap_or(1);
        let mut inner = Inner {
            dir: dir.to_owned(),
            next: reserved,
            reserved,
            topics: topics.to_vec(),
            last: BTreeMap::new(),
            logs: BTreeMap::new(),
            file: None,
            file_lines: 0,
        };
        let now = api::unix_now();
        for line in std::fs::read_to_string(dir.join("messages.jsonl"))
            .unwrap_or_default()
            .lines()
        {
            if let Ok(stored) = serde_json::from_str::<Stored>(line) {
                inner.next = inner.next.max(stored.message.id + 1);
                let (_, _, days) = inner.rule(&stored.message.topic);
                if stored.message.keep == Keep::Last || stored.at + days * 86400 >= now {
                    inner.keep(stored);
                }
            }
        }
        inner.compact()?;
        inner.reserve();
        Ok(Channel {
            inner: Mutex::new(inner),
            live: broadcast::channel(BEHIND).0,
        })
    }

    pub fn set_topics(&self, topics: &[Topic]) {
        self.inner.lock().unwrap().topics = topics.to_vec();
    }

    /// Publish. `own` is for berthd itself, which alone may use `berth/…`.
    fn publish(
        &self,
        topic: &str,
        body: serde_json::Value,
        keep: Option<Keep>,
        from: api::From,
        own: bool,
    ) -> R<Message> {
        if !valid_topic(topic) {
            return Err(E::bad(
                "A topic is a path of up to 200 characters without spaces",
            ));
        }
        if !own && (topic == "berth" || topic.starts_with("berth/")) {
            return Err(E::forbidden("Topics under berth/ are berthd's own"));
        }
        if body.to_string().len() > BODY {
            return Err(E(code::TOO_LARGE, "A message body is at most 64 KB".into()));
        }
        let mut inner = self.inner.lock().unwrap();
        if inner.next >= inner.reserved {
            inner.reserve();
        }
        let message = Message {
            id: inner.next,
            topic: topic.to_owned(),
            body,
            keep: keep.unwrap_or_else(|| inner.rule(topic).0),
            from,
            time: api::now(),
        };
        inner.next += 1;
        if message.keep != Keep::None {
            let stored = Stored {
                message: message.clone(),
                at: api::unix_now(),
            };
            if let (Some(file), Ok(line)) = (&mut inner.file, serde_json::to_string(&stored)) {
                let _ = writeln!(file, "{line}");
            }
            inner.file_lines += 1;
            inner.keep(stored);
            if inner.file_lines > 2 * inner.kept().max(100) {
                let _ = inner.compact();
            }
        }
        // Sent under the lock, so listeners see messages in the order of their ids.
        let _ = self.live.send(Arc::new(message.clone()));
        Ok(message)
    }

    pub fn internal(&self, topic: &str, body: serde_json::Value) {
        let from = api::From {
            level: Level::Host,
            uid: crate::users::own_uid(),
            session: None,
        };
        let _ = self.publish(topic, body, Some(Keep::None), from, true);
    }

    /// Kept messages of a prefix: after `since`, or with no `since` only each
    /// topic's `last`. With a subscription taken at the same moment.
    fn kept(
        &self,
        prefix: &str,
        since: Option<u64>,
        limit: usize,
    ) -> (Vec<Message>, bool, broadcast::Receiver<Arc<Message>>) {
        let inner = self.inner.lock().unwrap();
        let wanted = |stored: &&Stored| stored.message.topic.starts_with(prefix);
        let mut messages: Vec<Message> = match since {
            Some(since) => inner
                .all()
                .into_iter()
                .filter(wanted)
                .filter(|stored| stored.message.id > since)
                .map(|stored| stored.message.clone())
                .collect(),
            None => inner
                .last
                .values()
                .filter(wanted)
                .map(|stored| stored.message.clone())
                .collect(),
        };
        messages.sort_by_key(|message| message.id);
        let more = messages.len() > limit;
        messages.truncate(limit);
        (messages, more, self.live.subscribe())
    }
}

/// Who a caller is, with the session it runs in when it says so and that is true.
pub fn from(state: &State, caller: &Caller) -> api::From {
    let session = caller.pid.and_then(|pid| {
        let environment = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
        let id = environment
            .split(|byte| *byte == 0)
            .find_map(|entry| entry.strip_prefix(b"BERTH_SESSION="))?;
        let id = String::from_utf8(id.to_vec()).ok()?;
        state.sessions.get(&id).ok().map(|_| id)
    });
    api::From {
        level: caller.level,
        uid: caller.uid,
        session,
    }
}

pub async fn publish(
    AppState(state): AppState<Arc<State>>,
    Extension(caller): Extension<Caller>,
    Path(topic): Path<String>,
    Json(request): Json<api::Publish>,
) -> R<Json<Message>> {
    state
        .channel
        .publish(
            &topic,
            request.body,
            request.keep,
            from(&state, &caller),
            false,
        )
        .map(Json)
}

#[derive(serde::Deserialize)]
pub struct Read {
    #[serde(default)]
    topic: String,
    since: Option<u64>,
    limit: Option<usize>,
}

/// Kept messages, or with a WebSocket handshake those and then what arrives.
pub async fn read(
    AppState(state): AppState<Arc<State>>,
    Extension(caller): Extension<Caller>,
    Query(query): Query<Read>,
    upgrade: Result<ws::WebSocketUpgrade, ws::rejection::WebSocketUpgradeRejection>,
) -> Response {
    match upgrade {
        Ok(upgrade) => upgrade.on_upgrade(move |socket| listen(state, caller, socket, query)),
        Err(_) => {
            let limit = query.limit.unwrap_or(100).clamp(1, 1000);
            let (messages, more, _) =
                state
                    .channel
                    .kept(&query.topic, Some(query.since.unwrap_or(0)), limit);
            let next = more.then(|| messages.last().map_or(0, |message| message.id));
            Json(api::Messages { messages, next }).into_response()
        }
    }
}

async fn listen(state: Arc<State>, caller: Caller, socket: ws::WebSocket, query: Read) {
    let (mut sink, mut source) = socket.split();
    let (kept, _, mut live) = state.channel.kept(&query.topic, query.since, usize::MAX);
    let text = |value: &dyn erased::Json| ws::Message::Text(value.json().into());
    let mut sent = query.since.unwrap_or(0);
    for message in kept {
        sent = sent.max(message.id);
        if sink.send(text(&message)).await.is_err() {
            return;
        }
    }
    let from = from(&state, &caller);
    loop {
        tokio::select! {
            message = live.recv() => match message {
                Ok(message) => {
                    if message.id > sent && message.topic.starts_with(&query.topic) {
                        sent = message.id;
                        if sink.send(text(&*message)).await.is_err() {
                            break;
                        }
                    }
                }
                // Too far behind: the listener connects again with `since`.
                Err(_) => break,
            },
            frame = source.next() => match frame {
                Some(Ok(ws::Message::Text(frame))) => {
                    let outcome = serde_json::from_str::<api::PublishFrame>(&frame)
                        .map_err(|error| E::bad(error.to_string()))
                        .and_then(|frame| state.channel.publish(&frame.topic, frame.body, frame.keep, from.clone(), false));
                    if let Err(E(kind, message)) = outcome {
                        let error = serde_json::json!({"error": {"code": kind.1, "message": message}});
                        let _ = sink.send(text(&error)).await;
                    }
                }
                Some(Ok(_)) => {}
                Some(Err(_)) | None => break,
            },
        }
    }
    let _ = sink.close().await;
}

mod erased {
    pub trait Json {
        fn json(&self) -> String;
    }
    impl<T: serde::Serialize> Json for T {
        fn json(&self) -> String {
            serde_json::to_string(self).unwrap_or_default()
        }
    }
}
