//! The store in a SQLite file.

use super::{
    Backend, CursorRow, Entry, Error, Filter, Job, KvRow, MessageRow, Result, Rows, StateRow,
    Stored, Table, When, json_value, keep_from, keep_name,
};
use berth_api::{From, Level, Message};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;

/// Each step takes the schema one version further; `user_version` counts them.
pub(super) const MIGRATIONS: &[&str] = &[
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
    "CREATE TABLE state(
         kind TEXT NOT NULL, scope TEXT NOT NULL, name TEXT NOT NULL,
         value BLOB NOT NULL, sealed INTEGER NOT NULL, updated TEXT NOT NULL,
         PRIMARY KEY (kind, scope, name));",
];

impl std::convert::From<rusqlite::Error> for Error {
    fn from(error: rusqlite::Error) -> Self {
        Error::Database(error.to_string())
    }
}

pub struct SqliteBackend {
    db: Connection,
}

fn io(error: std::io::Error) -> Error {
    Error::Database(error.to_string())
}

/// Make the file if it is not there and keep it to its owner: it holds
/// what the hosts sent and the state, some of it secret.
fn private_file(path: &Path) -> Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
        .map_err(io)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(io)
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
            body: json_value(&body),
            keep: keep_from(&keep),
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

fn entry(row: &rusqlite::Row<'_>) -> rusqlite::Result<Entry> {
    let value: String = row.get(1)?;
    Ok(Entry {
        key: row.get(0)?,
        value: json_value(&value),
        revision: row.get(2)?,
        updated: row.get(3)?,
    })
}

const STATE_COLUMNS: &str = "kind, scope, name, value, sealed, updated";

fn state_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StateRow> {
    Ok(StateRow {
        kind: row.get(0)?,
        scope: row.get(1)?,
        name: row.get(2)?,
        value: row.get(3)?,
        sealed: row.get(4)?,
        updated: row.get(5)?,
    })
}

const PUT_STATE: &str = "INSERT INTO state(kind, scope, name, value, sealed, updated)
     VALUES (?1, ?2, ?3, ?4, ?5, ?6)
     ON CONFLICT (kind, scope, name) DO UPDATE
     SET value = excluded.value, sealed = excluded.sealed, updated = excluded.updated";

fn put_state(db: &Connection, row: &StateRow) -> rusqlite::Result<()> {
    db.execute(
        PUT_STATE,
        params![
            row.kind,
            row.scope,
            row.name,
            row.value,
            row.sealed,
            row.updated
        ],
    )?;
    Ok(())
}

impl SqliteBackend {
    /// Open the file, creating it and bringing its schema up to date.
    pub fn open(path: &Path) -> Result<SqliteBackend> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(io)?;
        }
        private_file(path)?;
        let mut db = Connection::open(path)?;
        // Another process may be writing: wait for it rather than fail.
        db.busy_timeout(std::time::Duration::from_secs(5))?;
        // Changing the journal mode does not wait for a busy file as other
        // statements do, which matters when two berths make a new file at once.
        let mut tries = 0;
        while let Err(error) = db.pragma_update(None, "journal_mode", "WAL") {
            let busy = matches!(
                error.sqlite_error_code(),
                Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
            );
            tries += 1;
            if !busy || tries > 250 {
                return Err(error.into());
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        // The version is read inside a transaction that writes from its
        // start, so two berths opening a new file at once migrate it once:
        // the second waits and then finds it done.
        let transaction = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let version: i64 = transaction.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version as usize > MIGRATIONS.len() {
            return Err(Error::Database(format!(
                "{} was written by a newer berth (schema {version})",
                path.display()
            )));
        }
        for (index, step) in MIGRATIONS.iter().enumerate().skip(version as usize) {
            transaction.execute_batch(step)?;
            transaction.pragma_update(None, "user_version", index as i64 + 1)?;
        }
        transaction.commit()?;
        Ok(SqliteBackend { db })
    }

    fn current_revision(db: &Connection, namespace: &str, key: &str) -> Result<Option<i64>> {
        Ok(db
            .query_row(
                "SELECT revision FROM kv WHERE namespace = ?1 AND key = ?2",
                [namespace, key],
                |row| row.get(0),
            )
            .optional()?)
    }
}

