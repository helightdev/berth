//! The store in a PostgreSQL database.
//!
//! The `postgres` client runs a runtime of its own and panics when it is used
//! from inside another, as from `berth serve`'s handlers. So the client lives
//! on a thread of its own, and each call hands it work and waits for the
//! answer.

use super::{
    Backend, CursorRow, Entry, Error, Filter, Job, KvRow, MessageRow, Result, Rows, StateRow,
    Stored, Table, When, json_value, keep_from, keep_name,
};
use berth_api::{From, Keep, Level, Message};
use postgres::config::SslMode;
use postgres::{Client, Config, NoTls, Row};
use std::path::Path;
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;
use tokio_postgres_rustls::MakeRustlsConnect;

type PgError = postgres::Error;
type PgResult<T> = std::result::Result<T, PgError>;

/// Each step takes the schema one version further, as in the SQLite file;
/// the one row of `schema_version` counts them. Times are text as there, in
/// the "C" collation so that they compare as SQLite compares them.
pub(super) const MIGRATIONS: &[&str] = &[
    "CREATE TABLE messages(
         seq BIGINT NOT NULL UNIQUE, host TEXT NOT NULL, id BIGINT NOT NULL,
         topic TEXT NOT NULL, body TEXT NOT NULL, keep TEXT NOT NULL,
         from_level TEXT NOT NULL, from_uid BIGINT NOT NULL, session TEXT,
         time TEXT COLLATE \"C\" NOT NULL, done BOOLEAN NOT NULL DEFAULT FALSE,
         PRIMARY KEY (host, id));
     CREATE TABLE cursors(host TEXT PRIMARY KEY, last_id BIGINT NOT NULL);
     CREATE TABLE kv(
         namespace TEXT NOT NULL, key TEXT COLLATE \"C\" NOT NULL, value TEXT NOT NULL,
         revision BIGINT NOT NULL, updated TEXT NOT NULL,
         PRIMARY KEY (namespace, key));",
    "CREATE INDEX messages_topic ON messages(topic, time);
     CREATE TABLE jobs(
         id TEXT COLLATE \"C\" PRIMARY KEY, host TEXT NOT NULL, what TEXT NOT NULL,
         started TEXT COLLATE \"C\" NOT NULL, ended TEXT, exit_code INTEGER,
         output_path TEXT NOT NULL);",
    "CREATE TABLE state(
         kind TEXT COLLATE \"C\" NOT NULL, scope TEXT COLLATE \"C\" NOT NULL,
         name TEXT COLLATE \"C\" NOT NULL, value BYTEA NOT NULL, sealed BOOLEAN NOT NULL,
         updated TEXT NOT NULL, PRIMARY KEY (kind, scope, name));",
];

/// The advisory lock that keeps two berths from migrating at once.
const MIGRATION_LOCK: i64 = 0x6265_7274_6800;

impl std::convert::From<PgError> for Error {
    fn from(error: PgError) -> Self {
        Error::Database(error.to_string())
    }
}

/// The connection, on its thread.
struct Link {
    config: Config,
    tls: Option<MakeRustlsConnect>,
    client: Option<Client>,
    /// The password, kept out of every error.
    secret: Option<String>,
}

impl Link {
    fn error(&self, error: impl std::fmt::Display) -> Error {
        let mut text = error.to_string();
        if let Some(secret) = self.secret.as_deref().filter(|secret| !secret.is_empty()) {
            text = text.replace(secret, "***");
        }
        Error::Database(text)
    }

    /// The client, connected again if the connection is gone. True when it
    /// is a new connection.
    fn client(&mut self) -> Result<(&mut Client, bool)> {
        let fresh = match &self.client {
            Some(client) => client.is_closed(),
            None => true,
        };
        if fresh {
            self.client = None;
            let connected = match &self.tls {
                Some(tls) => self.config.connect(tls.clone()),
                None => self.config.connect(NoTls),
            };
            let client = connected
                .map_err(|error| self.error(format_args!("connecting to PostgreSQL: {error}")))?;
            self.client = Some(client);
        }
        Ok((self.client.as_mut().expect("connected"), fresh))
    }

    /// Run work on the connection. When it failed on a connection that had
    /// been closed, as by a restart of the server, connect again and run it
    /// once more.
    fn run<T>(&mut self, work: &dyn Fn(&mut Client) -> PgResult<T>) -> Result<T> {
        let (client, fresh) = self.client()?;
        match work(client) {
            Ok(value) => Ok(value),
            // The server says why it closes the connection first, so the
            // error can be that, a fatal one, rather than a closed connection.
            Err(error) if !fresh && (closed(&error) || client.is_closed()) => {
                self.client = None;
                let (client, _) = self.client()?;
                work(client).map_err(|error| self.error(error))
            }
            Err(error) => Err(self.error(error)),
        }
    }
}

