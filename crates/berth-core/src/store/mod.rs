//! What berth keeps: messages taken in from the hosts' channels, a small
//! key-value store, the jobs `berth serve` ran, and state. It lives in a
//! SQLite file by default or in a PostgreSQL database; `berth serve` writes it
//! and the commands read it beside it.

mod installation;
mod postgres;
mod sqlite;
#[cfg(test)]
mod suite;

pub use installation::{Installation, default_file, hold_lock, shared, wait_lock};
pub use postgres::PostgresBackend;
pub use sqlite::SqliteBackend;

use crate::seal;
use berth_api::{Keep, Message, rfc3339, unix_now};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

/// The kinds of values in the `state` table.
pub mod kind {
    pub const PROFILE: &str = "profile";
    pub const CREDENTIALS: &str = "credentials";
    /// The record of a host's machine of berth before 0.5, which an import
    /// turns into a resource.
    pub const INSTANCE: &str = "instance";
    /// What a provisioner made for berth, by its id.
    pub const RESOURCE: &str = "resource";
    /// A resource's specification, by the resource's id.
    pub const SPEC: &str = "spec";
    /// The values of a provisioner's own arguments, by `PLUGIN/ID`.
    pub const PROVISIONER: &str = "provisioner";
    /// A provisioner berth runs under a name of its own: its type and settings.
    pub const PROVISIONER_ENTRY: &str = "provisioner-entry";
    pub const SETTING: &str = "setting";
    pub const REGISTRY: &str = "registry";
    pub const SECRET: &str = "secret";
    pub const KEY: &str = "key";
    /// What berth knows of a host's SSH: its host keys and the keys it trusts.
    pub const ACCESS: &str = "access";
    pub const TOKEN: &str = "token";
    pub const PLUGIN: &str = "plugin";
    pub const MIRROR: &str = "mirror";
    pub const META: &str = "meta";
}

/// The scope of this installation's own state; a mirror of a berth server uses another.
pub const OWN: &str = "";

/// A value's size limit.
pub const VALUE_LIMIT: usize = 64 * 1024;
/// How long messages and jobs are kept unless the settings say otherwise.
pub const RETENTION_DAYS: u64 = 90;

#[derive(Debug)]
pub enum Error {
    /// The condition of a conditional write did not hold. Carries the
    /// revision that is there, if the key exists.
    Conflict(Option<i64>),
    TooLarge,
    Database(String),
    /// A value is sealed with a key berth does not have. Names the key file
    /// it looked for and the id of the key, never key material.
    Encrypted {
        key_file: Option<PathBuf>,
        key_id: Option<String>,
    },
    /// A sealed value that does not open with its key, or a key that cannot be used.
    Sealing(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Conflict(Some(revision)) => write!(f, "the key is at revision {revision}"),
            Error::Conflict(None) => f.write_str("the key does not exist"),
            Error::TooLarge => write!(f, "a value is at most {VALUE_LIMIT} bytes"),
            Error::Database(message) => f.write_str(message),
            Error::Encrypted { key_file, key_id } => {
                f.write_str("the store is encrypted")?;
                if let Some(id) = key_id {
                    write!(f, " with key {id}")?;
                }
                match key_file {
                    Some(file) => write!(
                        f,
                        ", and berth has no key that opens it: it looked in {} and beside it",
                        file.display()
                    ),
                    None => f.write_str(", and berth was given no key that opens it"),
                }
            }
            Error::Sealing(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for Error {}

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

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Job {
    pub id: String,
    /// The hosts it is about, for a reader.
    pub host: String,
    /// The action, as rows from before `action` was kept say it.
    pub what: String,
    pub started: String,
    pub ended: Option<String>,
    pub exit_code: Option<i32>,
    pub output_path: String,
    /// `host:NAME`, `provisioner:NAME`, `resource:ID` or `installation`;
    /// empty in rows from before it was kept.
    pub subject: String,
    pub action: String,
    /// The caller's token: the same one again for the same subject and
    /// action finds this job.
    pub token: Option<String>,
}

impl Job {
    pub fn json(&self) -> Value {
        // Rows from before actions were kept say it as what they did.
        let action = match self.action.is_empty() {
            true => &self.what,
            false => &self.action,
        };
        json!({
            "id": self.id, "subject": self.subject, "action": action, "host": self.host,
            "started": self.started, "ended": self.ended, "exit_code": self.exit_code,
        })
    }
}

/// A row of the `state` table: a value of some kind, for a scope, by name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateRow {
    pub kind: String,
    pub scope: String,
    pub name: String,
    pub value: Vec<u8>,
    pub sealed: bool,
    pub updated: String,
}

/// The tables of the store, in the order a copy writes them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Table {
    State,
    Messages,
    Cursors,
    Kv,
    Jobs,
}

impl Table {
    pub const ALL: [Table; 5] = [
        Table::State,
        Table::Messages,
        Table::Cursors,
        Table::Kv,
        Table::Jobs,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Table::State => "state",
            Table::Messages => "messages",
            Table::Cursors => "cursors",
            Table::Kv => "kv",
            Table::Jobs => "jobs",
        }
    }
}

