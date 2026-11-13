//! The store's tests, run on each backend: on SQLite always, and on
//! PostgreSQL when `BERTH_TEST_POSTGRES` holds the URL of a database to test
//! in. Each PostgreSQL test works in a schema of its own, dropped after it.

use super::*;
use berth_api::{From, Level};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Where one test keeps its store.
enum Place {
    Sqlite(tempfile::TempDir),
    Postgres { url: String, schema: String },
}

impl Place {
    fn sqlite() -> Place {
        Place::Sqlite(tempfile::tempdir().unwrap())
    }

    /// A schema of its own in the test database, if there is one.
    fn postgres() -> Option<Place> {
        let base = std::env::var("BERTH_TEST_POSTGRES").ok()?;
        if base.trim().is_empty() {
            return None;
        }
        static COUNT: AtomicUsize = AtomicUsize::new(0);
        let schema = format!(
            "berth_test_{}_{}_{}",
            std::process::id(),
            COUNT.fetch_add(1, Ordering::Relaxed),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        );
        postgres::execute(&base, &format!("CREATE SCHEMA {schema}")).unwrap();
        // Its connections find its tables first and say whose they are.
        let separator = if base.contains('?') { '&' } else { '?' };
        let url =
            format!("{base}{separator}options=-csearch_path%3D{schema}&application_name={schema}");
        Some(Place::Postgres { url, schema })
    }

    fn target(&self) -> Target {
        match self {
            Place::Sqlite(dir) => Target::Sqlite(dir.path().join("berth.db")),
            Place::Postgres { url, .. } => Target::Postgres(url.clone()),
        }
    }

    fn open(&self) -> Store {
        Store::connect(&self.target()).unwrap()
    }
}

impl Drop for Place {
    fn drop(&mut self) {
        if let Place::Postgres { url, schema } = self {
            let _ = postgres::execute(url, &format!("DROP SCHEMA {schema} CASCADE"));
        }
    }
}