/// Whether the error ended the connection: a fatal error ends the session.
fn closed(error: &PgError) -> bool {
    error.is_closed()
        || error
            .as_db_error()
            .is_some_and(|db| matches!(db.severity(), "FATAL" | "PANIC"))
}

type Work = Box<dyn FnOnce(&mut Link) + Send>;

pub struct PostgresBackend {
    work: Option<mpsc::Sender<Work>>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for PostgresBackend {
    fn drop(&mut self) {
        // The thread ends when there is no more work, and closes the connection.
        self.work = None;
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The public roots, and the certificates of the PEM file `ca` if there is one.
fn roots(ca: Option<&Path>) -> Result<rustls::RootCertStore> {
    use rustls::pki_types::{CertificateDer, pem::PemObject};
    let mut roots =
        rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let Some(ca) = ca else {
        return Ok(roots);
    };
    let shown = ca.display();
    let text =
        std::fs::read(ca).map_err(|error| Error::Database(format!("store_ca {shown}: {error}")))?;
    let mut added = 0;
    for certificate in CertificateDer::pem_slice_iter(&text) {
        let certificate =
            certificate.map_err(|error| Error::Database(format!("store_ca {shown}: {error}")))?;
        roots
            .add(certificate)
            .map_err(|error| Error::Database(format!("store_ca {shown}: {error}")))?;
        added += 1;
    }
    if added == 0 {
        return Err(Error::Database(format!(
            "store_ca {shown} holds no certificate"
        )));
    }
    Ok(roots)
}

fn tls(ca: Option<&Path>) -> Result<MakeRustlsConnect> {
    let roots = roots(ca)?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|error| Error::Database(error.to_string()))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(MakeRustlsConnect::new(config))
}

fn stored(row: &Row) -> Stored {
    let keep: String = row.get(5);
    let level: String = row.get(6);
    let body: String = row.get(4);
    Stored {
        seq: row.get(0),
        host: row.get(1),
        message: Message {
            id: row.get::<_, i64>(2) as u64,
            topic: row.get(3),
            body: json_value(&body),
            keep: keep_from(&keep),
            from: From {
                level: if level == "agent" {
                    Level::Agent
                } else {
                    Level::Host
                },
                uid: row.get::<_, i64>(7) as u32,
                session: row.get(8),
            },
            time: row.get(9),
        },
        done: row.get(10),
    }
}

fn entry(row: &Row) -> Entry {
    let value: String = row.get(1);
    Entry {
        key: row.get(0),
        value: json_value(&value),
        revision: row.get(2),
        updated: row.get(3),
    }
}

fn job(row: &Row) -> Job {
    Job {
        id: row.get(0),
        host: row.get(1),
        what: row.get(2),
        started: row.get(3),
        ended: row.get(4),
        exit_code: row.get(5),
        output_path: row.get(6),
    }
}

const STATE_COLUMNS: &str = "kind, scope, name, value, sealed, updated";

fn state_row(row: &Row) -> StateRow {
    StateRow {
        kind: row.get(0),
        scope: row.get(1),
        name: row.get(2),
        value: row.get(3),
        sealed: row.get(4),
        updated: row.get(5),
    }
}

const PUT_STATE: &str = "INSERT INTO state(kind, scope, name, value, sealed, updated)
     VALUES ($1, $2, $3, $4, $5, $6)
     ON CONFLICT (kind, scope, name) DO UPDATE
     SET value = excluded.value, sealed = excluded.sealed, updated = excluded.updated";

fn put_state(client: &mut impl postgres::GenericClient, row: &StateRow) -> PgResult<()> {
    client.execute(
        PUT_STATE,
        &[
            &row.kind,
            &row.scope,
            &row.name,
            &row.value,
            &row.sealed,
            &row.updated,
        ],
    )?;
    Ok(())
}

fn revision(
    client: &mut impl postgres::GenericClient,
    namespace: &str,
    key: &str,
) -> PgResult<Option<i64>> {
    Ok(client
        .query_opt(
            "SELECT revision FROM kv WHERE namespace = $1 AND key = $2",
            &[&namespace, &key],
        )?
        .map(|row| row.get(0)))
}

/// Bring the schema up to date, one step to a transaction. Each step takes
/// the lock and reads the version again, so that two berths starting at once
/// do not both run it.
fn migrate(client: &mut Client) -> PgResult<std::result::Result<(), i64>> {
    loop {
        let mut transaction = client.transaction()?;
        transaction.execute("SELECT pg_advisory_xact_lock($1)", &[&MIGRATION_LOCK])?;
        transaction
            .batch_execute("CREATE TABLE IF NOT EXISTS schema_version(version INTEGER NOT NULL)")?;
        let version = match transaction.query_opt("SELECT version FROM schema_version", &[])? {
            Some(row) => row.get::<_, i32>(0) as usize,
            None => {
                transaction.execute("INSERT INTO schema_version(version) VALUES (0)", &[])?;
                0
            }
        };
        if version > MIGRATIONS.len() {
            return Ok(Err(version as i64));
        }
        if version == MIGRATIONS.len() {
            transaction.commit()?;
            return Ok(Ok(()));
        }
        transaction.batch_execute(MIGRATIONS[version])?;
        transaction.execute(
            "UPDATE schema_version SET version = $1",
            &[&(version as i32 + 1)],
        )?;
        transaction.commit()?;
    }
}

/// The URL with `sslmode=verify-full` or `verify-ca` as `require`, which
/// the client does not know but which is what `require` does here: the
/// certificate and the host name are always checked.
fn known_sslmode(url: &str) -> String {
    let Some((base, query)) = url.split_once('?') else {
        return url.to_owned();
    };
    let query: Vec<&str> = query
        .split('&')
        .map(|pair| match pair {
            "sslmode=verify-full" | "sslmode=verify-ca" => "sslmode=require",
            pair => pair,
        })
        .collect();
    format!("{base}?{}", query.join("&"))
}

/// The connection to the database at `url`, not made yet.
fn link(url: &str, ca: Option<&Path>) -> Result<Link> {
    let mut config: Config = known_sslmode(url)
        .parse()
        .map_err(|error| Error::Database(format!("the PostgreSQL address: {error}")))?;
    let tls = match config.get_ssl_mode() {
        SslMode::Require => Some(tls(ca)?),
        // Without `sslmode=require` the connection is plain, rather than TLS
        // only when the server happens to offer it.
        _ => {
            config.ssl_mode(SslMode::Disable);
            None
        }
    };
    if config.get_connect_timeout().is_none() {
        config.connect_timeout(Duration::from_secs(10));
    }
    let secret = config
        .get_password()
        .map(|password| String::from_utf8_lossy(password).into_owned());
    Ok(Link {
        config,
        tls,
        client: None,
        secret,
    })
}

/// Where the database at `url` is, without the user or the password.
pub(super) fn describe(url: &str) -> String {
    let Ok(config) = known_sslmode(url).parse::<Config>() else {
        return "PostgreSQL at an address that does not parse".into();
    };
    let host = match config.get_hosts().first() {
        Some(postgres::config::Host::Tcp(host)) => host.clone(),
        Some(postgres::config::Host::Unix(path)) => path.display().to_string(),
        None => "localhost".into(),
    };
    let port = config.get_ports().first().copied().unwrap_or(5432);
    // Without a name the database is the user's, whose name is not shown.
    match config.get_dbname() {
        Some(database) => format!("PostgreSQL at {host}:{port}, database {database}"),
        None => format!("PostgreSQL at {host}:{port}, the user's own database"),
    }
}

/// Run SQL on the database at `url`, from a thread of its own, as tests set
/// a database up.
#[cfg(test)]
pub(super) fn execute(url: &str, sql: &str) -> Result<()> {
    let mut link = link(url, None)?;
    let sql = sql.to_owned();
    std::thread::spawn(move || link.run(&|client| client.batch_execute(&sql)))
        .join()
        .expect("the thread ran")
}

impl PostgresBackend {
    /// Connect to the database at `url` and bring its schema up to date.
    pub fn connect(url: &str) -> Result<PostgresBackend> {
        PostgresBackend::connect_trusting(url, None)
    }

    /// As [`PostgresBackend::connect`], trusting the certificates in the PEM
    /// file `ca` as well as the public roots.
    pub fn connect_trusting(url: &str, ca: Option<&Path>) -> Result<PostgresBackend> {
        let mut link = link(url, ca)?;
        let (work, jobs) = mpsc::channel::<Work>();
        let thread = std::thread::Builder::new()
            .name("berth-postgres".into())
            .spawn(move || {
                for job in jobs {
                    job(&mut link);
                }
            })
            .map_err(|error| Error::Database(error.to_string()))?;
        let mut backend = PostgresBackend {
            work: Some(work),
            thread: Some(thread),
        };
        if let Err(version) = backend.call(migrate)? {
            return Err(Error::Database(format!(
                "the database was written by a newer berth (schema {version})"
            )));
        }
        Ok(backend)
    }

    /// Run work on the connection's thread and wait for it.
    fn call<T: Send + 'static>(
        &mut self,
        work: impl Fn(&mut Client) -> PgResult<T> + Send + 'static,
    ) -> Result<T> {
        let gone = || Error::Database("the PostgreSQL connection's thread has stopped".into());
        let (reply, answer) = mpsc::sync_channel(1);
        let job: Work = Box::new(move |link| {
            let _ = reply.send(link.run(&work));
        });
        self.work
            .as_ref()
            .ok_or_else(gone)?
            .send(job)
            .map_err(|_| gone())?;
        answer.recv().map_err(|_| gone())?
    }
}

impl Backend for PostgresBackend {
    fn name(&self) -> &'static str {
        "postgres"
    }