/// A row of `messages` as it is stored, for copying it unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageRow {
    pub seq: i64,
    pub host: String,
    pub id: i64,
    pub topic: String,
    pub body: String,
    pub keep: String,
    pub from_level: String,
    pub from_uid: i64,
    pub session: Option<String>,
    pub time: String,
    pub done: bool,
}

/// A row of `cursors`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorRow {
    pub host: String,
    pub last_id: i64,
}

/// A row of `kv` as it is stored, its value as the JSON text it was written as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KvRow {
    pub namespace: String,
    pub key: String,
    pub value: String,
    pub revision: i64,
    pub updated: String,
}

/// The rows of one table.
#[derive(Debug, Clone, PartialEq)]
pub enum Rows {
    State(Vec<StateRow>),
    Messages(Vec<MessageRow>),
    Cursors(Vec<CursorRow>),
    Kv(Vec<KvRow>),
    Jobs(Vec<Job>),
}

impl Rows {
    pub fn table(&self) -> Table {
        match self {
            Rows::State(_) => Table::State,
            Rows::Messages(_) => Table::Messages,
            Rows::Cursors(_) => Table::Cursors,
            Rows::Kv(_) => Table::Kv,
            Rows::Jobs(_) => Table::Jobs,
        }
    }

    pub fn len(&self) -> usize {
        match self {
            Rows::State(rows) => rows.len(),
            Rows::Messages(rows) => rows.len(),
            Rows::Cursors(rows) => rows.len(),
            Rows::Kv(rows) => rows.len(),
            Rows::Jobs(rows) => rows.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Where the store is.
#[derive(Clone, PartialEq, Eq)]
pub enum Target {
    Sqlite(PathBuf),
    /// A `postgres://` or `postgresql://` URL.
    Postgres(String),
}

impl Target {
    /// The store a setting names: nothing is the SQLite file at
    /// `default_file`, a PostgreSQL URL is that database, anything else the
    /// path of a SQLite file.
    pub fn parse(setting: Option<&str>, default_file: &Path) -> Target {
        let setting = setting.map(str::trim).unwrap_or("");
        if setting.is_empty() {
            Target::Sqlite(default_file.to_owned())
        } else if setting.starts_with("postgres://") || setting.starts_with("postgresql://") {
            Target::Postgres(setting.to_owned())
        } else {
            Target::Sqlite(PathBuf::from(setting))
        }
    }

    /// Where it is, for people: the file, or the PostgreSQL server and
    /// database without the user or the password.
    pub fn describe(&self) -> String {
        match self {
            Target::Sqlite(path) => format!("SQLite file {}", path.display()),
            Target::Postgres(url) => postgres::describe(url),
        }
    }
}

// The URL may hold a password, so it is never printed.
impl std::fmt::Debug for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Target::Sqlite(path) => f.debug_tuple("Sqlite").field(path).finish(),
            Target::Postgres(_) => f.write_str("Postgres(..)"),
        }
    }
}