/// Each test as a test on SQLite and one on PostgreSQL.
macro_rules! on_each_backend {
    ($($name:ident),* $(,)?) => {
        mod on_sqlite {
            $( #[test] fn $name() { super::$name(&super::Place::sqlite()); } )*
        }
        mod on_postgres {
            $(
                #[test]
                fn $name() {
                    if let Some(place) = super::Place::postgres() {
                        super::$name(&place);
                    }
                }
            )*
        }
    };
}

on_each_backend!(
    messages_are_taken_in_once_and_the_cursor_survives_a_restart,
    the_inbox_filters_pages_and_marks,
    two_writers_on_one_key_and_one_loses,
    old_messages_and_jobs_are_pruned_and_values_stay,
    a_store_is_up_to_date_and_says_what_it_is,
    state_rows_are_kept_listed_and_removed,
    replacing_states_leaves_other_scopes_and_kinds_alone,
    changes_are_all_or_nothing,
    sealed_values_round_trip,
    a_sealed_store_without_its_key_says_so,
    plain_rows_are_read_with_a_key_loaded,
    resealing_keeps_the_times,
    resealing_refuses_a_table_that_changed,
    the_old_files_go_into_an_empty_store,
    a_copy_takes_every_table_as_it_is_and_back,
    a_copy_into_a_store_that_has_rows_keeps_them,
);

fn message(id: u64, topic: &str, keep: Keep) -> Message {
    Message {
        id,
        topic: topic.into(),
        body: json!({"n": id}),
        keep,
        from: From {
            level: Level::Agent,
            uid: 1001,
            session: Some("sabc".into()),
        },
        time: rfc3339(unix_now()),
    }
}

fn all(store: &Store) -> Vec<Stored> {
    store
        .inbox(&Filter {
            limit: 1000,
            ..Default::default()
        })
        .unwrap()
        .0
}

fn messages_are_taken_in_once_and_the_cursor_survives_a_restart(place: &Place) {
    let store = place.open();
    assert_eq!(store.cursor("server").unwrap(), 0);
    assert!(
        store
            .take_in("server", &message(1, "a/x", Keep::Log))
            .unwrap()
            .is_some()
    );
    assert!(
        store
            .take_in("server", &message(2, "a/y", Keep::Log))
            .unwrap()
            .is_some()
    );
    // A message that is only handed on moves the cursor and is not kept.
    assert!(
        store
            .take_in("server", &message(3, "a/x", Keep::None))
            .unwrap()
            .is_none()
    );
    drop(store);

    let store = place.open();
    assert_eq!(store.cursor("server").unwrap(), 3);
    // The host sends one again: nothing new, and the cursor does not go back.
    assert!(
        store
            .take_in("server", &message(2, "a/y", Keep::Log))
            .unwrap()
            .is_none()
    );
    assert_eq!(store.cursor("server").unwrap(), 3);
    // Ids are per host.
    assert!(
        store
            .take_in("laptop", &message(1, "b/x", Keep::Log))
            .unwrap()
            .is_some()
    );
    let found = all(&store);
    let ids: Vec<_> = found
        .iter()
        .map(|m| (m.host.as_str(), m.message.id))
        .collect();
    assert_eq!(ids, [("laptop", 1), ("server", 2), ("server", 1)]);
    assert_eq!(
        found.iter().map(|m| m.seq).collect::<Vec<_>>(),
        [3, 2, 1],
        "seq counts the messages taken in"
    );
    assert_eq!(found[1].message.body, json!({"n": 2}));
    assert_eq!(found[1].message.from.session.as_deref(), Some("sabc"));
    assert_eq!(found[1].message.from.level, Level::Agent);
    assert_eq!(found[1].message.from.uid, 1001);

    // A state replaces the one before it on its topic.
    store
        .take_in("server", &message(4, "state", Keep::Last))
        .unwrap();
    store
        .take_in("server", &message(5, "state", Keep::Last))
        .unwrap();
    let filter = Filter {
        topic: Some("state"),
        limit: 10,
        ..Default::default()
    };
    let states = store.inbox(&filter).unwrap().0;
    assert_eq!(states.len(), 1);
    assert_eq!(
        (states[0].message.id, states[0].message.keep),
        (5, Keep::Last)
    );

    // A host whose counter started again is read from the start.
    store.reset_cursor("server").unwrap();
    assert_eq!(store.cursor("server").unwrap(), 0);
    assert_eq!(store.cursor("laptop").unwrap(), 1);
}

fn the_inbox_filters_pages_and_marks(place: &Place) {
    let store = place.open();
    for id in 1..=5 {
        let topic = if id % 2 == 0 { "even/n" } else { "odd/n" };
        store
            .take_in("server", &message(id, topic, Keep::Log))
            .unwrap();
    }
    let odd = Filter {
        topic: Some("odd/"),
        limit: 10,
        ..Default::default()
    };
    assert_eq!(store.inbox(&odd).unwrap().0.len(), 3);
    // A prefix is not a pattern.
    for pattern in ["%", "_", "odd/%"] {
        let pattern = Filter {
            topic: Some(pattern),
            limit: 10,
            ..Default::default()
        };
        assert!(store.inbox(&pattern).unwrap().0.is_empty());
    }
    let other = Filter {
        host: Some("laptop"),
        limit: 10,
        ..Default::default()
    };
    assert!(store.inbox(&other).unwrap().0.is_empty());

    let (page, next) = store
        .inbox(&Filter {
            limit: 2,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(
        page.iter().map(|m| m.message.id).collect::<Vec<_>>(),
        [5, 4]
    );
    let (page, next) = store
        .inbox(&Filter {
            limit: 2,
            before: next,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(
        page.iter().map(|m| m.message.id).collect::<Vec<_>>(),
        [3, 2]
    );
    let (page, next) = store
        .inbox(&Filter {
            limit: 2,
            before: next,
            ..Default::default()
        })
        .unwrap();
    assert_eq!((page.len(), next), (1, None));

    assert!(store.set_done("server", 3, true).unwrap());
    assert!(!store.set_done("server", 99, true).unwrap());
    let open = Filter {
        done: Some(false),
        limit: 10,
        ..Default::default()
    };
    assert_eq!(store.inbox(&open).unwrap().0.len(), 4);
    let done = Filter {
        done: Some(true),
        limit: 10,
        ..Default::default()
    };
    assert_eq!(store.inbox(&done).unwrap().0[0].message.id, 3);
    assert!(store.set_done("server", 3, false).unwrap());
    assert_eq!(store.inbox(&open).unwrap().0.len(), 5);
}

fn two_writers_on_one_key_and_one_loses(place: &Place) {
    let first = place.open();
    let second = place.open();
    assert_eq!(
        first
            .put("ui", "layout", &json!({"columns": 2}), When::New)
            .unwrap(),
        1
    );
    assert!(matches!(
        second.put("ui", "layout", &json!(1), When::New),
        Err(Error::Conflict(Some(1)))
    ));
    // Both read revision 1; the first to write wins.
    assert_eq!(
        first
            .put("ui", "layout", &json!({"columns": 3}), When::At(1))
            .unwrap(),
        2
    );
    assert!(matches!(
        second.put("ui", "layout", &json!({"columns": 4}), When::At(1)),
        Err(Error::Conflict(Some(2)))
    ));
    let entry = second.get("ui", "layout").unwrap().unwrap();
    assert_eq!((entry.value, entry.revision), (json!({"columns": 3}), 2));
    assert!(matches!(
        second.put("ui", "missing", &json!(1), When::At(1)),
        Err(Error::Conflict(None))
    ));

    first.put("ui", "a", &json!("x"), When::Always).unwrap();
    assert_eq!(first.put("ui", "a", &json!("y"), When::Always).unwrap(), 2);
    let keys: Vec<_> = first
        .list("ui")
        .unwrap()
        .into_iter()
        .map(|entry| entry.key)
        .collect();
    assert_eq!(keys, ["a", "layout"]);
    assert!(first.list("other").unwrap().is_empty());
    assert!(matches!(
        first.remove("ui", "layout", When::At(1)),
        Err(Error::Conflict(Some(2)))
    ));
    assert!(matches!(
        first.remove("ui", "missing", When::At(1)),
        Err(Error::Conflict(None))
    ));
    assert!(first.remove("ui", "layout", When::Always).unwrap());
    assert!(!first.remove("ui", "layout", When::Always).unwrap());
    assert!(first.get("ui", "layout").unwrap().is_none());
    let large = json!("x".repeat(VALUE_LIMIT));
    assert!(matches!(
        first.put("ui", "big", &large, When::Always),
        Err(Error::TooLarge)
    ));

    // Writers at the same time: each conditional write that wins moves the
    // revision by one, and every other one is told it lost.
    first.put("race", "n", &json!(0), When::New).unwrap();
    let wins: usize = std::thread::scope(|scope| {
        let writers: Vec<_> = [&first, &second]
            .into_iter()
            .map(|store| {
                scope.spawn(move || {
                    let mut wins = 0;
                    for _ in 0..20 {
                        let at = store.get("race", "n").unwrap().unwrap().revision;
                        match store.put("race", "n", &json!(at), When::At(at)) {
                            Ok(revision) => {
                                assert_eq!(revision, at + 1);
                                wins += 1;
                            }
                            Err(Error::Conflict(Some(_))) => {}
                            Err(error) => panic!("{error}"),
                        }
                    }
                    wins
                })
            })
            .collect();
        writers
            .into_iter()
            .map(|writer| writer.join().unwrap())
            .sum()
    });
    let entry = first.get("race", "n").unwrap().unwrap();
    assert_eq!(entry.revision as usize, wins + 1);
}

fn old_messages_and_jobs_are_pruned_and_values_stay(place: &Place) {
    let store = place.open();
    let mut old = message(1, "a", Keep::Log);
    old.time = rfc3339(unix_now() - 100 * 86400);
    store.take_in("server", &old).unwrap();
    store
        .take_in("server", &message(2, "a", Keep::Log))
        .unwrap();
    store.put("ui", "k", &json!(1), When::Always).unwrap();
    store.write("note", "server", "n", b"kept").unwrap();
    let job = |id: &str, age: u64, ended: bool| Job {
        id: id.into(),
        host: "server".into(),
        what: "apply".into(),
        started: rfc3339(unix_now() - age * 86400),
        ended: ended.then(|| rfc3339(unix_now() - age * 86400)),
        exit_code: ended.then_some(0),
        output_path: format!("/tmp/{id}.log"),
    };
    store.add_job(&job("jold", 100, true)).unwrap();
    store.add_job(&job("jrunning", 100, false)).unwrap();
    store.add_job(&job("jnew", 1, true)).unwrap();
    assert_eq!(store.prune(RETENTION_DAYS).unwrap(), ["/tmp/jold.log"]);
    assert_eq!(all(&store).len(), 1);
    let left: Vec<_> = store
        .jobs(10)
        .unwrap()
        .into_iter()
        .map(|job| job.id)
        .collect();
    assert_eq!(left, ["jnew", "jrunning"]);
    assert!(store.get("ui", "k").unwrap().is_some());
    assert!(store.read("note", "server", "n").unwrap().is_some());
    // The cursor is not what was pruned.
    assert_eq!(store.cursor("server").unwrap(), 2);

    store.end_job("jrunning", Some(3)).unwrap();
    let ended = store.job("jrunning").unwrap().unwrap();
    assert_eq!(ended.exit_code, Some(3));
    assert!(ended.ended.is_some());
    assert!(store.job("nope").unwrap().is_none());
    // A job ends once.
    store.end_job("jrunning", Some(4)).unwrap();
    assert_eq!(store.job("jrunning").unwrap().unwrap().exit_code, Some(3));

    store.add_job(&job("jlost", 0, false)).unwrap();
    assert_eq!(store.end_lost_jobs().unwrap(), 1);
    let lost = store.job("jlost").unwrap().unwrap();
    assert_eq!((lost.ended.is_some(), lost.exit_code), (true, None));
    assert_eq!(store.end_lost_jobs().unwrap(), 0);
}

fn a_store_is_up_to_date_and_says_what_it_is(place: &Place) {
    let store = place.open();
    let (name, steps) = match place {
        Place::Sqlite(_) => ("sqlite", sqlite::MIGRATIONS.len()),
        Place::Postgres { .. } => ("postgres", postgres::MIGRATIONS.len()),
    };
    assert_eq!(store.backend_name(), name);
    assert_eq!(store.schema_version().unwrap(), steps as i64);
    // Both say the same version for the same tables.
    assert_eq!(sqlite::MIGRATIONS.len(), postgres::MIGRATIONS.len());
    drop(store);
    // Opening it again changes nothing.
    assert_eq!(place.open().schema_version().unwrap(), steps as i64);
}

fn keys() -> seal::Keys {
    seal::Keys::new(seal::Key::generate().unwrap(), vec![])
}

fn copy(keys: &seal::Keys) -> seal::Keys {
    seal::Keys::new(
        seal::Key::from_hex(&keys.current().to_hex()).unwrap(),
        vec![],
    )
}

/// The row as it is in the table.
fn stored_row(store: &Store, kind: &str, scope: &str, name: &str) -> StateRow {
    store.backend().state(kind, scope, name).unwrap().unwrap()
}

fn all_names(store: &Store) -> Vec<(String, String, String)> {
    let rows = store.backend().all_states().unwrap();
    rows.into_iter()
        .map(|row| (row.kind, row.scope, row.name))
        .collect()
}

fn owned(names: &[(&str, &str, &str)]) -> Vec<(String, String, String)> {
    names
        .iter()
        .map(|(kind, scope, name)| ((*kind).into(), (*scope).into(), (*name).into()))
        .collect()
}

fn state_rows_are_kept_listed_and_removed(place: &Place) {
    let store = place.open();
    assert!(store.read("secret", "studio", "token").unwrap().is_none());
    // Bytes that are not text.
    store
        .write("secret", "studio", "token", &[0, 159, 146, 150, 255])
        .unwrap();
    store.write("secret", "studio", "b-key", b"plain").unwrap();
    store.write("secret", "laptop", "token", b"other").unwrap();
    store
        .write("profile", "studio", "token", b"another kind")
        .unwrap();

    assert_eq!(
        store.read("secret", "studio", "token").unwrap().unwrap(),
        [0, 159, 146, 150, 255]
    );
    let row = stored_row(&store, "secret", "studio", "token");
    assert!(!row.sealed, "without a key a value is plain");
    assert!(row.updated.ends_with('Z'), "{}", row.updated);

    assert_eq!(store.names("secret", "studio").unwrap(), ["b-key", "token"]);
    let entries = store.entries("secret", "studio").unwrap();
    assert_eq!(entries[0], ("b-key".to_owned(), b"plain".to_vec()));

    // Writing a value again replaces it.
    store.write("secret", "studio", "token", b"new").unwrap();
    assert_eq!(
        store.read("secret", "studio", "token").unwrap().unwrap(),
        b"new"
    );
    assert_eq!(store.names("secret", "studio").unwrap().len(), 2);

    store
        .write_json(kind::SETTING, OWN, "retention", &json!({"days": 7}))
        .unwrap();
    assert_eq!(
        store.read_json(kind::SETTING, OWN, "retention").unwrap(),
        Some(json!({"days": 7}))
    );
    assert!(store.read_json("secret", "studio", "b-key").is_err());

    assert!(store.forget("secret", "studio", "token").unwrap());
    assert!(!store.forget("secret", "studio", "token").unwrap());
    assert!(store.read("secret", "studio", "token").unwrap().is_none());
    assert!(store.read("secret", "laptop", "token").unwrap().is_some());
    assert!(store.read("profile", "studio", "token").unwrap().is_some());
}

fn replacing_states_leaves_other_scopes_and_kinds_alone(place: &Place) {
    let store = place.open().with_keys(Some(keys()), None);
    for (kind, scope, name) in [
        ("profile", "studio", "a"),
        ("profile", "studio", "b"),
        ("link", "studio", "a"),
        ("secret", "studio", "a"),
        ("profile", "laptop", "a"),
    ] {
        store.write(kind, scope, name, b"old").unwrap();
    }
    store
        .replace(
            "studio",
            &["profile", "link"],
            &[
                ("profile", "c", b"new".to_vec()),
                ("link", "a", b"new".to_vec()),
            ],
        )
        .unwrap();
    assert_eq!(
        all_names(&store),
        owned(&[
            ("link", "studio", "a"),
            ("profile", "laptop", "a"),
            ("profile", "studio", "c"),
            ("secret", "studio", "a"),
        ])
    );
    assert_eq!(store.read("link", "studio", "a").unwrap().unwrap(), b"new");
    assert!(stored_row(&store, "link", "studio", "a").sealed);
    assert_eq!(
        store.read("profile", "laptop", "a").unwrap().unwrap(),
        b"old"
    );

    // Nothing to put empties the kinds of the scope.
    store.replace("studio", &["profile"], &[]).unwrap();
    assert!(store.names("profile", "studio").unwrap().is_empty());
    assert_eq!(all_names(&store).len(), 3);
}

fn changes_are_all_or_nothing(place: &Place) {
    let store = place.open().with_keys(Some(keys()), None);
    store.write("profile", OWN, "studio", b"1").unwrap();
    store.write("credentials", OWN, "studio", b"2").unwrap();
    // A rename: the new name written and the old one gone, together.
    store
        .change(
            &[
                ("profile", OWN, "lab", b"1".to_vec()),
                ("credentials", OWN, "lab", b"2".to_vec()),
            ],
            &[
                ("profile", OWN, "studio"),
                ("credentials", OWN, "studio"),
                ("instance", OWN, "missing"),
            ],
        )
        .unwrap();
    assert_eq!(
        all_names(&store),
        owned(&[("credentials", "", "lab"), ("profile", "", "lab")])
    );
    assert_eq!(
        store.read("credentials", OWN, "lab").unwrap().unwrap(),
        b"2"
    );

    // A write the database refuses, here through a trigger, undoes the whole change.
    let before = all_names(&store);
    match place {
        Place::Sqlite(dir) => {
            let db = rusqlite::Connection::open(dir.path().join("berth.db")).unwrap();
            db.execute_batch(
                "CREATE TRIGGER refuse BEFORE INSERT ON state WHEN NEW.name = 'refused'
                 BEGIN SELECT RAISE(ABORT, 'refused'); END;",
            )
            .unwrap();
        }
        Place::Postgres { url, schema } => {
            postgres::execute(
                url,
                &format!(
                    "CREATE FUNCTION {schema}.refuse() RETURNS trigger LANGUAGE plpgsql AS
                     $$ BEGIN IF NEW.name = 'refused' THEN RAISE EXCEPTION 'refused'; END IF; RETURN NEW; END $$;
                     CREATE TRIGGER refuse BEFORE INSERT ON {schema}.state
                     FOR EACH ROW EXECUTE FUNCTION {schema}.refuse();"
                ),
            )
            .unwrap();
        }
    }
    assert!(
        store
            .change(
                &[
                    ("profile", OWN, "other", b"3".to_vec()),
                    ("profile", OWN, "refused", b"4".to_vec()),
                ],
                &[("profile", OWN, "lab")],
            )
            .is_err()
    );
    assert_eq!(all_names(&store), before);
    assert!(
        store
            .replace("", &["profile"], &[("profile", "refused", b"5".to_vec())])
            .is_err()
    );
    assert_eq!(all_names(&store), before);
}

fn sealed_values_round_trip(place: &Place) {
    let keys = keys();
    let store = place.open().with_keys(Some(copy(&keys)), None);
    assert!(store.encrypted());
    store.write("secret", OWN, "token", b"hunter2").unwrap();
    store.write("secret", OWN, "empty", b"").unwrap();
    let row = stored_row(&store, "secret", OWN, "token");
    assert!(row.sealed);
    assert_eq!(seal::key_id(&row.value), Some(keys.current().id()));
    assert!(!row.value.windows(7).any(|part| part == b"hunter2"));
    assert_eq!(
        store.read("secret", OWN, "token").unwrap().unwrap(),
        b"hunter2"
    );
    assert_eq!(store.read("secret", OWN, "empty").unwrap().unwrap(), b"");
    assert_eq!(
        store.entries("secret", OWN).unwrap(),
        [
            ("empty".to_owned(), vec![]),
            ("token".to_owned(), b"hunter2".to_vec())
        ]
    );
    // Names are readable without unsealing.
    let bare = place.open();
    assert_eq!(bare.names("secret", OWN).unwrap(), ["empty", "token"]);

    // A sealed value is bound to its place: moved to another name, it does not open.
    let mut moved = row.clone();
    moved.name = "other".into();
    store.backend().put_state(&moved).unwrap();
    assert!(matches!(
        store.read("secret", OWN, "other"),
        Err(Error::Sealing(_))
    ));

    let report = store.seal_report().unwrap();
    assert_eq!((report.sealed, report.plain), (3, 0));
    assert_eq!(report.key_ids, [seal::hex(&keys.current().id())]);
}

fn a_sealed_store_without_its_key_says_so(place: &Place) {
    let store = place.open().with_keys(Some(keys()), None);
    store.write("secret", OWN, "token", b"hunter2").unwrap();
    let file = PathBuf::from("/made/up/store.key");
    for store in [
        place.open().with_keys(None, Some(file.clone())),
        place.open().with_keys(Some(keys()), Some(file.clone())),
    ] {
        let error = store.read("secret", OWN, "token").unwrap_err();
        assert!(matches!(error, Error::Encrypted { .. }), "{error}");
        let text = error.to_string();
        assert!(
            text.contains("encrypted") && text.contains("/made/up/store.key"),
            "{text}"
        );
        assert!(store.entries("secret", OWN).is_err());
        assert!(store.reseal(None).is_err());
    }
}

fn plain_rows_are_read_with_a_key_loaded(place: &Place) {
    place
        .open()
        .write("profile", OWN, "studio", b"plain")
        .unwrap();
    let store = place.open().with_keys(Some(keys()), None);
    store.write("profile", OWN, "lab", b"sealed").unwrap();
    assert_eq!(
        store.entries("profile", OWN).unwrap(),
        [
            ("lab".to_owned(), b"sealed".to_vec()),
            ("studio".to_owned(), b"plain".to_vec())
        ]
    );
    let report = store.seal_report().unwrap();
    assert_eq!((report.sealed, report.plain), (1, 1));
}

fn resealing_keeps_the_times(place: &Place) {
    let plain = place.open();
    plain.write("secret", OWN, "a", b"1").unwrap();
    plain.write("profile", "mirror", "b", b"2").unwrap();
    // Times from before, so that a rewrite stamping now would show.
    for (kind, scope, name) in [("secret", OWN, "a"), ("profile", "mirror", "b")] {
        let row = StateRow {
            updated: "2020-01-01T00:00:00Z".into(),
            ..stored_row(&plain, kind, scope, name)
        };
        plain.backend().put_state(&row).unwrap();
    }
    let times = |store: &Store| -> Vec<String> {
        let rows = store.backend().all_states().unwrap();
        rows.into_iter().map(|row| row.updated).collect()
    };
    let before = times(&plain);

    let first = keys();
    assert_eq!(plain.reseal(Some(&first)).unwrap(), 2);
    let sealed = place.open().with_keys(Some(copy(&first)), None);
    let report = sealed.seal_report().unwrap();
    assert_eq!((report.sealed, report.plain), (2, 0));
    assert_eq!(report.key_ids, [seal::hex(&first.current().id())]);
    assert_eq!(sealed.read("secret", OWN, "a").unwrap().unwrap(), b"1");
    assert_eq!(times(&sealed), before);

    let second = keys();
    assert_eq!(sealed.reseal(Some(&second)).unwrap(), 2);
    let report = sealed.seal_report().unwrap();
    assert_eq!(report.key_ids, [seal::hex(&second.current().id())]);
    // The store still has only the first key, which no longer opens anything.
    assert!(matches!(
        sealed.read("secret", OWN, "a"),
        Err(Error::Encrypted { .. })
    ));
    let resealed = place.open().with_keys(Some(copy(&second)), None);
    assert_eq!(
        resealed.read("profile", "mirror", "b").unwrap().unwrap(),
        b"2"
    );
    assert_eq!(times(&resealed), before);

    assert_eq!(resealed.reseal(None).unwrap(), 2);
    let report = plain.seal_report().unwrap();
    assert_eq!(
        (report.sealed, report.plain, report.key_ids.len()),
        (0, 2, 0)
    );
    assert_eq!(plain.read("secret", OWN, "a").unwrap().unwrap(), b"1");
    assert_eq!(times(&plain), before);
    // An empty table is rewritten too.
    let empty = Place::sqlite();
    assert_eq!(empty.open().reseal(Some(&keys())).unwrap(), 0);
}

fn resealing_refuses_a_table_that_changed(place: &Place) {
    let store = place.open();
    store.write("secret", OWN, "a", b"1").unwrap();
    let rows = store.backend().all_states().unwrap();
    let sealed: Vec<_> = rows
        .iter()
        .map(|row| {
            let new = StateRow {
                value: b"changed".to_vec(),
                sealed: true,
                ..row.clone()
            };
            (row.clone(), new)
        })
        .collect();
    // Someone wrote in between: a value changed, or a row came.
    store.write("secret", OWN, "a", b"2").unwrap();
    assert!(!store.backend().swap_states(&sealed).unwrap());
    store.write("secret", OWN, "a", b"1").unwrap();
    store.write("secret", OWN, "b", b"3").unwrap();
    assert!(!store.backend().swap_states(&sealed).unwrap());
    assert_eq!(store.seal_report().unwrap().sealed, 0);
    store.forget("secret", OWN, "b").unwrap();
    assert!(store.backend().swap_states(&sealed).unwrap());
    assert_eq!(store.seal_report().unwrap().sealed, 1);
}

// ── Each backend on its own ─────────────────────────────────────────────────

#[test]
fn a_setting_names_the_store() {
    let default = Path::new("/data/berth.db");
    for setting in [None, Some(""), Some("  ")] {
        assert_eq!(
            Target::parse(setting, default),
            Target::Sqlite(default.into())
        );
    }
    assert_eq!(
        Target::parse(Some("/srv/berth/state.db"), default),
        Target::Sqlite("/srv/berth/state.db".into())
    );
    for url in [
        "postgres://berth@192.0.2.10/berth",
        "postgresql://berth@192.0.2.10/berth",
    ] {
        assert_eq!(
            Target::parse(Some(url), default),
            Target::Postgres(url.into())
        );
    }
    // The URL may hold a password.
    let target = Target::parse(Some("postgres://berth:made-up@192.0.2.10/berth"), default);
    assert!(!format!("{target:?}").contains("made-up"));
}

#[test]
fn the_sqlite_file_is_private() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("berth.db");
    let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    let store = Store::open(&path).unwrap();
    store.put("ui", "k", &json!(1), When::Always).unwrap();
    assert_eq!(mode(&path), 0o600);
    let wal = dir.path().join("berth.db-wal");
    if wal.exists() {
        assert_eq!(mode(&wal), 0o600);
    }
    drop(store);

    // A file from before is made private when it is opened.
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    Store::open(&path).unwrap();
    assert_eq!(mode(&path), 0o600);
}

#[test]
fn an_older_sqlite_file_is_migrated_and_a_newer_one_refused() {
    use rusqlite::Connection;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("berth.db");
    // A file as the first schema version left it, with data in it.
    let db = Connection::open(&path).unwrap();
    db.execute_batch(sqlite::MIGRATIONS[0]).unwrap();
    db.pragma_update(None, "user_version", 1).unwrap();
    db.execute(
        "INSERT INTO kv(namespace, key, value, revision, updated) VALUES ('ui', 'k', '7', 4, 'then')",
        [],
    )
    .unwrap();
    drop(db);
    let store = Store::open(&path).unwrap();
    assert_eq!(
        store.schema_version().unwrap(),
        sqlite::MIGRATIONS.len() as i64
    );
    assert_eq!(store.get("ui", "k").unwrap().unwrap().revision, 4);
    assert!(store.jobs(10).unwrap().is_empty());
    assert!(store.backend().all_states().unwrap().is_empty());
    store
        .take_in("server", &message(1, "a", Keep::Log))
        .unwrap();
    drop(store);
    // Opening again changes nothing.
    let store = Store::open(&path).unwrap();
    assert_eq!(all(&store).len(), 1);
    drop(store);

    let db = Connection::open(&path).unwrap();
    db.pragma_update(None, "user_version", 99).unwrap();
    drop(db);
    assert!(Store::open(&path).is_err());
}

#[test]
fn an_older_postgres_schema_is_migrated_and_a_newer_one_refused() {
    let Some(place) = Place::postgres() else {
        return;
    };
    let Place::Postgres { url, .. } = &place else {
        unreachable!()
    };
    // Tables as the first schema version left them, with data in them.
    postgres::execute(
        url,
        &format!(
            "{}
             CREATE TABLE schema_version(version INTEGER NOT NULL);
             INSERT INTO schema_version(version) VALUES (1);
             INSERT INTO kv(namespace, key, value, revision, updated) VALUES ('ui', 'k', '7', 4, 'then');",
            postgres::MIGRATIONS[0]
        ),
    )
    .unwrap();
    let store = place.open();
    assert_eq!(
        store.schema_version().unwrap(),
        postgres::MIGRATIONS.len() as i64
    );
    assert_eq!(store.get("ui", "k").unwrap().unwrap().revision, 4);
    assert!(store.jobs(10).unwrap().is_empty());
    assert!(store.backend().all_states().unwrap().is_empty());
    store
        .take_in("server", &message(1, "a", Keep::Log))
        .unwrap();
    drop(store);
    assert_eq!(all(&place.open()).len(), 1);

    postgres::execute(url, "UPDATE schema_version SET version = 99").unwrap();
    let Err(error) = Store::connect(&place.target()) else {
        panic!("a schema from a newer berth was opened");
    };
    assert!(error.to_string().contains("newer berth"), "{error}");
}

#[test]
fn two_berths_starting_at_once_migrate_once() {
    let Some(place) = Place::postgres() else {
        return;
    };
    let target = place.target();
    std::thread::scope(|scope| {
        let starting: Vec<_> = (0..4)
            .map(|_| scope.spawn(|| Store::connect(&target).map(|store| store.schema_version())))
            .collect();
        for start in starting {
            let version = start.join().unwrap().unwrap().unwrap();
            assert_eq!(version, postgres::MIGRATIONS.len() as i64);
        }
    });
}

#[test]
fn a_closed_postgres_connection_is_made_again() {
    let Some(place) = Place::postgres() else {
        return;
    };
    let Place::Postgres { url, schema } = &place else {
        unreachable!()
    };
    let store = place.open();
    store.put("ui", "k", &json!(1), When::Always).unwrap();
    // The server ends the store's connection, as on a restart.
    postgres::execute(
        url,
        &format!(
            "SELECT pg_terminate_backend(pid) FROM pg_stat_activity
             WHERE application_name = '{schema}' AND pid <> pg_backend_pid()"
        ),
    )
    .unwrap();
    // It is gone before the store is called again.
    postgres::execute(
        url,
        &format!(
            "DO $$ BEGIN
                 WHILE EXISTS (SELECT FROM pg_stat_activity
                               WHERE application_name = '{schema}' AND pid <> pg_backend_pid()) LOOP
                     PERFORM pg_sleep(0.01);
                 END LOOP;
             END $$"
        ),
    )
    .unwrap();
    assert_eq!(store.get("ui", "k").unwrap().unwrap().value, json!(1));
    assert_eq!(store.put("ui", "k", &json!(2), When::At(1)).unwrap(), 2);
}

#[test]
fn postgres_errors_do_not_show_the_password() {
    // Nothing listens on port 1.
    let url = "postgres://berth:made-up-password@127.0.0.1:1/berth";
    let Err(error) = Store::connect(&Target::Postgres(url.into())) else {
        panic!("connected to nothing");
    };
    let text = error.to_string();
    assert!(
        !text.contains("made-up-password") && !text.contains(url),
        "{text}"
    );

    // A wrong password for a server that is there.
    let Ok(base) = std::env::var("BERTH_TEST_POSTGRES") else {
        return;
    };
    let config: ::postgres::Config = base.parse().unwrap();
    let Some(::postgres::config::Host::Tcp(host)) = config.get_hosts().first() else {
        return;
    };
    let wrong = format!(
        "postgres://{}:made-up-wrong-password@{host}:{}/{}",
        config.get_user().unwrap_or("postgres"),
        config.get_ports().first().copied().unwrap_or(5432),
        config.get_dbname().unwrap_or("postgres"),
    );
    let Err(error) = Store::connect(&Target::Postgres(wrong)) else {
        panic!("a wrong password let us in");
    };
    let text = error.to_string();
    assert!(!text.contains("made-up-wrong-password"), "{text}");
}

#[test]
fn sslmode_require_does_not_fall_back_to_a_plain_connection() {
    // The test server speaks no TLS.
    let Some(place) = Place::postgres() else {
        return;
    };
    let Place::Postgres { url, .. } = &place else {
        unreachable!()
    };
    let Err(error) = Store::connect(&Target::Postgres(format!("{url}&sslmode=require"))) else {
        return;
    };
    assert!(error.to_string().contains("TLS"), "{error}");
}

/// `berth serve` calls the store from its handlers, inside a runtime.
#[tokio::test]
async fn the_store_can_be_called_from_async_code() {
    let mut places = vec![Place::sqlite()];
    places.extend(Place::postgres());
    for place in &places {
        let store = place.open();
        store.put("ui", "k", &json!("async"), When::Always).unwrap();
        tokio::task::yield_now().await;
        assert_eq!(store.get("ui", "k").unwrap().unwrap().value, json!("async"));
        store
            .take_in("server", &message(1, "a", Keep::Log))
            .unwrap();
        assert_eq!(all(&store).len(), 1);
    }
}

/// An installation pointed at a new database takes in the files it has.
fn the_old_files_go_into_an_empty_store(place: &Place) {
    let dir = tempfile::tempdir().unwrap();
    let paths = crate::paths::Paths {
        config_dir: dir.path().join("config"),
        data_dir: dir.path().join("data"),
    };
    std::fs::create_dir_all(&paths.config_dir).unwrap();
    std::fs::write(
        paths.settings_file(),
        r#"{"default_host": "studio", "registries": {"hub": {"type": "docker", "url": "ghcr.io"}}}"#,
    )
    .unwrap();
    std::fs::write(
        paths.config_dir.join("secrets.json"),
        r#"{"github": "ghp-made-up"}"#,
    )
    .unwrap();
    let installation = Installation {
        target: place.target(),
        key_file: paths.config_dir.join("store.key"),
        ca: None,
    };
    let store = place.open();
    crate::import::run(&paths, &installation, &store).unwrap();
    assert_eq!(
        store.read(kind::SECRET, OWN, "github").unwrap().unwrap(),
        b"ghp-made-up"
    );
    assert_eq!(store.names(kind::REGISTRY, OWN).unwrap(), ["hub"]);
    assert_eq!(
        store
            .read(kind::SETTING, OWN, "default_host")
            .unwrap()
            .unwrap(),
        b"studio"
    );
    assert!(paths.config_dir.join("migrated/secrets.json").exists());
    assert_eq!(
        std::fs::read_to_string(paths.settings_file()).unwrap(),
        "{}\n"
    );
}

/// A store with something in every table: messages, a cursor, values at a
/// later revision, jobs, sealed and plain state, and a berth server's scope.
fn filled(store: &Store) {
    store
        .take_in("studio", &message(1, "a", Keep::Log))
        .unwrap();
    store
        .take_in("studio", &message(2, "b", Keep::Last))
        .unwrap();
    store.take_in("attic", &message(7, "a", Keep::Log)).unwrap();
    store.set_done("studio", 1, true).unwrap();
    store
        .put("ui", "k", &json!({"b": 1, "a": [2]}), When::Always)
        .unwrap();
    store
        .put("ui", "k", &json!({"b": 2, "a": [2]}), When::Always)
        .unwrap();
    store
        .put("other", "x", &json!("text"), When::Always)
        .unwrap();
    store
        .add_job(&Job {
            id: "j1".into(),
            host: "studio".into(),
            what: "apply".into(),
            started: "2026-01-02T03:04:05Z".into(),
            ended: Some("2026-01-02T03:05:00Z".into()),
            exit_code: Some(2),
            output_path: "/srv/berth/jobs/j1.log".into(),
        })
        .unwrap();
    store
        .add_job(&Job {
            id: "j2".into(),
            host: "attic".into(),
            what: "rollout".into(),
            started: "2026-01-03T00:00:00Z".into(),
            ended: None,
            exit_code: None,
            output_path: "/srv/berth/jobs/j2.log".into(),
        })
        .unwrap();
    store.write(kind::SETTING, OWN, "plain", b"1").unwrap();
    // A sealed row is copied as it is stored, so any key makes one.
    let other = Place::sqlite();
    let sealed = other.open().with_keys(Some(keys()), None);
    sealed.write(kind::SECRET, OWN, "api", b"made-up").unwrap();
    let row = stored_row(&sealed, kind::SECRET, OWN, "api");
    store.backend().put_state(&row).unwrap();
    store
        .write(kind::PROFILE, "served:http://192.0.2.7:7777", "yard", b"{}")
        .unwrap();
}

/// What every table of a store holds, as it is stored.
fn tables(store: &Store) -> Vec<Rows> {
    Table::ALL
        .iter()
        .map(|table| store.export(*table).unwrap())
        .collect()
}

/// The rows, without those of berth servers' scopes.
fn without_served(mut rows: Vec<Rows>) -> Vec<Rows> {
    if let Rows::State(state) = &mut rows[0] {
        state.retain(|row| !served_scope(&row.scope));
    }
    rows
}

fn a_copy_takes_every_table_as_it_is_and_back(place: &Place) {
    let source = Place::sqlite();
    let from = source.open();
    filled(&from);
    let to = place.open();
    let report = super::copy(&from, &to, false).unwrap();
    assert_eq!(report.left_out, 1);
    let counts: Vec<_> = report
        .tables
        .iter()
        .map(|copied| (copied.table.name(), copied.found, copied.added))
        .collect();
    assert_eq!(
        counts,
        [
            ("state", 2, 2),
            ("messages", 3, 3),
            ("cursors", 2, 2),
            ("kv", 2, 2),
            ("jobs", 2, 2)
        ]
    );
    let expected = without_served(tables(&from));
    assert_eq!(tables(&to), expected);
    // What was copied works as it did: the next message, the next revision.
    let (messages, _) = to
        .inbox(&Filter {
            limit: 10,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(messages[0].seq, 3);
    assert!(messages.iter().any(|stored| stored.done));
    assert_eq!(to.get("ui", "k").unwrap().unwrap().revision, 2);
    assert_eq!(to.put("ui", "k", &json!(3), When::At(2)).unwrap(), 3);
    let taken = to.take_in("studio", &message(3, "a", Keep::Log)).unwrap();
    assert_eq!(taken.unwrap().seq, 4);
    assert_eq!(to.cursor("attic").unwrap(), 7);

    // And back into a new file, as it is now.
    let back = Place::sqlite();
    let again = back.open();
    super::copy(&to, &again, false).unwrap();
    assert_eq!(tables(&again), tables(&to));
}

fn a_copy_into_a_store_that_has_rows_keeps_them(place: &Place) {
    let source = Place::sqlite();
    let from = source.open();
    filled(&from);
    let to = place.open();
    to.write(kind::SETTING, OWN, "plain", b"the target's")
        .unwrap();
    let refused = super::copy(&from, &to, false).unwrap_err().to_string();
    assert!(refused.contains("--into-existing"), "{refused}");
    assert_eq!(to.count(Table::Messages).unwrap(), 0);

    let report = super::copy(&from, &to, true).unwrap();
    assert_eq!((report.tables[0].found, report.tables[0].added), (2, 1));
    assert_eq!(
        to.read(kind::SETTING, OWN, "plain").unwrap().unwrap(),
        b"the target's"
    );
    assert_eq!(to.count(Table::Jobs).unwrap(), 2);
    // Again, nothing is added.
    let report = super::copy(&from, &to, true).unwrap();
    assert!(report.tables.iter().all(|copied| copied.added == 0));
}