impl Backend for SqliteBackend {
    fn name(&self) -> &'static str {
        "sqlite"
    }

    fn schema_version(&mut self) -> Result<i64> {
        Ok(self
            .db
            .query_row("PRAGMA user_version", [], |row| row.get(0))?)
    }

    fn cursor(&mut self, host: &str) -> Result<u64> {
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

    fn take_in(&mut self, host: &str, message: &Message) -> Result<Option<Stored>> {
        let transaction = self.db.transaction()?;
        let mut taken = None;
        if message.keep != berth_api::Keep::None {
            if message.keep == berth_api::Keep::Last {
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

    fn reset_cursor(&mut self, host: &str) -> Result<()> {
        self.db
            .execute("DELETE FROM cursors WHERE host = ?1", [host])?;
        Ok(())
    }

    fn inbox(&mut self, filter: &Filter<'_>, limit: usize) -> Result<Vec<Stored>> {
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
            params![filter.host, topic, filter.before, filter.done, limit as i64],
            stored,
        )?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    fn set_done(&mut self, host: &str, id: u64, done: bool) -> Result<bool> {
        let changed = self.db.execute(
            "UPDATE messages SET done = ?3 WHERE host = ?1 AND id = ?2",
            params![host, id as i64, done],
        )?;
        Ok(changed == 1)
    }

    fn get(&mut self, namespace: &str, key: &str) -> Result<Option<Entry>> {
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

    fn put(
        &mut self,
        namespace: &str,
        key: &str,
        text: &str,
        when: When,
        updated: &str,
    ) -> Result<i64> {
        let transaction = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let current = Self::current_revision(&transaction, namespace, key)?;
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
            params![namespace, key, text, revision, updated],
        )?;
        transaction.commit()?;
        Ok(revision)
    }

    fn remove(&mut self, namespace: &str, key: &str, when: When) -> Result<bool> {
        let transaction = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let current = Self::current_revision(&transaction, namespace, key)?;
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

    fn list(&mut self, namespace: &str) -> Result<Vec<Entry>> {
        let mut statement = self.db.prepare(
            "SELECT key, value, revision, updated FROM kv WHERE namespace = ?1 ORDER BY key",
        )?;
        let rows = statement.query_map([namespace], entry)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    fn add_job(&mut self, job: &Job) -> Result<()> {
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

    fn end_job(&mut self, id: &str, exit_code: Option<i32>, ended: &str) -> Result<()> {
        self.db.execute(
            "UPDATE jobs SET ended = ?2, exit_code = ?3 WHERE id = ?1 AND ended IS NULL",
            params![id, ended, exit_code],
        )?;
        Ok(())
    }

    fn end_lost_jobs(&mut self, ended: &str) -> Result<usize> {
        Ok(self
            .db
            .execute("UPDATE jobs SET ended = ?1 WHERE ended IS NULL", [ended])?)
    }

    fn job(&mut self, id: &str) -> Result<Option<Job>> {
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

    fn jobs(&mut self, limit: usize) -> Result<Vec<Job>> {
        let mut statement = self.db.prepare(
            "SELECT id, host, what, started, ended, exit_code, output_path FROM jobs
             ORDER BY started DESC, id DESC LIMIT ?1",
        )?;
        let rows = statement.query_map([limit as i64], job)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    fn prune(&mut self, cutoff: &str) -> Result<Vec<String>> {
        let transaction = self.db.transaction()?;
        transaction.execute("DELETE FROM messages WHERE time < ?1", [cutoff])?;
        let files = {
            let mut statement = transaction
                .prepare("SELECT output_path FROM jobs WHERE ended IS NOT NULL AND started < ?1")?;
            let rows = statement.query_map([cutoff], |row| row.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        transaction.execute(
            "DELETE FROM jobs WHERE ended IS NOT NULL AND started < ?1",
            [cutoff],
        )?;
        transaction.commit()?;
        Ok(files)
    }

    fn state(&mut self, kind: &str, scope: &str, name: &str) -> Result<Option<StateRow>> {
        Ok(self
            .db
            .query_row(
                &format!(
                    "SELECT {STATE_COLUMNS} FROM state WHERE kind = ?1 AND scope = ?2 AND name = ?3"
                ),
                [kind, scope, name],
                state_row,
            )
            .optional()?)
    }

    fn states(&mut self, kind: &str, scope: &str) -> Result<Vec<StateRow>> {
        let mut statement = self.db.prepare(&format!(
            "SELECT {STATE_COLUMNS} FROM state WHERE kind = ?1 AND scope = ?2 ORDER BY name"
        ))?;
        let rows = statement.query_map([kind, scope], state_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    fn put_state(&mut self, row: &StateRow) -> Result<()> {
        Ok(put_state(&self.db, row)?)
    }

    fn remove_state(&mut self, kind: &str, scope: &str, name: &str) -> Result<bool> {
        let removed = self.db.execute(
            "DELETE FROM state WHERE kind = ?1 AND scope = ?2 AND name = ?3",
            [kind, scope, name],
        )?;
        Ok(removed == 1)
    }

    fn replace_states(&mut self, scope: &str, kinds: &[&str], rows: &[StateRow]) -> Result<()> {
        let transaction = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        for kind in kinds {
            transaction.execute(
                "DELETE FROM state WHERE scope = ?1 AND kind = ?2",
                [scope, kind],
            )?;
        }
        for row in rows {
            put_state(&transaction, row)?;
        }
        transaction.commit()?;
        Ok(())
    }

    fn all_states(&mut self) -> Result<Vec<StateRow>> {
        let mut statement = self.db.prepare(&format!(
            "SELECT {STATE_COLUMNS} FROM state ORDER BY kind, scope, name"
        ))?;
        let rows = statement.query_map([], state_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    fn change_states(
        &mut self,
        writes: &[StateRow],
        removals: &[(String, String, String)],
    ) -> Result<()> {
        let transaction = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        for row in writes {
            put_state(&transaction, row)?;
        }
        for (kind, scope, name) in removals {
            transaction.execute(
                "DELETE FROM state WHERE kind = ?1 AND scope = ?2 AND name = ?3",
                [kind, scope, name],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    fn swap_states(&mut self, rows: &[(StateRow, StateRow)]) -> Result<bool> {
        // Immediate: no other writer gets in between the count and the updates.
        let transaction = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let count: i64 =
            transaction.query_row("SELECT COUNT(*) FROM state", [], |row| row.get(0))?;
        if count as usize != rows.len() {
            return Ok(false);
        }
        for (old, new) in rows {
            let changed = transaction.execute(
                "UPDATE state SET value = ?4, sealed = ?5
                 WHERE kind = ?1 AND scope = ?2 AND name = ?3 AND value = ?6 AND sealed = ?7",
                params![
                    old.kind, old.scope, old.name, new.value, new.sealed, old.value, old.sealed
                ],
            )?;
            if changed != 1 {
                return Ok(false);
            }
        }
        transaction.commit()?;
        Ok(true)
    }

    fn export(&mut self, table: Table) -> Result<Rows> {
        let db = &self.db;
        let all = |sql: &str| db.prepare(sql);
        Ok(match table {
            Table::State => {
                let mut statement = all(&format!(
                    "SELECT {STATE_COLUMNS} FROM state ORDER BY kind, scope, name"
                ))?;
                let rows = statement.query_map([], state_row)?;
                Rows::State(rows.collect::<rusqlite::Result<_>>()?)
            }
            Table::Messages => {
                let mut statement = all(&format!(
                    "SELECT {MESSAGE_COLUMNS} FROM messages ORDER BY seq"
                ))?;
                let rows = statement.query_map([], |row| {
                    Ok(MessageRow {
                        seq: row.get(0)?,
                        host: row.get(1)?,
                        id: row.get(2)?,
                        topic: row.get(3)?,
                        body: row.get(4)?,
                        keep: row.get(5)?,
                        from_level: row.get(6)?,
                        from_uid: row.get(7)?,
                        session: row.get(8)?,
                        time: row.get(9)?,
                        done: row.get(10)?,
                    })
                })?;
                Rows::Messages(rows.collect::<rusqlite::Result<_>>()?)
            }
            Table::Cursors => {
                let mut statement = all("SELECT host, last_id FROM cursors ORDER BY host")?;
                let rows = statement.query_map([], |row| {
                    Ok(CursorRow {
                        host: row.get(0)?,
                        last_id: row.get(1)?,
                    })
                })?;
                Rows::Cursors(rows.collect::<rusqlite::Result<_>>()?)
            }
            Table::Kv => {
                let mut statement = all(
                    "SELECT namespace, key, value, revision, updated FROM kv ORDER BY namespace, key",
                )?;
                let rows = statement.query_map([], |row| {
                    Ok(KvRow {
                        namespace: row.get(0)?,
                        key: row.get(1)?,
                        value: row.get(2)?,
                        revision: row.get(3)?,
                        updated: row.get(4)?,
                    })
                })?;
                Rows::Kv(rows.collect::<rusqlite::Result<_>>()?)
            }
            Table::Jobs => {
                let mut statement = all(
                    "SELECT id, host, what, started, ended, exit_code, output_path FROM jobs ORDER BY started, id",
                )?;
                let rows = statement.query_map([], job)?;
                Rows::Jobs(rows.collect::<rusqlite::Result<_>>()?)
            }
        })
    }

    fn import(&mut self, rows: &Rows) -> Result<usize> {
        let transaction = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let mut added = 0;
        match rows {
            Rows::State(rows) => {
                let mut statement = transaction.prepare(
                    "INSERT INTO state(kind, scope, name, value, sealed, updated)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6) ON CONFLICT DO NOTHING",
                )?;
                for row in rows {
                    added += statement.execute(params![
                        row.kind,
                        row.scope,
                        row.name,
                        row.value,
                        row.sealed,
                        row.updated
                    ])?;
                }
            }
            Rows::Messages(rows) => {
                let mut statement = transaction.prepare(&format!(
                    "INSERT INTO messages({MESSAGE_COLUMNS})
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11) ON CONFLICT DO NOTHING"
                ))?;
                for row in rows {
                    added += statement.execute(params![
                        row.seq,
                        row.host,
                        row.id,
                        row.topic,
                        row.body,
                        row.keep,
                        row.from_level,
                        row.from_uid,
                        row.session,
                        row.time,
                        row.done
                    ])?;
                }
            }
            Rows::Cursors(rows) => {
                let mut statement = transaction.prepare(
                    "INSERT INTO cursors(host, last_id) VALUES (?1, ?2) ON CONFLICT DO NOTHING",
                )?;
                for row in rows {
                    added += statement.execute(params![row.host, row.last_id])?;
                }
            }
            Rows::Kv(rows) => {
                let mut statement = transaction.prepare(
                    "INSERT INTO kv(namespace, key, value, revision, updated)
                     VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT DO NOTHING",
                )?;
                for row in rows {
                    added += statement.execute(params![
                        row.namespace,
                        row.key,
                        row.value,
                        row.revision,
                        row.updated
                    ])?;
                }
            }
            Rows::Jobs(rows) => {
                let mut statement = transaction.prepare(
                    "INSERT INTO jobs(id, host, what, started, ended, exit_code, output_path)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) ON CONFLICT DO NOTHING",
                )?;
                for job in rows {
                    added += statement.execute(params![
                        job.id,
                        job.host,
                        job.what,
                        job.started,
                        job.ended,
                        job.exit_code,
                        job.output_path
                    ])?;
                }
            }
        }
        transaction.commit()?;
        Ok(added)
    }

    fn count(&mut self, table: Table) -> Result<u64> {
        let count: i64 = self.db.query_row(
            &format!("SELECT COUNT(*) FROM {}", table.name()),
            [],
            |row| row.get(0),
        )?;
        Ok(count as u64)
    }
}