/// What a database has to do for the store. Times are given by `Store`, so
/// every backend writes the same.
pub trait Backend: Send {
    fn name(&self) -> &'static str;
    fn schema_version(&mut self) -> Result<i64>;

    fn cursor(&mut self, host: &str) -> Result<u64>;
    fn take_in(&mut self, host: &str, message: &Message) -> Result<Option<Stored>>;
    fn reset_cursor(&mut self, host: &str) -> Result<()>;
    /// The messages that pass the filter, newest first, at most `limit` of
    /// them; `filter.limit` is left to `Store`.
    fn inbox(&mut self, filter: &Filter<'_>, limit: usize) -> Result<Vec<Stored>>;
    fn set_done(&mut self, host: &str, id: u64, done: bool) -> Result<bool>;

    fn get(&mut self, namespace: &str, key: &str) -> Result<Option<Entry>>;
    /// Write `text`, a JSON value, if `when` holds. Returns the new revision.
    fn put(
        &mut self,
        namespace: &str,
        key: &str,
        text: &str,
        when: When,
        updated: &str,
    ) -> Result<i64>;
    fn remove(&mut self, namespace: &str, key: &str, when: When) -> Result<bool>;
    fn list(&mut self, namespace: &str) -> Result<Vec<Entry>>;

    /// Add the job, unless one with its subject, action and token is there:
    /// then that one is returned instead, in the same statement, so two
    /// callers with one token get one job.
    fn add_job(&mut self, job: &Job) -> Result<Job>;
    fn job_by_token(&mut self, subject: &str, action: &str, token: &str) -> Result<Option<Job>>;
    fn end_job(&mut self, id: &str, exit_code: Option<i32>, ended: &str) -> Result<()>;
    fn end_lost_jobs(&mut self, ended: &str) -> Result<usize>;
    fn job(&mut self, id: &str) -> Result<Option<Job>>;
    fn jobs(&mut self, limit: usize) -> Result<Vec<Job>>;

    /// Drop messages from before `cutoff` and jobs that ended and started
    /// before it, in one transaction. Returns the dropped jobs' output files.
    fn prune(&mut self, cutoff: &str) -> Result<Vec<String>>;

    fn state(&mut self, kind: &str, scope: &str, name: &str) -> Result<Option<StateRow>>;
    /// A scope's rows of a kind, by name.
    fn states(&mut self, kind: &str, scope: &str) -> Result<Vec<StateRow>>;
    /// Insert the row or replace the one with its key.
    fn put_state(&mut self, row: &StateRow) -> Result<()>;
    fn remove_state(&mut self, kind: &str, scope: &str, name: &str) -> Result<bool>;
    /// In one transaction: delete every row of `scope` whose kind is in
    /// `kinds`, then insert `rows`.
    fn replace_states(&mut self, scope: &str, kinds: &[&str], rows: &[StateRow]) -> Result<()>;
    /// Every row of the table.
    fn all_states(&mut self) -> Result<Vec<StateRow>>;
    /// In one transaction: insert or replace each of `writes`, and delete
    /// the rows with the keys (kind, scope, name) in `removals`.
    fn change_states(
        &mut self,
        writes: &[StateRow],
        removals: &[(String, String, String)],
    ) -> Result<()>;
    /// In one transaction, while nobody else writes to the table: give each
    /// row the value and `sealed` of its new version, leaving `updated` as
    /// it is. False, with nothing changed, when the table no longer holds
    /// exactly the old rows.
    fn swap_states(&mut self, rows: &[(StateRow, StateRow)]) -> Result<bool>;

    /// Every row of a table, as it is stored.
    fn export(&mut self, table: Table) -> Result<Rows>;
    /// In one transaction: add the rows as they are, and leave a row that
    /// is there already by its key as it is. Returns how many were added.
    fn import(&mut self, rows: &Rows) -> Result<usize>;
    /// How many rows a table has.
    fn count(&mut self, table: Table) -> Result<u64>;
}

/// How much of the `state` table is sealed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SealReport {
    pub sealed: usize,
    pub plain: usize,
    /// The ids of the keys the sealed rows were made with, in hex.
    pub key_ids: Vec<String>,
}

