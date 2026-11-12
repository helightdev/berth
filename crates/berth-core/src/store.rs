//! What berth keeps on the workstation: messages taken in from the hosts'
//! channels, a small key-value store, and the jobs `berth serve` ran. One
//! SQLite file; `berth serve` writes it and the commands read it beside it.

use berth_api::{From, Keep, Level, Message, rfc3339, unix_now};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::path::Path;

/// A value's size limit.
pub const VALUE_LIMIT: usize = 64 * 1024;
/// How long messages and jobs are kept unless the settings say otherwise.
pub const RETENTION_DAYS: u64 = 90;

/// Each step takes the schema one version further; `user_version` counts them.
const MIGRATIONS: &[&str] = &[
    "CREATE TABLE messages(
         seq INTEGER NOT NULL UNIQUE, host TEXT NOT NULL, id INTEGER NOT NULL,
         topic TEXT NOT NULL, body TEXT NOT NULL, keep TEXT NOT NULL,
         from_level TEXT NOT NULL, from_uid INTEGER NOT NULL, session TEXT,
         time TEXT NOT NULL, done INTEGER NOT NULL DEFAULT 0,
         PRIMARY KEY (host, id));
     CREATE TABLE cursors(host TEXT PRIMARY KEY, last_id INTEGER NOT NULL);
     CREATE TABLE kv(
         namespace TEXT NOT NULL, key TEXT NOT NULL, value TEXT NOT NULL,
         revision INTEGER NOT NULL, updated TEXT NOT NULL,
         PRIMARY KEY (namespace, key));",
    "CREATE INDEX messages_topic ON messages(topic, time);
     CREATE TABLE jobs(
         id TEXT PRIMARY KEY, host TEXT NOT NULL, what TEXT NOT NULL,
         started TEXT NOT NULL, ended TEXT, exit_code INTEGER, output_path TEXT NOT NULL);",
];