    fn schema_version(&mut self) -> Result<i64> {
        self.call(|client| {
            let row = client.query_one("SELECT version FROM schema_version", &[])?;
            Ok(row.get::<_, i32>(0) as i64)
        })
    }

    fn cursor(&mut self, host: &str) -> Result<u64> {
        let host = host.to_owned();
        self.call(move |client| {
            let row = client.query_opt("SELECT last_id FROM cursors WHERE host = $1", &[&host])?;
            Ok(row.map_or(0, |row| row.get::<_, i64>(0)) as u64)
        })
    }

    fn take_in(&mut self, host: &str, message: &Message) -> Result<Option<Stored>> {
        let host = host.to_owned();
        let message = message.clone();
        self.call(move |client| {
            let mut transaction = client.transaction()?;
            let mut taken = None;
            if message.keep != Keep::None {
                // As in SQLite, a message's seq is one past the highest, so
                // the messages are taken in one at a time.
                transaction.batch_execute("LOCK TABLE messages IN SHARE ROW EXCLUSIVE MODE")?;
                if message.keep == Keep::Last {
                    transaction.execute(
                        "DELETE FROM messages WHERE host = $1 AND topic = $2 AND keep = 'last' AND id < $3",
                        &[&host, &message.topic, &(message.id as i64)],
                    )?;
                }
                let seq: i64 = transaction
                    .query_one("SELECT COALESCE(MAX(seq), 0) + 1 FROM messages", &[])?
                    .get(0);
                let level = match message.from.level {
                    Level::Host => "host",
                    Level::Agent => "agent",
                };
                let added = transaction.execute(
                    "INSERT INTO messages(seq, host, id, topic, body, keep, from_level, from_uid, session, time, done)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, FALSE)
                     ON CONFLICT (host, id) DO NOTHING",
                    &[
                        &seq,
                        &host,
                        &(message.id as i64),
                        &message.topic,
                        &message.body.to_string(),
                        &keep_name(message.keep),
                        &level,
                        &(message.from.uid as i64),
                        &message.from.session,
                        &message.time,
                    ],
                )?;
                if added == 1 {
                    taken = Some(Stored {
                        seq,
                        host: host.clone(),
                        message: message.clone(),
                        done: false,
                    });
                }
            }
            transaction.execute(
                "INSERT INTO cursors(host, last_id) VALUES ($1, $2)
                 ON CONFLICT (host) DO UPDATE SET last_id = GREATEST(cursors.last_id, excluded.last_id)",
                &[&host, &(message.id as i64)],
            )?;
            transaction.commit()?;
            Ok(taken)
        })
    }