pub struct Store {
    backend: Mutex<Box<dyn Backend>>,
    /// With keys every value written to `state` is sealed; without, plain.
    keys: Option<seal::Keys>,
    /// The key file berth looked for, to name when a value does not open.
    key_file: Option<PathBuf>,
}

/// The associated data that binds a sealed value to its place.
fn context(kind: &str, scope: &str, name: &str) -> Vec<u8> {
    [kind, scope, name].join("\0").into_bytes()
}

pub(crate) fn keep_name(keep: Keep) -> &'static str {
    match keep {
        Keep::None => "none",
        Keep::Last => "last",
        Keep::Log => "log",
    }
}

pub(crate) fn keep_from(name: &str) -> Keep {
    if name == "last" {
        Keep::Last
    } else {
        Keep::Log
    }
}

/// A stored JSON text as a value; what cannot be read is null.
pub(crate) fn json_value(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or(Value::Null)
}

fn now() -> String {
    rfc3339(unix_now())
}

impl Store {
    fn new(backend: Box<dyn Backend>) -> Store {
        Store {
            backend: Mutex::new(backend),
            keys: None,
            key_file: None,
        }
    }

    /// The store, sealing with `keys` from now on. `key_file` is where they
    /// were looked for, which an error names when a value does not open.
    pub fn with_keys(self, keys: Option<seal::Keys>, key_file: Option<PathBuf>) -> Store {
        Store {
            keys,
            key_file,
            ..self
        }
    }

    /// Open the SQLite file, creating it and bringing its schema up to date.
    pub fn open(path: &Path) -> Result<Store> {
        Ok(Store::new(Box::new(SqliteBackend::open(path)?)))
    }

    /// Open the store at `target`, bringing its schema up to date.
    pub fn connect(target: &Target) -> Result<Store> {
        Store::connect_trusting(target, None)
    }

    /// As [`Store::connect`], with the certificates in the PEM file `ca`
    /// trusted as well for a PostgreSQL connection with `sslmode=require`.
    pub fn connect_trusting(target: &Target, ca: Option<&Path>) -> Result<Store> {
        match target {
            Target::Sqlite(path) => Store::open(path),
            Target::Postgres(url) => Ok(Store::new(Box::new(PostgresBackend::connect_trusting(
                url, ca,
            )?))),
        }
    }

