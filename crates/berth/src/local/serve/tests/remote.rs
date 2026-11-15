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