    fn reset_cursor(&mut self, host: &str) -> Result<()> {
        let host = host.to_owned();
        self.call(move |client| {
            client.execute("DELETE FROM cursors WHERE host = $1", &[&host])?;
            Ok(())
        })
    }

    fn inbox(&mut self, filter: &Filter<'_>, limit: usize) -> Result<Vec<Stored>> {
        let host = filter.host.map(str::to_owned);
        let topic = filter.topic.unwrap_or("").to_owned();
        let (before, done) = (filter.before, filter.done);
        self.call(move |client| {
            let rows = client.query(
                "SELECT seq, host, id, topic, body, keep, from_level, from_uid, session, time, done
                 FROM messages
                 WHERE ($1::TEXT IS NULL OR host = $1)
                   AND left(topic, char_length($2::TEXT)) = $2
                   AND ($3::BIGINT IS NULL OR seq < $3)
                   AND ($4::BOOLEAN IS NULL OR done = $4)
                 ORDER BY seq DESC LIMIT $5",
                &[&host, &topic, &before, &done, &(limit as i64)],
            )?;
            Ok(rows.iter().map(stored).collect())
        })
    }

    fn set_done(&mut self, host: &str, id: u64, done: bool) -> Result<bool> {
        let host = host.to_owned();
        self.call(move |client| {
            let changed = client.execute(
                "UPDATE messages SET done = $3 WHERE host = $1 AND id = $2",
                &[&host, &(id as i64), &done],
            )?;
            Ok(changed == 1)
        })
    }