    // A backend that panicked part-way holds no half-done transaction, as
    // each runs in one call, so the lock is taken back.
    fn backend(&self) -> MutexGuard<'_, Box<dyn Backend>> {
        self.backend
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// "sqlite" or "postgres".
    pub fn backend_name(&self) -> &'static str {
        self.backend().name()
    }

    pub fn schema_version(&self) -> Result<i64> {
        self.backend().schema_version()
    }

    // ── Messages ────────────────────────────────────────────────────────────

    /// The last message id taken in from a host; 0 before the first.
    pub fn cursor(&self, host: &str) -> Result<u64> {
        self.backend().cursor(host)
    }

    /// Take in a message from a host's channel and move the host's cursor, in
    /// one transaction. A `none` message only moves the cursor. Returns what
    /// was stored, or nothing when it was already there or is not kept.
    pub fn take_in(&self, host: &str, message: &Message) -> Result<Option<Stored>> {
        self.backend().take_in(host, message)
    }

    /// Forget where a host's channel was read up to: its counter started again.
    pub fn reset_cursor(&self, host: &str) -> Result<()> {
        self.backend().reset_cursor(host)
    }

    /// Messages, newest first, and the `before` for the next page when there is one.
    pub fn inbox(&self, filter: &Filter<'_>) -> Result<(Vec<Stored>, Option<i64>)> {
        let limit = filter.limit.clamp(1, 1000);
        // One more than asked for says whether there is a next page.
        let mut messages = self.backend().inbox(filter, limit + 1)?;
        let next = if messages.len() > limit {
            messages.truncate(limit);
            messages.last().map(|message| message.seq)
        } else {
            None
        };
        Ok((messages, next))
    }

    /// Mark a message done or not. False when there is no such message.
    pub fn set_done(&self, host: &str, id: u64, done: bool) -> Result<bool> {
        self.backend().set_done(host, id, done)
    }

    // ── Key-value ───────────────────────────────────────────────────────────

    pub fn get(&self, namespace: &str, key: &str) -> Result<Option<Entry>> {
        self.backend().get(namespace, key)
    }

    /// Set a value. Returns its new revision.
    pub fn put(&self, namespace: &str, key: &str, value: &Value, when: When) -> Result<i64> {
        let text = value.to_string();
        if text.len() > VALUE_LIMIT {
            return Err(Error::TooLarge);
        }
        self.backend().put(namespace, key, &text, when, &now())
    }

    /// Remove a key. False when it was not there.
    pub fn remove(&self, namespace: &str, key: &str, when: When) -> Result<bool> {
        self.backend().remove(namespace, key, when)
    }

    /// A namespace's entries, by key.
    pub fn list(&self, namespace: &str) -> Result<Vec<Entry>> {
        self.backend().list(namespace)
    }

    // ── Jobs ────────────────────────────────────────────────────────────────

    /// Add a job, and return the job the store has for it: this one, or
    /// the one added before with the same subject, action and token.
    pub fn add_job(&self, job: &Job) -> Result<Job> {
        self.backend().add_job(job)
    }

    /// The job started with this token for this subject and action.
    pub fn job_by_token(&self, subject: &str, action: &str, token: &str) -> Result<Option<Job>> {
        self.backend().job_by_token(subject, action, token)
    }

    pub fn end_job(&self, id: &str, exit_code: Option<i32>) -> Result<()> {
        self.backend().end_job(id, exit_code, &now())
    }

    /// Jobs that were running when the program that ran them went away.
    pub fn end_lost_jobs(&self) -> Result<usize> {
        self.backend().end_lost_jobs(&now())
    }

    pub fn job(&self, id: &str) -> Result<Option<Job>> {
        self.backend().job(id)
    }

    /// Jobs, newest first.
    pub fn jobs(&self, limit: usize) -> Result<Vec<Job>> {
        self.backend().jobs(limit.clamp(1, 1000))
    }

    // ── Pruning ─────────────────────────────────────────────────────────────

    /// Drop messages and ended jobs older than `days`. Key-value and state
    /// are never pruned. Returns the output files of the jobs dropped, for
    /// removing.
    pub fn prune(&self, days: u64) -> Result<Vec<String>> {
        let cutoff = rfc3339(unix_now().saturating_sub(days * 86400));
        self.backend().prune(&cutoff)
    }

    // ── State ───────────────────────────────────────────────────────────────

    /// A key is loaded, so values written to `state` are sealed.
    pub fn encrypted(&self) -> bool {
        self.keys.is_some()
    }

    fn row(
        &self,
        kind: &str,
        scope: &str,
        name: &str,
        plain: &[u8],
        updated: &str,
    ) -> Result<StateRow> {
        let (value, sealed) = match &self.keys {
            Some(keys) => (seal_with(keys, kind, scope, name, plain)?, true),
            None => (plain.to_vec(), false),
        };
        Ok(StateRow {
            kind: kind.to_owned(),
            scope: scope.to_owned(),
            name: name.to_owned(),
            value,
            sealed,
            updated: updated.to_owned(),
        })
    }

    /// The plain value of a row. A plain row is taken as it is, even with a
    /// key loaded, as a store being converted holds both.
    fn opened(&self, row: StateRow) -> Result<Vec<u8>> {
        if !row.sealed {
            return Ok(row.value);
        }
        let encrypted = |id: Option<[u8; 8]>| Error::Encrypted {
            key_file: self.key_file.clone(),
            key_id: id.map(|id| seal::hex(&id)),
        };
        let Some(keys) = &self.keys else {
            return Err(encrypted(seal::key_id(&row.value)));
        };
        keys.open(&context(&row.kind, &row.scope, &row.name), &row.value)
            .map_err(|error| match error {
                seal::Error::NoKey(id) => encrypted(Some(id)),
                error => {
                    Error::Sealing(format!("{} {:?} in the store: {error}", row.kind, row.name))
                }
            })
    }

    pub fn read(&self, kind: &str, scope: &str, name: &str) -> Result<Option<Vec<u8>>> {
        match self.backend().state(kind, scope, name)? {
            Some(row) => self.opened(row).map(Some),
            None => Ok(None),
        }
    }

    pub fn read_json(&self, kind: &str, scope: &str, name: &str) -> Result<Option<Value>> {
        match self.read(kind, scope, name)? {
            Some(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|error| {
                Error::Database(format!("{kind} {name:?} in the store is not JSON: {error}"))
            }),
            None => Ok(None),
        }
    }

    /// Insert or replace a value; it is updated now.
    pub fn write(&self, kind: &str, scope: &str, name: &str, value: &[u8]) -> Result<()> {
        let row = self.row(kind, scope, name, value, &now())?;
        self.backend().put_state(&row)
    }

    pub fn write_json(&self, kind: &str, scope: &str, name: &str, value: &Value) -> Result<()> {
        self.write(kind, scope, name, value.to_string().as_bytes())
    }

    /// Remove a value. False when it was not there.
    pub fn forget(&self, kind: &str, scope: &str, name: &str) -> Result<bool> {
        self.backend().remove_state(kind, scope, name)
    }

    /// The names a scope has values of a kind for, in order. Nothing is unsealed.
    pub fn names(&self, kind: &str, scope: &str) -> Result<Vec<String>> {
        let rows = self.backend().states(kind, scope)?;
        Ok(rows.into_iter().map(|row| row.name).collect())
    }

    /// A scope's values of a kind, unsealed, by name.
    pub fn entries(&self, kind: &str, scope: &str) -> Result<Vec<(String, Vec<u8>)>> {
        let rows = self.backend().states(kind, scope)?;
        rows.into_iter()
            .map(|row| {
                let name = row.name.clone();
                Ok((name, self.opened(row)?))
            })
            .collect()
    }

    /// In one transaction: everything of `scope` in `kinds` is replaced by
    /// `items` (kind, name, value).
    pub fn replace(
        &self,
        scope: &str,
        kinds: &[&str],
        items: &[(&str, &str, Vec<u8>)],
    ) -> Result<()> {
        let updated = now();
        let rows = items
            .iter()
            .map(|(kind, name, value)| self.row(kind, scope, name, value, &updated))
            .collect::<Result<Vec<_>>>()?;
        self.backend().replace_states(scope, kinds, &rows)
    }

    /// Several writes (kind, scope, name, value) and removals (kind, scope,
    /// name) in one transaction, for a change that must not be half done.
    pub fn change(
        &self,
        writes: &[(&str, &str, &str, Vec<u8>)],
        removals: &[(&str, &str, &str)],
    ) -> Result<()> {
        let updated = now();
        let rows = writes
            .iter()
            .map(|(kind, scope, name, value)| self.row(kind, scope, name, value, &updated))
            .collect::<Result<Vec<_>>>()?;
        let removals: Vec<_> = removals
            .iter()
            .map(|(kind, scope, name)| {
                ((*kind).to_owned(), (*scope).to_owned(), (*name).to_owned())
            })
            .collect();
        self.backend().change_states(&rows, &removals)
    }

    /// The ids of the keys the sealed rows of `state` were made with.
    pub fn key_ids(&self) -> Result<BTreeSet<[u8; 8]>> {
        let rows = self.backend().all_states()?;
        Ok(rows
            .iter()
            .filter(|row| row.sealed)
            .filter_map(|row| seal::key_id(&row.value))
            .collect())
    }

    /// How many rows of `state` are sealed and plain, and with which keys.
    pub fn seal_report(&self) -> Result<SealReport> {
        let rows = self.backend().all_states()?;
        let sealed = rows.iter().filter(|row| row.sealed).count();
        let ids: BTreeSet<_> = rows
            .iter()
            .filter(|row| row.sealed)
            .filter_map(|row| seal::key_id(&row.value))
            .collect();
        Ok(SealReport {
            sealed,
            plain: rows.len() - sealed,
            key_ids: ids.iter().map(|id| seal::hex(id)).collect(),
        })
    }

    /// Rewrite every row of `state` in one transaction: sealed with `to`'s
    /// current key, or plain when `to` is `None`. The rows are read with this
    /// store's keys, which stay as they are, and keep their `updated`.
    /// Returns how many rows were rewritten.
    pub fn reseal(&self, to: Option<&seal::Keys>) -> Result<usize> {
        let rows = self.backend().all_states()?;
        let mut pairs = Vec::with_capacity(rows.len());
        for row in rows {
            let plain = self.opened(row.clone())?;
            let (value, sealed) = match to {
                Some(keys) => (
                    seal_with(keys, &row.kind, &row.scope, &row.name, &plain)?,
                    true,
                ),
                None => (plain, false),
            };
            let new = StateRow {
                value,
                sealed,
                ..row.clone()
            };
            pairs.push((row, new));
        }
        if !self.backend().swap_states(&pairs)? {
            return Err(Error::Database(
                "the store changed while it was being rewritten: nothing was changed, run it again"
                    .into(),
            ));
        }
        Ok(pairs.len())
    }

    // ── Copying ─────────────────────────────────────────────────────────────

    /// Every row of a table as it is stored, sealed values sealed.
    pub fn export(&self, table: Table) -> Result<Rows> {
        self.backend().export(table)
    }

    /// Add rows as they are, in one transaction; rows there already stay.
    /// Returns how many were added.
    pub fn import(&self, rows: &Rows) -> Result<usize> {
        self.backend().import(rows)
    }

    pub fn count(&self, table: Table) -> Result<u64> {
        self.backend().count(table)
    }
}