#[derive(Debug)]
pub enum Error {
    /// The condition of a conditional write did not hold. Carries the
    /// revision that is there, if the key exists.
    Conflict(Option<i64>),
    TooLarge,
    Database(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Conflict(Some(revision)) => write!(f, "the key is at revision {revision}"),
            Error::Conflict(None) => f.write_str("the key does not exist"),
            Error::TooLarge => write!(f, "a value is at most {VALUE_LIMIT} bytes"),
            Error::Database(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for Error {}

impl std::convert::From<rusqlite::Error> for Error {
    fn from(error: rusqlite::Error) -> Self {
        Error::Database(error.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// When a write to the key-value store may happen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum When {
    Always,
    /// Only if the key is at this revision.
    At(i64),
    /// Only if the key does not exist.
    New,
}

#[derive(Debug, Clone)]
pub struct Stored {
    /// The order it was taken in; what `before` and `next` count in.
    pub seq: i64,
    pub host: String,
    pub message: Message,
    pub done: bool,
}

impl Stored {
    pub fn json(&self) -> Value {
        let mut value = serde_json::to_value(&self.message).unwrap_or_default();
        value["host"] = json!(self.host);
        value["done"] = json!(self.done);
        value["seq"] = json!(self.seq);
        value
    }
}

#[derive(Debug, Clone, Default)]
pub struct Filter<'a> {
    pub host: Option<&'a str>,
    /// Topics starting with this.
    pub topic: Option<&'a str>,
    /// Only messages taken in before this `seq`.
    pub before: Option<i64>,
    pub done: Option<bool>,
    pub limit: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub key: String,
    pub value: Value,
    pub revision: i64,
    pub updated: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Job {
    pub id: String,
    pub host: String,
    pub what: String,
    pub started: String,
    pub ended: Option<String>,
    pub exit_code: Option<i32>,
    pub output_path: String,
}

impl Job {
    pub fn json(&self) -> Value {
        json!({
            "id": self.id, "host": self.host, "what": self.what, "started": self.started,
            "ended": self.ended, "exit_code": self.exit_code,
        })
    }
}

pub struct Store {
    db: Connection,
}

fn keep_name(keep: Keep) -> &'static str {
    match keep {
        Keep::None => "none",
        Keep::Last => "last",
        Keep::Log => "log",
    }
}

fn stored(row: &rusqlite::Row<'_>) -> rusqlite::Result<Stored> {
    let body: String = row.get(4)?;
    let keep: String = row.get(5)?;
    let level: String = row.get(6)?;
    Ok(Stored {
        seq: row.get(0)?,
        host: row.get(1)?,
        message: Message {
            id: row.get::<_, i64>(2)? as u64,
            topic: row.get(3)?,
            body: serde_json::from_str(&body).unwrap_or(Value::Null),
            keep: if keep == "last" {
                Keep::Last
            } else {
                Keep::Log
            },
            from: From {
                level: if level == "agent" {
                    Level::Agent
                } else {
                    Level::Host
                },
                uid: row.get(7)?,
                session: row.get(8)?,
            },
            time: row.get(9)?,
        },
        done: row.get(10)?,
    })
}

const MESSAGE_COLUMNS: &str =
    "seq, host, id, topic, body, keep, from_level, from_uid, session, time, done";

fn job(row: &rusqlite::Row<'_>) -> rusqlite::Result<Job> {
    Ok(Job {
        id: row.get(0)?,
        host: row.get(1)?,
        what: row.get(2)?,
        started: row.get(3)?,
        ended: row.get(4)?,
        exit_code: row.get(5)?,
        output_path: row.get(6)?,
    })
}

impl Store {
    /// Open the file, creating it and bringing its schema up to date.
    pub fn open(path: &Path) -> Result<Store> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| Error::Database(error.to_string()))?;
        }
        let mut db = Connection::open(path)?;
        // Another process may be writing: wait for it rather than fail.
        db.busy_timeout(std::time::Duration::from_secs(5))?;
        db.pragma_update(None, "journal_mode", "WAL")?;
        let version: i64 = db.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version as usize > MIGRATIONS.len() {
            return Err(Error::Database(format!(
                "{} was written by a newer berth (schema {version})",
                path.display()
            )));
        }
        for (index, step) in MIGRATIONS.iter().enumerate().skip(version as usize) {
            let transaction = db.transaction()?;
            transaction.execute_batch(step)?;
            transaction.pragma_update(None, "user_version", index as i64 + 1)?;
            transaction.commit()?;
        }
        Ok(Store { db })
    }

    pub fn schema_version(&self) -> Result<i64> {
        Ok(self
            .db
            .query_row("PRAGMA user_version", [], |row| row.get(0))?)
    }

    // ── Messages ────────────────────────────────────────────────────────────

    /// The last message id taken in from a host; 0 before the first.
    pub fn cursor(&self, host: &str) -> Result<u64> {
        let id: Option<i64> = self
            .db
            .query_row(
                "SELECT last_id FROM cursors WHERE host = ?1",
                [host],
                |row| row.get(0),
            )
            .optional()?;
        Ok(id.unwrap_or(0) as u64)
    }

    /// Take in a message from a host's channel and move the host's cursor, in
    /// one transaction. A `none` message only moves the cursor. Returns what
    /// was stored, or nothing when it was already there or is not kept.
    pub fn take_in(&mut self, host: &str, message: &Message) -> Result<Option<Stored>> {
        let transaction = self.db.transaction()?;
        let mut taken = None;
        if message.keep != Keep::None {
            if message.keep == Keep::Last {
                transaction.execute(
                    "DELETE FROM messages WHERE host = ?1 AND topic = ?2 AND keep = 'last' AND id < ?3",
                    params![host, message.topic, message.id as i64],
                )?;
            }
            let seq: i64 = transaction.query_row(
                "SELECT COALESCE(MAX(seq), 0) + 1 FROM messages",
                [],
                |row| row.get(0),
            )?;
            let level = match message.from.level {
                Level::Host => "host",
                Level::Agent => "agent",
            };
            let added = transaction.execute(
                "INSERT INTO messages(seq, host, id, topic, body, keep, from_level, from_uid, session, time, done)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 0)
                 ON CONFLICT (host, id) DO NOTHING",
                params![
                    seq,
                    host,
                    message.id as i64,
                    message.topic,
                    message.body.to_string(),
                    keep_name(message.keep),
                    level,
                    message.from.uid,
                    message.from.session,
                    message.time,
                ],
            )?;
            if added == 1 {
                taken = Some(Stored {
                    seq,
                    host: host.to_owned(),
                    message: message.clone(),
                    done: false,
                });
            }
        }
        transaction.execute(
            "INSERT INTO cursors(host, last_id) VALUES (?1, ?2)
             ON CONFLICT (host) DO UPDATE SET last_id = MAX(last_id, excluded.last_id)",
            params![host, message.id as i64],
        )?;
        transaction.commit()?;
        Ok(taken)
    }

    /// Forget where a host's channel was read up to: its counter started again.
    pub fn reset_cursor(&mut self, host: &str) -> Result<()> {
        self.db
            .execute("DELETE FROM cursors WHERE host = ?1", [host])?;
        Ok(())
    }

    /// Messages, newest first, and the `before` for the next page when there is one.
    pub fn inbox(&self, filter: &Filter<'_>) -> Result<(Vec<Stored>, Option<i64>)> {
        let limit = filter.limit.clamp(1, 1000);
        let topic = filter.topic.unwrap_or("");
        let mut statement = self.db.prepare(&format!(
            "SELECT {MESSAGE_COLUMNS} FROM messages
             WHERE (?1 IS NULL OR host = ?1)
               AND substr(topic, 1, length(?2)) = ?2
               AND (?3 IS NULL OR seq < ?3)
               AND (?4 IS NULL OR done = ?4)
             ORDER BY seq DESC LIMIT ?5"
        ))?;
        let rows = statement.query_map(
            params![
                filter.host,
                topic,
                filter.before,
                filter.done,
                limit as i64 + 1
            ],
            stored,
        )?;
        let mut messages = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        let next = if messages.len() > limit {
            messages.truncate(limit);
            messages.last().map(|message| message.seq)
        } else {
            None
        };
        Ok((messages, next))
    }

    /// Mark a message done or not. False when there is no such message.
    pub fn set_done(&mut self, host: &str, id: u64, done: bool) -> Result<bool> {
        let changed = self.db.execute(
            "UPDATE messages SET done = ?3 WHERE host = ?1 AND id = ?2",
            params![host, id as i64, done],
        )?;
        Ok(changed == 1)
    }

    // ── Key-value ───────────────────────────────────────────────────────────

    pub fn get(&self, namespace: &str, key: &str) -> Result<Option<Entry>> {
        let entry = self
            .db
            .query_row(
                "SELECT key, value, revision, updated FROM kv WHERE namespace = ?1 AND key = ?2",
                [namespace, key],
                entry,
            )
            .optional()?;
        Ok(entry)
    }

    /// Set a value. Returns its new revision.
    pub fn put(&mut self, namespace: &str, key: &str, value: &Value, when: When) -> Result<i64> {
        let text = value.to_string();
        if text.len() > VALUE_LIMIT {
            return Err(Error::TooLarge);
        }
        let transaction = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let current: Option<i64> = transaction
            .query_row(
                "SELECT revision FROM kv WHERE namespace = ?1 AND key = ?2",
                [namespace, key],
                |row| row.get(0),
            )
            .optional()?;
        let allowed = match when {
            When::Always => true,
            When::At(revision) => current == Some(revision),
            When::New => current.is_none(),
        };
        if !allowed {
            return Err(Error::Conflict(current));
        }
        let revision = current.unwrap_or(0) + 1;
        transaction.execute(
            "INSERT INTO kv(namespace, key, value, revision, updated) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (namespace, key) DO UPDATE
             SET value = excluded.value, revision = excluded.revision, updated = excluded.updated",
            params![namespace, key, text, revision, rfc3339(unix_now())],
        )?;
        transaction.commit()?;
        Ok(revision)
    }

    /// Remove a key. False when it was not there.
    pub fn remove(&mut self, namespace: &str, key: &str, when: When) -> Result<bool> {
        let transaction = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let current: Option<i64> = transaction
            .query_row(
                "SELECT revision FROM kv WHERE namespace = ?1 AND key = ?2",
                [namespace, key],
                |row| row.get(0),
            )
            .optional()?;
        if let When::At(revision) = when
            && current != Some(revision)
        {
            return Err(Error::Conflict(current));
        }
        let removed = transaction.execute(
            "DELETE FROM kv WHERE namespace = ?1 AND key = ?2",
            [namespace, key],
        )?;
        transaction.commit()?;
        Ok(removed == 1)
    }

    /// A namespace's entries, by key.
    pub fn list(&self, namespace: &str) -> Result<Vec<Entry>> {
        let mut statement = self.db.prepare(
            "SELECT key, value, revision, updated FROM kv WHERE namespace = ?1 ORDER BY key",
        )?;
        let rows = statement.query_map([namespace], entry)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    // ── Jobs ────────────────────────────────────────────────────────────────

    pub fn add_job(&mut self, job: &Job) -> Result<()> {
        self.db.execute(
            "INSERT INTO jobs(id, host, what, started, ended, exit_code, output_path)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                job.id,
                job.host,
                job.what,
                job.started,
                job.ended,
                job.exit_code,
                job.output_path
            ],
        )?;
        Ok(())
    }

    pub fn end_job(&mut self, id: &str, exit_code: Option<i32>) -> Result<()> {
        self.db.execute(
            "UPDATE jobs SET ended = ?2, exit_code = ?3 WHERE id = ?1 AND ended IS NULL",
            params![id, rfc3339(unix_now()), exit_code],
        )?;
        Ok(())
    }

    /// Jobs that were running when the program that ran them went away.
    pub fn end_lost_jobs(&mut self) -> Result<usize> {
        Ok(self.db.execute(
            "UPDATE jobs SET ended = ?1 WHERE ended IS NULL",
            [rfc3339(unix_now())],
        )?)
    }

    pub fn job(&self, id: &str) -> Result<Option<Job>> {
        let found = self
            .db
            .query_row(
                "SELECT id, host, what, started, ended, exit_code, output_path FROM jobs WHERE id = ?1",
                [id],
                job,
            )
            .optional()?;
        Ok(found)
    }

    /// Jobs, newest first.
    pub fn jobs(&self, limit: usize) -> Result<Vec<Job>> {
        let mut statement = self.db.prepare(
            "SELECT id, host, what, started, ended, exit_code, output_path FROM jobs
             ORDER BY started DESC, id DESC LIMIT ?1",
        )?;
        let rows = statement.query_map([limit.clamp(1, 1000) as i64], job)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    // ── Pruning ─────────────────────────────────────────────────────────────

    /// Drop messages and ended jobs older than `days`. Key-value is never
    /// pruned. Returns the output files of the jobs dropped, for removing.
    pub fn prune(&mut self, days: u64) -> Result<Vec<String>> {
        let cutoff = rfc3339(unix_now().saturating_sub(days * 86400));
        let transaction = self.db.transaction()?;
        transaction.execute("DELETE FROM messages WHERE time < ?1", [&cutoff])?;
        let files = {
            let mut statement = transaction
                .prepare("SELECT output_path FROM jobs WHERE ended IS NOT NULL AND started < ?1")?;
            let rows = statement.query_map([&cutoff], |row| row.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        transaction.execute(
            "DELETE FROM jobs WHERE ended IS NOT NULL AND started < ?1",
            [&cutoff],
        )?;
        transaction.commit()?;
        Ok(files)
    }
}

fn entry(row: &rusqlite::Row<'_>) -> rusqlite::Result<Entry> {
    let value: String = row.get(1)?;
    Ok(Entry {
        key: row.get(0)?,
        value: serde_json::from_str(&value).unwrap_or(Value::Null),
        revision: row.get(2)?,
        updated: row.get(3)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn messages_are_taken_in_once_and_the_cursor_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("berth.db");
        let mut store = Store::open(&path).unwrap();
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

        let mut store = Store::open(&path).unwrap();
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
        assert_eq!(found[1].message.body, json!({"n": 2}));
        assert_eq!(found[1].message.from.session.as_deref(), Some("sabc"));
        assert_eq!(found[1].message.from.level, Level::Agent);

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
    }

    #[test]
    fn the_inbox_filters_pages_and_marks() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("berth.db")).unwrap();
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
        let pattern = Filter {
            topic: Some("%"),
            limit: 10,
            ..Default::default()
        };
        assert!(store.inbox(&pattern).unwrap().0.is_empty());
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

    #[test]
    fn two_writers_on_one_key_and_one_loses() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("berth.db");
        let mut first = Store::open(&path).unwrap();
        let mut second = Store::open(&path).unwrap();
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
        assert!(first.remove("ui", "layout", When::Always).unwrap());
        assert!(!first.remove("ui", "layout", When::Always).unwrap());
        assert!(first.get("ui", "layout").unwrap().is_none());
        let large = json!("x".repeat(VALUE_LIMIT));
        assert!(matches!(
            first.put("ui", "big", &large, When::Always),
            Err(Error::TooLarge)
        ));
    }

    #[test]
    fn old_messages_and_jobs_are_pruned_and_values_stay() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("berth.db")).unwrap();
        let mut old = message(1, "a", Keep::Log);
        old.time = rfc3339(unix_now() - 100 * 86400);
        store.take_in("server", &old).unwrap();
        store
            .take_in("server", &message(2, "a", Keep::Log))
            .unwrap();
        store.put("ui", "k", &json!(1), When::Always).unwrap();
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
        // The cursor is not what was pruned.
        assert_eq!(store.cursor("server").unwrap(), 2);

        store.end_job("jrunning", Some(3)).unwrap();
        let ended = store.job("jrunning").unwrap().unwrap();
        assert_eq!(ended.exit_code, Some(3));
        assert!(ended.ended.is_some());
        assert!(store.job("nope").unwrap().is_none());
    }

    #[test]
    fn an_older_file_is_migrated_and_a_newer_one_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("berth.db");
        // A file as the first schema version left it, with data in it.
        let db = Connection::open(&path).unwrap();
        db.execute_batch(MIGRATIONS[0]).unwrap();
        db.pragma_update(None, "user_version", 1).unwrap();
        db.execute(
            "INSERT INTO kv(namespace, key, value, revision, updated) VALUES ('ui', 'k', '7', 4, 'then')",
            [],
        )
        .unwrap();
        drop(db);
        let mut store = Store::open(&path).unwrap();
        assert_eq!(store.schema_version().unwrap(), MIGRATIONS.len() as i64);
        assert_eq!(store.get("ui", "k").unwrap().unwrap().revision, 4);
        assert!(store.jobs(10).unwrap().is_empty());
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
}