    fn get(&mut self, namespace: &str, key: &str) -> Result<Option<Entry>> {
        let (namespace, key) = (namespace.to_owned(), key.to_owned());
        self.call(move |client| {
            let row = client.query_opt(
                "SELECT key, value, revision, updated FROM kv WHERE namespace = $1 AND key = $2",
                &[&namespace, &key],
            )?;
            Ok(row.as_ref().map(entry))
        })
    }

    fn put(
        &mut self,
        namespace: &str,
        key: &str,
        text: &str,
        when: When,
        updated: &str,
    ) -> Result<i64> {
        let (namespace, key) = (namespace.to_owned(), key.to_owned());
        let (text, updated) = (text.to_owned(), updated.to_owned());
        // Each case is one statement, which holds under two writers; what is
        // there is read only to say why one did not.
        let written = self.call(move |client| {
            let row = match when {
                When::Always => client.query_opt(
                    "INSERT INTO kv(namespace, key, value, revision, updated) VALUES ($1, $2, $3, 1, $4)
                     ON CONFLICT (namespace, key) DO UPDATE
                     SET value = excluded.value, revision = kv.revision + 1, updated = excluded.updated
                     RETURNING revision",
                    &[&namespace, &key, &text, &updated],
                )?,
                When::New => client.query_opt(
                    "INSERT INTO kv(namespace, key, value, revision, updated) VALUES ($1, $2, $3, 1, $4)
                     ON CONFLICT (namespace, key) DO NOTHING
                     RETURNING revision",
                    &[&namespace, &key, &text, &updated],
                )?,
                When::At(at) => client.query_opt(
                    "UPDATE kv SET value = $3, revision = revision + 1, updated = $4
                     WHERE namespace = $1 AND key = $2 AND revision = $5
                     RETURNING revision",
                    &[&namespace, &key, &text, &updated, &at],
                )?,
            };
            Ok(match row {
                Some(row) => Ok(row.get::<_, i64>(0)),
                None => Err(revision(client, &namespace, &key)?),
            })
        })?;
        written.map_err(Error::Conflict)
    }

    fn remove(&mut self, namespace: &str, key: &str, when: When) -> Result<bool> {
        let (namespace, key) = (namespace.to_owned(), key.to_owned());
        let removed = self.call(move |client| {
            let removed = match when {
                When::At(at) => client.execute(
                    "DELETE FROM kv WHERE namespace = $1 AND key = $2 AND revision = $3",
                    &[&namespace, &key, &at],
                )?,
                When::Always | When::New => client.execute(
                    "DELETE FROM kv WHERE namespace = $1 AND key = $2",
                    &[&namespace, &key],
                )?,
            };
            Ok(match (removed, when) {
                (0, When::At(_)) => Err(revision(client, &namespace, &key)?),
                (removed, _) => Ok(removed == 1),
            })
        })?;
        removed.map_err(Error::Conflict)
    }

    fn list(&mut self, namespace: &str) -> Result<Vec<Entry>> {
        let namespace = namespace.to_owned();
        self.call(move |client| {
            let rows = client.query(
                "SELECT key, value, revision, updated FROM kv WHERE namespace = $1 ORDER BY key",
                &[&namespace],
            )?;
            Ok(rows.iter().map(entry).collect())
        })
    }

    fn add_job(&mut self, job: &Job) -> Result<()> {
        let job = job.clone();
        self.call(move |client| {
            client.execute(
                "INSERT INTO jobs(id, host, what, started, ended, exit_code, output_path)
                 VALUES ($1, $2, $3, $4, $5, $6, $7)",
                &[
                    &job.id,
                    &job.host,
                    &job.what,
                    &job.started,
                    &job.ended,
                    &job.exit_code,
                    &job.output_path,
                ],
            )?;
            Ok(())
        })
    }

    fn end_job(&mut self, id: &str, exit_code: Option<i32>, ended: &str) -> Result<()> {
        let (id, ended) = (id.to_owned(), ended.to_owned());
        self.call(move |client| {
            client.execute(
                "UPDATE jobs SET ended = $2, exit_code = $3 WHERE id = $1 AND ended IS NULL",
                &[&id, &ended, &exit_code],
            )?;
            Ok(())
        })
    }

    fn end_lost_jobs(&mut self, ended: &str) -> Result<usize> {
        let ended = ended.to_owned();
        self.call(move |client| {
            let changed =
                client.execute("UPDATE jobs SET ended = $1 WHERE ended IS NULL", &[&ended])?;
            Ok(changed as usize)
        })
    }

    fn job(&mut self, id: &str) -> Result<Option<Job>> {
        let id = id.to_owned();
        self.call(move |client| {
            let row = client.query_opt(
                "SELECT id, host, what, started, ended, exit_code, output_path FROM jobs WHERE id = $1",
                &[&id],
            )?;
            Ok(row.as_ref().map(job))
        })
    }

