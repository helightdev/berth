//! The host's own API, for the UI: one background thread owns the runtime and
//! the link, takes requests and sends back what happened. The UI never waits
//! for the network.

use berth_api::{self as api, client::Frame};
use berth_core::link::{Link, LinkSpec};
pub use berth_tui::host::{Event, HostView, NEEDS_API, Pending, Request, State, Transport, list};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::sync::{Arc, mpsc};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

/// The exit code an `exited` frame carries, when the text is one.
pub fn exited(text: &str) -> Option<Option<i32>> {
    berth_tui::host::exited(&serde_json::from_str(text).ok()?)
}

enum Out {
    Input(Vec<u8>),
    Resize(u16, u16),
}

/// How a host's version is compared with what would be applied now. It may
/// wait on the network, and runs off the runtime.
pub type Compare = Arc<dyn Fn(&api::Version) -> Option<api::Behind> + Send + Sync>;

async fn refresh(
    link: Arc<Link>,
    events: mpsc::Sender<Event>,
    all: bool,
    compare: Option<Compare>,
) {
    if all {
        match link.get::<api::Version>("/v1/version").await {
            Ok(version) => {
                let old = version.api < NEEDS_API;
                if let Some(compare) = compare {
                    let asked = version.clone();
                    let events = events.clone();
                    tokio::task::spawn_blocking(move || {
                        let _ = events.send(Event::Behind(compare(&asked)));
                    });
                }
                let _ = events.send(Event::Version(version));
                if old {
                    return;
                }
            }
            Err(error) => {
                let _ = events.send(Event::Failed {
                    missing: Link::missing(&error),
                    message: error.to_string(),
                });
                return;
            }
        }
        if let Ok(tasks) = link.get::<Value>("/v1/tasks").await {
            let _ = events.send(Event::Tasks(list(tasks, "tasks")));
        }
    }
    if let Ok(sessions) = link.get::<Value>("/v1/sessions").await {
        let _ = events.send(Event::Sessions(list(sessions, "sessions")));
    }
    if let Ok(stats) = link.get::<api::Stats>("/v1/stats").await {
        let _ = events.send(Event::Stats(Box::new(stats)));
    }
}

async fn follow(
    link: Arc<Link>,
    session: api::Session,
    events: mpsc::Sender<Event>,
    mut out: UnboundedReceiver<Out>,
) {
    let terminal = session.kind == api::SessionKind::Terminal;
    let id = session.id.clone();
    // A terminal replays its screen; a process is read from the start.
    let path = match terminal {
        true => format!("/v1/sessions/{id}"),
        false => format!("/v1/sessions/{id}?since=0"),
    };
    let mut stream = match link.stream(&path).await {
        Ok(stream) => stream,
        Err(error) => {
            // An ended session can still be read back.
            let output = link
                .get::<api::Output>(&format!("/v1/sessions/{id}/output"))
                .await;
            match output.ok().and_then(|output| api::unb64(&output.data_b64)) {
                Some(bytes) if !session.running() => {
                    let _ = events.send(Event::Output(id.clone(), bytes));
                    let _ = events.send(Event::Exited(id, session.exit_code));
                }
                _ => {
                    let _ = events.send(Event::Error(error.to_string()));
                    let _ = events.send(Event::Closed(id));
                }
            }
            return;
        }
    };
    loop {
        tokio::select! {
            frame = stream.next() => match frame {
                Some(Ok(Frame::Binary(bytes))) => {
                    let event = match (terminal, bytes.split_first()) {
                        (true, _) => Event::Output(id.clone(), bytes.to_vec()),
                        (false, Some((_, rest))) => Event::Output(id.clone(), rest.to_vec()),
                        (false, None) => continue,
                    };
                    if events.send(event).is_err() {
                        return;
                    }
                }
                Some(Ok(Frame::Text(text))) => {
                    if let Some(code) = exited(text.as_str()) {
                        let _ = events.send(Event::Exited(id.clone(), code));
                    }
                }
                Some(Ok(_)) => {}
                _ => {
                    let _ = events.send(Event::Closed(id));
                    return;
                }
            },
            message = out.recv() => {
                let frame = match message {
                    Some(Out::Input(bytes)) if terminal => Frame::Binary(bytes.into()),
                    Some(Out::Input(bytes)) => Frame::Binary([vec![0], bytes].concat().into()),
                    Some(Out::Resize(rows, cols)) => {
                        Frame::Text(json!({"resize": {"rows": rows, "cols": cols}}).to_string().into())
                    }
                    None => {
                        let _ = stream.close().await;
                        return;
                    }
                };
                if stream.send(frame).await.is_err() {
                    let _ = events.send(Event::Closed(id));
                    return;
                }
            }
        }
    }
}