/// What a copy did with one table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Copied {
    pub table: Table,
    /// The rows of the source that were to be copied.
    pub found: usize,
    /// Those the target did not have yet.
    pub added: usize,
}

/// What [`copy`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CopyReport {
    pub tables: Vec<Copied>,
    /// Rows of `served:` scopes, a cache of berth servers, which were left out.
    pub left_out: usize,
}

/// Whether a scope of `state` holds what a client knows of a berth server,
/// which its next mirror writes again.
pub fn served_scope(scope: &str) -> bool {
    scope.starts_with("served:")
}

/// Copy every table of `from` into `to`, the values as they are, sealed or
/// plain: both are stores of the same installation, opened with its key. A
/// target that holds anything in `state`, `messages`, `kv` or `jobs` is
/// refused unless `into_existing`, and then keeps its rows and gets the
/// rest. The scopes of berth servers are left out.
pub fn copy(from: &Store, to: &Store, into_existing: bool) -> Result<CopyReport> {
    if !into_existing {
        for table in [Table::State, Table::Messages, Table::Kv, Table::Jobs] {
            let count = to.count(table)?;
            if count > 0 {
                return Err(Error::Database(format!(
                    "the target already holds {count} rows in {}: give --into-existing to add to it, keeping what it has",
                    table.name()
                )));
            }
        }
    }
    let mut report = CopyReport::default();
    for table in Table::ALL {
        let rows = match from.export(table)? {
            Rows::State(rows) => {
                let before = rows.len();
                let kept: Vec<StateRow> = rows
                    .into_iter()
                    .filter(|row| !served_scope(&row.scope))
                    .collect();
                report.left_out = before - kept.len();
                Rows::State(kept)
            }
            rows => rows,
        };
        let added = to.import(&rows)?;
        report.tables.push(Copied {
            table,
            found: rows.len(),
            added,
        });
    }
    Ok(report)
}

fn seal_with(
    keys: &seal::Keys,
    kind: &str,
    scope: &str,
    name: &str,
    plain: &[u8],
) -> Result<Vec<u8>> {
    keys.seal(&context(kind, scope, name), plain)
        .map_err(|error| Error::Sealing(error.to_string()))
}