    fn jobs(&mut self, limit: usize) -> Result<Vec<Job>> {
        self.call(move |client| {
            let rows = client.query(
                "SELECT id, host, what, started, ended, exit_code, output_path FROM jobs
                 ORDER BY started DESC, id DESC LIMIT $1",
                &[&(limit as i64)],
            )?;
            Ok(rows.iter().map(job).collect())
        })
    }

    fn prune(&mut self, cutoff: &str) -> Result<Vec<String>> {
        let cutoff = cutoff.to_owned();
        self.call(move |client| {
            let mut transaction = client.transaction()?;
            transaction.execute("DELETE FROM messages WHERE time < $1", &[&cutoff])?;
            let rows = transaction.query(
                "DELETE FROM jobs WHERE ended IS NOT NULL AND started < $1 RETURNING output_path",
                &[&cutoff],
            )?;
            transaction.commit()?;
            Ok(rows.iter().map(|row| row.get(0)).collect())
        })
    }

    fn state(&mut self, kind: &str, scope: &str, name: &str) -> Result<Option<StateRow>> {
        let (kind, scope, name) = (kind.to_owned(), scope.to_owned(), name.to_owned());
        self.call(move |client| {
            let row = client.query_opt(
                &format!(
                    "SELECT {STATE_COLUMNS} FROM state WHERE kind = $1 AND scope = $2 AND name = $3"
                ),
                &[&kind, &scope, &name],
            )?;
            Ok(row.as_ref().map(state_row))
        })
    }

    fn states(&mut self, kind: &str, scope: &str) -> Result<Vec<StateRow>> {
        let (kind, scope) = (kind.to_owned(), scope.to_owned());
        self.call(move |client| {
            let rows = client.query(
                &format!(
                    "SELECT {STATE_COLUMNS} FROM state WHERE kind = $1 AND scope = $2 ORDER BY name"
                ),
                &[&kind, &scope],
            )?;
            Ok(rows.iter().map(state_row).collect())
        })
    }

    fn put_state(&mut self, row: &StateRow) -> Result<()> {
        let row = row.clone();
        self.call(move |client| put_state(client, &row))
    }

    fn remove_state(&mut self, kind: &str, scope: &str, name: &str) -> Result<bool> {
        let (kind, scope, name) = (kind.to_owned(), scope.to_owned(), name.to_owned());
        self.call(move |client| {
            let removed = client.execute(
                "DELETE FROM state WHERE kind = $1 AND scope = $2 AND name = $3",
                &[&kind, &scope, &name],
            )?;
            Ok(removed == 1)
        })
    }

    fn replace_states(&mut self, scope: &str, kinds: &[&str], rows: &[StateRow]) -> Result<()> {
        let scope = scope.to_owned();
        let kinds: Vec<String> = kinds.iter().map(|kind| (*kind).to_owned()).collect();
        let rows = rows.to_vec();
        self.call(move |client| {
            let mut transaction = client.transaction()?;
            transaction.execute(
                "DELETE FROM state WHERE scope = $1 AND kind = ANY($2)",
                &[&scope, &kinds],
            )?;
            for row in &rows {
                put_state(&mut transaction, row)?;
            }
            transaction.commit()
        })
    }

    fn all_states(&mut self) -> Result<Vec<StateRow>> {
        self.call(|client| {
            let rows = client.query(
                &format!("SELECT {STATE_COLUMNS} FROM state ORDER BY kind, scope, name"),
                &[],
            )?;
            Ok(rows.iter().map(state_row).collect())
        })
    }

    fn change_states(
        &mut self,
        writes: &[StateRow],
        removals: &[(String, String, String)],
    ) -> Result<()> {
        let (writes, removals) = (writes.to_vec(), removals.to_vec());
        self.call(move |client| {
            let mut transaction = client.transaction()?;
            for row in &writes {
                put_state(&mut transaction, row)?;
            }
            for (kind, scope, name) in &removals {
                transaction.execute(
                    "DELETE FROM state WHERE kind = $1 AND scope = $2 AND name = $3",
                    &[kind, scope, name],
                )?;
            }
            transaction.commit()
        })
    }