async fn worker(
    link: Arc<Link>,
    mut requests: UnboundedReceiver<Request>,
    events: mpsc::Sender<Event>,
    compare: Option<Compare>,
) {
    let mut stream: Option<UnboundedSender<Out>> = None;
    while let Some(request) = requests.recv().await {
        let (link, events) = (link.clone(), events.clone());
        match request {
            Request::Refresh => drop(tokio::spawn(refresh(link, events, true, compare.clone()))),
            Request::Poll => drop(tokio::spawn(refresh(link, events, false, None))),
            Request::Run { name, body } => drop(tokio::spawn(async move {
                let _ = events.send(match link.run_task(&name, &body).await {
                    Ok(session) => Event::Opened(Box::new(session)),
                    Err(error) => Event::Error(error.to_string()),
                });
            })),
            Request::Start { body } => drop(tokio::spawn(async move {
                let _ = events.send(
                    match link.post::<api::Session>("/v1/sessions", &body).await {
                        Ok(session) => Event::Opened(Box::new(session)),
                        Err(error) => Event::Error(error.to_string()),
                    },
                );
            })),
            Request::Open(session) => {
                let (sender, receiver) = unbounded_channel();
                stream = Some(sender);
                tokio::spawn(follow(link, *session, events, receiver));
            }
            Request::Input(bytes) => {
                if let Some(stream) = &stream {
                    let _ = stream.send(Out::Input(bytes));
                }
            }
            Request::Resize(rows, cols) => {
                if let Some(stream) = &stream {
                    let _ = stream.send(Out::Resize(rows, cols));
                }
            }
            Request::Close => stream = None,
            Request::Delete(id) => drop(tokio::spawn(async move {
                let path = format!("/v1/sessions/{id}");
                if let Err(error) = link.call::<Value>("DELETE", &path, None).await {
                    let _ = events.send(Event::Error(error.to_string()));
                }
                refresh(link, events, false, None).await;
            })),
            Request::Choices { task, argument } => drop(tokio::spawn(async move {
                let path = format!(
                    "/v1/tasks/{}/choices/{}",
                    api::client::encode(&task),
                    api::client::encode(&argument)
                );
                let choices = match link.get::<Value>(&path).await {
                    Ok(value) => list(value, "choices"),
                    Err(_) => vec![],
                };
                let _ = events.send(Event::Choices {
                    task,
                    argument,
                    choices,
                });
            })),
        }
    }
}

/// The thread that talks to one host.
pub struct Remote {
    requests: UnboundedSender<Request>,
    events: mpsc::Receiver<Event>,
}

impl Remote {
    pub fn start(spec: LinkSpec, compare: Option<Compare>) -> std::io::Result<Remote> {
        let runtime = berth_core::link::runtime()?;
        let (requests, receiver) = unbounded_channel();
        let (sender, events) = mpsc::channel();
        std::thread::spawn(move || {
            runtime.block_on(worker(Arc::new(Link::new(spec)), receiver, sender, compare));
        });
        Ok(Remote { requests, events })
    }
}

impl Transport for Remote {
    fn send(&self, request: Request) {
        let _ = self.requests.send(request);
    }

    fn next(&self) -> Option<Event> {
        self.events.try_recv().ok()
    }
}

/// Start talking to the host over `spec`. Returns why not, if it cannot be.
#[cfg(test)]
pub fn connect(host: &mut HostView, spec: LinkSpec) -> Option<String> {
    connect_with(host, spec, None)
}

