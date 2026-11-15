//! The screens against a berth serve of their own over the network: a setup
//! key signs them in, and its portal answers.

use super::*;
use berth_page::device::Linked;
use berth_page::portal::{Link, Portal};
use berth_page::settings::{self, Settings};
use berth_tui::{Code, Event, Key};
use ratatui::{Terminal as Screen, backend::TestBackend};
use std::cell::RefCell;
use std::rc::Rc;

fn screen(app: &mut berth_page::App) -> String {
    let mut terminal = Screen::new(TestBackend::new(120, 30)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let buffer = terminal.backend().buffer();
    (0..30)
        .map(|y| {
            (0..120)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Take in what the network says until `done`, for up to ten seconds.
fn until(what: &str, mut done: impl FnMut() -> bool) {
    let started = std::time::Instant::now();
    while !done() {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "no {what} in time"
        );
        berth_page::pump();
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_screens_sign_in_with_a_setup_key_and_use_the_portal() {
    let dir = tempfile::tempdir().unwrap();
    let state = state_in(dir.path(), Some("s3cret-token"));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let router = api::router(state.clone(), true);
    tokio::spawn(async move { axum::serve(listener, router).await });
    let (key, _) = super::super::setup_key::make(&state.paths).unwrap();

    let client = tempfile::tempdir().unwrap();
    let kept = client.path().join("berth");
    let linked = Linked {
        server: Some(url.clone()),
        token: None,
        setup_key: Some(key),
    };
    berth_page::device::install(linked, Settings::new(Some(kept.clone())));

    // The key collects the token, which is kept for the next run.
    let mut app = berth_page::App::new();
    until("token", || {
        app.tick();
        screen(&mut app).contains("is installed and serves at")
    });
    let settings = Settings::new(Some(kept.clone()));
    assert_eq!(
        settings.get(settings::TOKEN).as_deref(),
        Some("s3cret-token")
    );
    assert_eq!(
        settings.get(settings::SERVER).as_deref(),
        Some(url.as_str())
    );

    // Done ends the key and shows the hosts of the server.
    app.event(Event::Key(Key {
        code: Code::Enter,
        ctrl: false,
        alt: false,
        shift: false,
    }));
    until("end of the key", || {
        app.tick();
        state
            .store
            .read(kind::TOKEN, store::OWN, super::super::setup_key::NAME)
            .unwrap()
            .is_none()
    });
    until("hosts", || {
        app.tick();
        let shown = screen(&mut app);
        shown.contains("Hosts") && !shown.contains("is installed")
    });

    // The portal answers with the token, and a wrong one is refused.
    let portal = Portal::open(&url, "s3cret-token").unwrap();
    let hosts = Rc::new(RefCell::new(None));
    let answer = hosts.clone();
    portal.call("hosts.list", None, json!({}), Value::Null, move |result| {
        *answer.borrow_mut() = Some(result)
    });
    until("answer", || hosts.borrow().is_some());
    assert_eq!(portal.link(), Link::Open);
    assert_eq!(hosts.borrow_mut().take().unwrap(), Ok(json!([])));
    let refused = Portal::open(&url, "wrong").unwrap();
    until("refusal", || refused.link() != Link::Connecting);
    assert_eq!(refused.link(), Link::Refused);
}

/// A server of berth 0.5.2 as its portal answers: every call with its version.
async fn older_server(token: &'static str) -> String {
    use axum::extract::ws::{Message, WebSocketUpgrade};
    let portal = axum::routing::get(move |ws: WebSocketUpgrade| async move {
        ws.protocols([token]).on_upgrade(|mut socket| async move {
            while let Some(Ok(Message::Text(text))) = socket.recv().await {
                let asked: Value = serde_json::from_str(text.as_str()).unwrap();
                let said = json!({"id": asked["id"], "result": {"version": "0.5.2", "api": 1, "mode": "serve"}});
                if socket.send(Message::Text(said.to_string().into())).await.is_err() {
                    break;
                }
            }
        })
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let router = axum::Router::new().route(berth_api::ops::PORTAL, portal);
    tokio::spawn(async move { axum::serve(listener, router).await });
    url
}

#[tokio::test(flavor = "multi_thread")]
async fn a_client_refuses_an_older_server_and_still_reads_its_version() {
    use crate::cli::{args::Global, client::Client, fail};
    let url = older_server("s3cret-token").await;
    let dir = tempfile::tempdir().unwrap();
    let token = dir.path().join("token");
    std::fs::write(&token, "s3cret-token").unwrap();
    let global = Global {
        server: Some(url.clone()),
        token_file: Some(token),
        ..Global::default()
    };
    let kept = Settings::new(Some(dir.path().join("berth")));
    let (opened, version) = tokio::task::spawn_blocking(move || {
        let opened = Client::open_in(&global, &kept).map(|_| ());
        let version = Client::open_any(&global, &kept).and_then(|client| client.server_version());
        (opened, version)
    })
    .await
    .unwrap();
    let refused = opened.unwrap_err();
    assert_eq!(refused.status, fail::REFUSED);
    assert!(
        refused.message.contains("is berth 0.5.2 (API 1), older than this berth")
            && refused.message.contains("Update the server first"),
        "{}",
        refused.message
    );
    assert_eq!(version.unwrap(), ("0.5.2".to_owned(), 1));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_screens_say_that_an_older_server_is_updated_first() {
    let url = older_server("s3cret-token").await;
    let client = tempfile::tempdir().unwrap();
    let kept = client.path().join("berth");
    let linked = Linked {
        server: Some(url.clone()),
        token: Some("s3cret-token".into()),
        setup_key: None,
    };
    berth_page::device::install(linked, Settings::new(Some(kept.clone())));
    let mut app = berth_page::App::new();
    until("the older server", || {
        app.tick();
        screen(&mut app).contains("older than this berth")
    });
    // The token stays for when the server is updated.
    assert_eq!(
        Settings::new(Some(kept)).get(settings::TOKEN).as_deref(),
        Some("s3cret-token")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_screens_keep_the_token_of_a_server_that_does_not_answer_or_refuses_it() {
    // Nobody listens at this address.
    let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let away = format!("http://{}", closed.local_addr().unwrap());
    drop(closed);
    let client = tempfile::tempdir().unwrap();
    let kept = client.path().join("berth");
    let settings = Settings::new(Some(kept.clone()));
    settings
        .set(&[(settings::SERVER, &away), (settings::TOKEN, "s3cret-kept")])
        .unwrap();
    let linked = Linked {
        server: None,
        token: None,
        setup_key: None,
    };
    berth_page::device::install(linked, Settings::new(Some(kept.clone())));
    let mut app = berth_page::App::new();
    let started = std::time::Instant::now();
    while started.elapsed() < Duration::from_secs(2) {
        app.tick();
        berth_page::pump();
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(settings.get(settings::TOKEN).as_deref(), Some("s3cret-kept"));
    drop(app);

    // A server that refuses it asks for another, and the kept one stays until then.
    let dir = tempfile::tempdir().unwrap();
    let state = state_in(dir.path(), Some("s3cret-token"));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let router = api::router(state, true);
    tokio::spawn(async move { axum::serve(listener, router).await });
    settings
        .set(&[(settings::SERVER, &url), (settings::TOKEN, "s3cret-kept")])
        .unwrap();
    let linked = Linked {
        server: None,
        token: None,
        setup_key: None,
    };
    berth_page::device::install(linked, Settings::new(Some(kept.clone())));
    let mut app = berth_page::App::new();
    until("the refusal", || {
        app.tick();
        screen(&mut app).contains("did not take that token")
    });
    assert_eq!(settings.get(settings::TOKEN).as_deref(), Some("s3cret-kept"));
}