    fn swap_states(&mut self, rows: &[(StateRow, StateRow)]) -> Result<bool> {
        let rows = rows.to_vec();
        self.call(move |client| {
            let mut transaction = client.transaction()?;
            // Others may read meanwhile, but not write.
            transaction.batch_execute("LOCK TABLE state IN EXCLUSIVE MODE")?;
            let count: i64 = transaction
                .query_one("SELECT COUNT(*) FROM state", &[])?
                .get(0);
            if count as usize != rows.len() {
                return Ok(false);
            }
            for (old, new) in &rows {
                let changed = transaction.execute(
                    "UPDATE state SET value = $4, sealed = $5
                     WHERE kind = $1 AND scope = $2 AND name = $3 AND value = $6 AND sealed = $7",
                    &[
                        &old.kind,
                        &old.scope,
                        &old.name,
                        &new.value,
                        &new.sealed,
                        &old.value,
                        &old.sealed,
                    ],
                )?;
                if changed != 1 {
                    return Ok(false);
                }
            }
            transaction.commit()?;
            Ok(true)
        })
    }

    fn export(&mut self, table: Table) -> Result<Rows> {
        self.call(move |client| {
            Ok(match table {
                Table::State => {
                    let rows = client.query(
                        &format!("SELECT {STATE_COLUMNS} FROM state ORDER BY kind, scope, name"),
                        &[],
                    )?;
                    Rows::State(rows.iter().map(state_row).collect())
                }
                Table::Messages => {
                    let rows = client.query(
                        "SELECT seq, host, id, topic, body, keep, from_level, from_uid, session, time, done
                         FROM messages ORDER BY seq",
                        &[],
                    )?;
                    Rows::Messages(
                        rows.iter()
                            .map(|row| MessageRow {
                                seq: row.get(0),
                                host: row.get(1),
                                id: row.get(2),
                                topic: row.get(3),
                                body: row.get(4),
                                keep: row.get(5),
                                from_level: row.get(6),
                                from_uid: row.get(7),
                                session: row.get(8),
                                time: row.get(9),
                                done: row.get(10),
                            })
                            .collect(),
                    )
                }
                Table::Cursors => {
                    let rows =
                        client.query("SELECT host, last_id FROM cursors ORDER BY host", &[])?;
                    Rows::Cursors(
                        rows.iter()
                            .map(|row| CursorRow {
                                host: row.get(0),
                                last_id: row.get(1),
                            })
                            .collect(),
                    )
                }
                Table::Kv => {
                    let rows = client.query(
                        "SELECT namespace, key, value, revision, updated FROM kv ORDER BY namespace, key",
                        &[],
                    )?;
                    Rows::Kv(
                        rows.iter()
                            .map(|row| KvRow {
                                namespace: row.get(0),
                                key: row.get(1),
                                value: row.get(2),
                                revision: row.get(3),
                                updated: row.get(4),
                            })
                            .collect(),
                    )
                }
                Table::Jobs => {
                    let rows = client.query(
                        "SELECT id, host, what, started, ended, exit_code, output_path FROM jobs
                         ORDER BY started, id",
                        &[],
                    )?;
                    Rows::Jobs(rows.iter().map(job).collect())
                }
            })
        })
    }