/// The same, comparing the host with what would be applied now.
pub fn connect_with(
    host: &mut HostView,
    spec: LinkSpec,
    compare: Option<Compare>,
) -> Option<String> {
    match Remote::start(spec, compare) {
        Ok(remote) => {
            host.connect(Box::new(remote));
            None
        }
        Err(error) => {
            host.unreachable(error.to_string());
            Some(error.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use berth_tui::host::{Arguments, View};
    use std::time::Duration;

    #[test]
    fn lists_come_bare_or_under_a_key() {
        let bare: Vec<api::Task> = list(json!([{"name": "status"}]), "tasks");
        let keyed: Vec<api::Task> = list(
            json!({"tasks": [{"name": "status"}, {"no": "name"}]}),
            "tasks",
        );
        assert_eq!((bare.len(), keyed.len()), (1, 1));
        assert_eq!(exited(r#"{"exited":{"exit_code":3}}"#), Some(Some(3)));
        assert_eq!(exited(r#"{"exited":{}}"#), Some(None));
        assert_eq!(exited(r#"{"dropped":{"frames":1}}"#), None);
    }

    #[test]
    fn a_transcript_is_shown_line_by_line() {
        let session: api::Session = serde_json::from_value(json!({
            "id": "s1", "kind": "process", "title": "demo", "command": ["true"], "user": "agent",
            "started_by": {"level": "host", "uid": 0}, "started": "2026-11-12T00:00:00Z",
        }))
        .unwrap();
        let mut view = View::new(&session, 4, 20, true, false);
        assert!(view.feed(b"one\ntwo").is_empty());
        view.feed(b"!\nthree\n");
        assert_eq!(view.screen().contents(), "one\ntwo!\nthree");
        assert!(view.running() && view.outcome().is_none());
        view.ended = Some(Some(2));
        assert_eq!(view.outcome(), Some(false));
        // A terminal answers a question about the cursor, also one in two pieces.
        let mut terminal = View::new(&session, 4, 20, true, false);
        terminal.terminal = true;
        assert!(terminal.feed(b"ab\x1b[6").is_empty());
        assert_eq!(terminal.feed(b"n"), b"\x1b[1;3R");
    }

    /// The overview against a real controller, when one has been built
    /// (`cargo build -p berthd`): connect, list, run tasks, type into one.
    #[test]
    fn the_overview_talks_to_a_controller() {
        let built = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("berthd");
        if !built.is_file() {
            eprintln!("skipped: {} is not built", built.display());
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("etc")).unwrap();
        let tasks = json!([
            {"name": "status", "label": "Status", "group": "System", "user": "agent", "run": "echo up; echo warn >&2"},
            {"name": "demo:status", "label": "Demo", "extends": ["status"], "user": "agent", "run": "echo demo; exit 4"},
            {"name": "greet", "label": "Greet", "user": "agent", "run": "echo \"hello $BERTH_ARG_WHO\"",
             "arguments": [{"name": "who", "required": true, "choices": "printf 'ann\\nbob\\n'"}]},
            {"name": "ask", "label": "Ask", "user": "agent", "terminal": true, "run": "printf 'code? '; read c; echo got $c"},
        ]);
        std::fs::write(dir.path().join("etc/tasks.json"), tasks.to_string()).unwrap();
        let mut daemon = std::process::Command::new(&built)
            .arg("--single")
            .arg(dir.path())
            .spawn()
            .unwrap();
        struct Stop<'a>(&'a mut std::process::Child);
        impl Drop for Stop<'_> {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let _stop = Stop(&mut daemon);
        for _ in 0..100 {
            if dir.path().join("control.sock").exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let spec = LinkSpec {
            program: built.to_string_lossy().into_owned(),
            args: vec![
                "connect".into(),
                "--dir".into(),
                dir.path().to_string_lossy().into_owned(),
            ],
            first_line: None,
            via: None,
            ..LinkSpec::default()
        };
        let mut host = HostView::default();
        assert!(connect(&mut host, spec).is_none());
        fn until(host: &mut HostView, what: &str, done: impl Fn(&HostView) -> bool) {
            for _ in 0..400 {
                if let Some(note) = host.pump() {
                    eprintln!("note: {note}");
                }
                if done(host) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            panic!("never: {what}; state {:?}", host.state);
        }
        until(&mut host, "ready with tasks", |host| {
            matches!(host.state, State::Ready(_)) && host.tasks.len() == 4
        });
        until(&mut host, "stats", |host| host.stats.is_some());
        assert!(host.stats_line().unwrap().contains("load "));
        assert!(host.tasks[0].run.is_empty() && host.tasks[1].extends == ["status"]);

        // A task and the step a plugin adds, as one transcript; the first
        // failing step is the outcome.
        host.run("status", json!({}));
        until(&mut host, "status ended", |host| {
            host.view.as_ref().is_some_and(|view| !view.running())
        });
        let view = host.view.as_ref().unwrap();
        assert!(view.ours && !view.terminal && view.title == "Status");
        let text = view.screen().contents();
        assert!(
            text.contains("up") && text.contains("warn") && text.contains("demo"),
            "{text}"
        );
        assert_eq!(view.ended, Some(Some(4)));
        let id = view.id.clone();
        // Closing a finished one that was started here forgets it.
        host.close(true);
        until(&mut host, "forgotten", |host| {
            host.sessions.iter().all(|session| session.id != id)
        });

        // Arguments reach the command; the host offers choices for one.
        host.send(Request::Choices {
            task: "greet".into(),
            argument: "who".into(),
        });
        host.arguments = Some(Arguments {
            action: 1,
            index: 0,
            values: Default::default(),
            input: Default::default(),
            choices: vec![],
            chosen: None,
            secret: false,
        });
        until(&mut host, "choices", |host| {
            host.arguments.as_ref().unwrap().choices == ["ann", "bob"]
        });
        host.arguments = None;
        host.run("greet", json!({"arguments": {"who": "a 'quoted' b"}}));
        until(&mut host, "greet ended", |host| {
            host.view.as_ref().is_some_and(|view| !view.running())
        });
        assert!(
            host.view
                .as_ref()
                .unwrap()
                .screen()
                .contents()
                .contains("hello a 'quoted' b")
        );
        host.close(false);
        until(&mut host, "kept", |host| {
            host.sessions.iter().any(|session| session.title == "Greet")
        });

        // A terminal task takes typing; left with Ctrl+Q it runs on and can be
        // opened again.
        host.run("ask", json!({"rows": 10, "cols": 40}));
        until(&mut host, "the question", |host| {
            host.view
                .as_ref()
                .is_some_and(|view| view.screen().contents().contains("code?"))
        });
        assert!(host.view.as_ref().unwrap().terminal);
        host.close(false);
        until(&mut host, "listed as running", |host| {
            host.sessions
                .iter()
                .any(|session| session.title == "Ask" && session.running())
        });
        let session = host
            .sessions
            .iter()
            .find(|session| session.title == "Ask")
            .unwrap()
            .clone();
        host.open(&session);
        until(&mut host, "the question again", |host| {
            host.view
                .as_ref()
                .is_some_and(|view| view.screen().contents().contains("code?"))
        });
        host.input(b"42\r".to_vec());
        until(&mut host, "the answer", |host| {
            host.view.as_ref().is_some_and(|view| !view.running())
        });
        let view = host.view.as_ref().unwrap();
        assert!(view.screen().contents().contains("got 42") && view.outcome() == Some(true));
        assert!(!view.ours);

        // A task the host does not have says so.
        host.close(true);
        host.run("nothing", json!({}));
        let mut said = None;
        for _ in 0..200 {
            said = host.pump();
            if said.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        assert!(said.is_some() && host.view.is_none());
    }

    #[test]
    fn a_host_without_a_controller_is_told_apart() {
        let spec = |script: &str| LinkSpec {
            program: "sh".into(),
            args: vec!["-c".into(), script.into()],
            first_line: None,
            via: None,
            ..LinkSpec::default()
        };
        let state = |script: &str, password: Option<&str>| {
            let mut host = HostView::default();
            host.password = password.map(str::to_owned);
            connect(&mut host, spec(script));
            for _ in 0..400 {
                host.pump();
                if host.state != State::Connecting {
                    break;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            (host.state, host.password)
        };
        assert_eq!(
            state("echo 'sudo: berthd: command not found' >&2; exit 1", None).0,
            State::Missing
        );
        assert_eq!(
            state("echo 'sudo: a password is required' >&2; exit 1", None).0,
            State::Password
        );
        // A password sudo did not take is forgotten, to be asked for again.
        let wrong = state(
            "echo 'Sorry, try again.' >&2; echo 'sudo: 1 incorrect password attempt' >&2; exit 1",
            Some("x"),
        );
        assert_eq!(wrong, (State::Password, None));
        let (other, _) = state(
            "echo 'ssh: connect to host box port 22: Connection timed out' >&2; exit 255",
            None,
        );
        assert!(matches!(other, State::Unreachable(message) if message.contains("timed out")));
    }
}