    fn import(&mut self, rows: &Rows) -> Result<usize> {
        let rows = rows.clone();
        self.call(move |client| {
            let mut transaction = client.transaction()?;
            let mut added = 0;
            match &rows {
                Rows::State(rows) => {
                    let statement = transaction.prepare(
                        "INSERT INTO state(kind, scope, name, value, sealed, updated)
                         VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT DO NOTHING",
                    )?;
                    for row in rows {
                        added += transaction.execute(
                            &statement,
                            &[
                                &row.kind,
                                &row.scope,
                                &row.name,
                                &row.value,
                                &row.sealed,
                                &row.updated,
                            ],
                        )?;
                    }
                }
                Rows::Messages(rows) => {
                    let statement = transaction.prepare(
                        "INSERT INTO messages(seq, host, id, topic, body, keep, from_level, from_uid, session, time, done)
                         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11) ON CONFLICT DO NOTHING",
                    )?;
                    for row in rows {
                        added += transaction.execute(
                            &statement,
                            &[
                                &row.seq,
                                &row.host,
                                &row.id,
                                &row.topic,
                                &row.body,
                                &row.keep,
                                &row.from_level,
                                &row.from_uid,
                                &row.session,
                                &row.time,
                                &row.done,
                            ],
                        )?;
                    }
                }
                Rows::Cursors(rows) => {
                    let statement = transaction.prepare(
                        "INSERT INTO cursors(host, last_id) VALUES ($1, $2) ON CONFLICT DO NOTHING",
                    )?;
                    for row in rows {
                        added += transaction.execute(&statement, &[&row.host, &row.last_id])?;
                    }
                }
                Rows::Kv(rows) => {
                    let statement = transaction.prepare(
                        "INSERT INTO kv(namespace, key, value, revision, updated)
                         VALUES ($1, $2, $3, $4, $5) ON CONFLICT DO NOTHING",
                    )?;
                    for row in rows {
                        added += transaction.execute(
                            &statement,
                            &[
                                &row.namespace,
                                &row.key,
                                &row.value,
                                &row.revision,
                                &row.updated,
                            ],
                        )?;
                    }
                }
                Rows::Jobs(rows) => {
                    let statement = transaction.prepare(
                        "INSERT INTO jobs(id, host, what, started, ended, exit_code, output_path)
                         VALUES ($1, $2, $3, $4, $5, $6, $7) ON CONFLICT DO NOTHING",
                    )?;
                    for job in rows {
                        added += transaction.execute(
                            &statement,
                            &[
                                &job.id,
                                &job.host,
                                &job.what,
                                &job.started,
                                &job.ended,
                                &job.exit_code,
                                &job.output_path,
                            ],
                        )?;
                    }
                }
            }
            transaction.commit()?;
            Ok(added as usize)
        })
    }

    fn count(&mut self, table: Table) -> Result<u64> {
        let sql = format!("SELECT COUNT(*) FROM {}", table.name());
        self.call(move |client| Ok(client.query_one(&sql, &[])?.get::<_, i64>(0) as u64))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A made-up authority, self-signed, for no server.
    const CA: &str = "-----BEGIN CERTIFICATE-----
MIIBhjCCAS2gAwIBAgIUFfx8WDjDXux6eDCJclO4Ci3znsEwCgYIKoZIzj0EAwIw
GDEWMBQGA1UEAwwNYmVydGggdGVzdCBDQTAgFw0yNjExMTMxNDM4MzJaGA8yMTI2
MTAyMDE0MzgzMlowGDEWMBQGA1UEAwwNYmVydGggdGVzdCBDQTBZMBMGByqGSM49
AgEGCCqGSM49AwEHA0IABEMWEVRpZaoz7Ljdi5CHiKJdEELekFmjjWB+rQqKtSwd
VsiZv5P3b8bcw8iDDsRgUlcs0xu3QFblxOeSF1rFfFujUzBRMB0GA1UdDgQWBBSQ
lAEePrLb3DFF6WE8gieOoSl7wzAfBgNVHSMEGDAWgBSQlAEePrLb3DFF6WE8gieO
oSl7wzAPBgNVHRMBAf8EBTADAQH/MAoGCCqGSM49BAMCA0cAMEQCIEv8azN9TfVj
jh+hiP8W8T03TM71Q818aMlgFcEOy9kUAiBfsxF17yq3wfDOtKVlKF3kIX2dccb7
pC+r1q+IpC/guA==
-----END CERTIFICATE-----
";

    #[test]
    fn verify_modes_are_taken_as_require() {
        for mode in ["verify-full", "verify-ca"] {
            let url =
                format!("postgres://berth@192.0.2.10/berth?application_name=x&sslmode={mode}");
            assert_eq!(
                known_sslmode(&url),
                "postgres://berth@192.0.2.10/berth?application_name=x&sslmode=require"
            );
            let config: Config = known_sslmode(&url).parse().unwrap();
            assert_eq!(config.get_ssl_mode(), SslMode::Require);
            assert!(
                url.parse::<Config>().is_err(),
                "the client knows {mode} now"
            );
        }
        for url in [
            "postgres://berth@192.0.2.10/berth",
            "postgres://berth@192.0.2.10/berth?sslmode=disable",
        ] {
            assert_eq!(known_sslmode(url), url);
        }
    }

    #[test]
    fn a_private_authority_is_trusted_as_well() {
        let dir = tempfile::tempdir().unwrap();
        let public = roots(None).unwrap().len();
        let file = dir.path().join("ca.pem");
        std::fs::write(&file, format!("{CA}{CA}")).unwrap();
        assert_eq!(roots(Some(&file)).unwrap().len(), public + 2);
        assert!(
            link(
                "postgres://berth@192.0.2.10/berth?sslmode=require",
                Some(&file)
            )
            .is_ok()
        );

        std::fs::write(&file, "no certificate here\n").unwrap();
        let error = roots(Some(&file)).unwrap_err().to_string();
        assert!(error.contains("no certificate"), "{error}");
        let missing = dir.path().join("missing.pem");
        let error = roots(Some(&missing)).unwrap_err().to_string();
        assert!(error.contains("store_ca"), "{error}");
    }

    #[test]
    fn where_the_database_is_says_no_user_or_password() {
        let shown = describe("postgres://admin:made-up@192.0.2.10:6432/berth?sslmode=verify-full");
        assert_eq!(shown, "PostgreSQL at 192.0.2.10:6432, database berth");
        let shown = describe("postgres://admin:made-up@192.0.2.10");
        assert!(
            !shown.contains("admin") && !shown.contains("made-up"),
            "{shown}"
        );
        assert!(shown.contains("192.0.2.10:5432"), "{shown}");
    }
}
