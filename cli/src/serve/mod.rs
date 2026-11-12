//! `berth serve`: one API over all hosts, with the store. It reaches each
//! host through a link, as the UI does, and adds nothing to a host's own API.
//! The commands that only read the store (`inbox`, `kv`) are here too.

mod api;
mod auth;
mod docs;
#[cfg(feature = "web")]
mod page;
mod setup;
#[cfg(test)]
mod tests;

use crate::Result;
use berth_api::{Message, Version, client};
use berth_core::{
    host::Host,
    link::{Link, LinkSpec, runtime_dir},
    paths::Paths,
    store::{self, Filter, Store, Stored, When},
};
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex, RwLock},
    time::{Duration, Instant},
};
use tokio::sync::broadcast;

pub use docs::{asyncapi, openapi, operations};

/// How long an answer about a host's reachability is used again.
const SEEN_FOR: Duration = Duration::from_secs(10);

/// Something that happened, for whoever listens.
#[derive(Debug, Clone)]
pub enum Event {
    /// A message from a host's channel, kept or not.
    Channel { host: String, message: Message },
    /// A message that was stored.
    Inbox(Stored),
    /// A job started or ended.
    Job(Value),
    Kv {
        namespace: String,
        key: String,
        value: Option<Value>,
        revision: i64,
    },
}

/// A host as `berth serve` knows it.
pub struct Remote {
    pub name: String,
    pub address: String,
    pub link: Option<Link>,
    /// The SSH user's sudo password, for a host that asks for one.
    password: Option<String>,
    /// Why there is no link.
    pub problem: Option<String>,
    seen: Mutex<Option<(Instant, std::result::Result<Version, String>)>>,
}

impl Remote {
    pub fn new(name: &str, address: &str, spec: LinkSpec) -> Remote {
        Remote {
            name: name.to_owned(),
            address: address.to_owned(),
            link: Some(Link::new(spec)),
            password: None,
            problem: None,
            seen: Mutex::new(None),
        }
    }

    /// The host's version, asked for at most every few seconds.
    pub async fn version(&self) -> std::result::Result<Version, String> {
        let Some(link) = &self.link else {
            return Err(self.problem.clone().unwrap_or_else(|| "no link".into()));
        };
        if let Some((when, answer)) = &*self.seen.lock().unwrap()
            && when.elapsed() < SEEN_FOR
        {
            return answer.clone();
        }
        let asked =
            tokio::time::timeout(Duration::from_secs(15), link.get::<Version>("/v1/version")).await;
        let answer = match asked {
            Ok(Ok(version)) => Ok(version),
            Ok(Err(error)) if Link::missing(&error) => {
                Err(format!("no berthd on the host yet: {error}"))
            }
            Ok(Err(error)) => Err(error.to_string()),
            Err(_) => Err("no answer in time".into()),
        };
        *self.seen.lock().unwrap() = Some((Instant::now(), answer.clone()));
        answer
    }
}

pub struct Serve {
    pub paths: Paths,
    pub store: Mutex<Store>,
    pub hosts: RwLock<BTreeMap<String, Arc<Remote>>>,
    pub events: broadcast::Sender<Event>,
    /// Who is let in over TCP, and as what.
    pub auth: auth::Auth,
    pub jobs_dir: PathBuf,
    /// The tasks that take in the hosts' channels, by host.
    followers: Mutex<BTreeMap<String, tokio::task::AbortHandle>>,
    /// The sudo passwords given at the start, which outlive a changed profile.
    passwords: Mutex<BTreeMap<String, String>>,
}

impl Serve {
    /// With only the server's own token to get in by.
    #[cfg(test)]
    pub fn new(paths: Paths, store: Store, token: Option<String>) -> Arc<Serve> {
        Serve::with(paths, store, auth::Auth::token(token))
    }

    pub fn with(paths: Paths, store: Store, auth: auth::Auth) -> Arc<Serve> {
        let jobs_dir = paths.data_dir.join("jobs");
        Arc::new(Serve {
            paths,
            store: Mutex::new(store),
            hosts: RwLock::new(BTreeMap::new()),
            events: broadcast::channel(1024).0,
            auth,
            jobs_dir,
            followers: Mutex::new(BTreeMap::new()),
            passwords: Mutex::new(BTreeMap::new()),
        })
    }

    pub fn host(&self, name: &str) -> Option<Arc<Remote>> {
        self.hosts.read().unwrap().get(name).cloned()
    }

    /// Add a host and start taking in its channel.
    pub fn add(self: &Arc<Self>, remote: Remote) -> Arc<Remote> {
        let remote = Arc::new(remote);
        self.hosts
            .write()
            .unwrap()
            .insert(remote.name.clone(), remote.clone());
        if remote.link.is_some() {
            let follower = tokio::spawn(take_in(self.clone(), remote.clone()));
            let before = self
                .followers
                .lock()
                .unwrap()
                .insert(remote.name.clone(), follower.abort_handle());
            if let Some(before) = before {
                before.abort();
            }
        }
        remote
    }

    /// Drop what is known of a host, as after its profile changed or went.
    /// [`Serve::refresh`] takes it up again from its profile.
    pub fn forget(&self, name: &str) {
        self.hosts.write().unwrap().remove(name);
        if let Some(follower) = self.followers.lock().unwrap().remove(name) {
            follower.abort();
        }
    }

    /// Take up saved hosts that are not known yet. Only at the start may a
    /// password be asked for on the terminal.
    pub fn refresh(self: &Arc<Self>, start: bool) {
        for (name, _) in self.paths.hosts() {
            if self.host(&name).is_some() {
                continue;
            }
            let Ok(host) = Host::load(&self.paths, &name) else {
                continue;
            };
            // A link that is a command of its own goes through no sudo of ours.
            let asks = host.asks_password() && !host.config["builder"]["link"].is_array();
            let password = match (start, asks) {
                (_, false) => Ok(None),
                (true, true) => host.sudo_password().map_err(|error| error.to_string()),
                (false, true) => self
                    .passwords
                    .lock()
                    .unwrap()
                    .get(&host.name)
                    .cloned()
                    .or_else(|| std::env::var("BERTH_SUDO_PASSWORD").ok())
                    .map(Some)
                    .ok_or_else(|| {
                        format!(
                            "{} asks for a sudo password: start berth serve again",
                            host.name
                        )
                    }),
            };
            if let Ok(Some(password)) = &password {
                let mut passwords = self.passwords.lock().unwrap();
                passwords.insert(host.name.clone(), password.clone());
            }
            let reachable = !host.address().is_empty() || host.config["builder"]["link"].is_array();
            let link = password.clone().and_then(|password| match reachable {
                true => host
                    .link(password.as_deref())
                    .map_err(|error| error.to_string()),
                false => Err("its profile has no SSH address yet".into()),
            });
            let remote = match link {
                Ok(spec) => Remote::new(&host.name, host.address(), spec),
                Err(problem) => Remote {
                    name: host.name.clone(),
                    address: host.address().to_owned(),
                    link: None,
                    password: None,
                    problem: Some(problem),
                    seen: Mutex::new(None),
                },
            };
            self.add(Remote {
                password: password.ok().flatten(),
                ..remote
            });
        }
    }

    /// A message from a host: store what is kept and tell whoever listens.
    fn accept(&self, host: &str, message: Message) {
        let taken = self.store.lock().unwrap().take_in(host, &message);
        let _ = self.events.send(Event::Channel {
            host: host.to_owned(),
            message,
        });
        match taken {
            Ok(Some(stored)) => {
                let _ = self.events.send(Event::Inbox(stored));
            }
            Ok(None) => {}
            Err(error) => eprintln!("berth serve: storing a message of {host}: {error}"),
        }
    }

    /// Drop what is older than the retention time, with the output of the jobs dropped.
    fn prune(&self) {
        let days = self
            .paths
            .settings()
            .ok()
            .and_then(|settings| settings.get("retention_days")?.as_str()?.parse().ok())
            .unwrap_or(store::RETENTION_DAYS);
        if let Ok(files) = self.store.lock().unwrap().prune(days) {
            for file in files {
                let _ = std::fs::remove_file(file);
            }
        }
    }
}

/// Follow a host's channel from where it was read up to, for as long as we run.
async fn take_in(state: Arc<Serve>, remote: Arc<Remote>) {
    let Some(link) = &remote.link else {
        return;
    };
    let mut wait = 1;
    loop {
        let since = state
            .store
            .lock()
            .unwrap()
            .cursor(&remote.name)
            .unwrap_or(0);
        if let Ok(mut stream) = link.stream(&format!("/v1/channel?since={since}")).await {
            wait = 1;
            while let Some(Ok(frame)) = stream.next().await {
                if let client::Frame::Text(text) = frame
                    && let Ok(message) = serde_json::from_str::<Message>(text.as_str())
                {
                    state.accept(&remote.name, message);
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(wait)).await;
        wait = (wait * 2).min(60);
    }
}

fn copy(paths: &Paths) -> Paths {
    Paths {
        config_dir: paths.config_dir.clone(),
        data_dir: paths.data_dir.clone(),
    }
}

pub fn store_path(paths: &Paths) -> PathBuf {
    paths.data_dir.join("berth.db")
}

fn open_store(paths: &Paths) -> Result<Store> {
    Ok(Store::open(&store_path(paths))?)
}

/// The token clients over TCP have to show, made on first use.
fn token(paths: &Paths) -> Result<String> {
    use std::os::unix::fs::OpenOptionsExt;
    let path = paths.data_dir.join("serve.token");
    if let Ok(token) = std::fs::read_to_string(&path)
        && token.trim().len() >= 32
    {
        return Ok(token.trim().to_owned());
    }
    let mut bytes = [0u8; 32];
    std::io::Read::read_exact(&mut std::fs::File::open("/dev/urandom")?, &mut bytes)?;
    let token: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    std::fs::create_dir_all(&paths.data_dir)?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)?;
    std::io::Write::write_all(&mut file, format!("{token}\n").as_bytes())?;
    Ok(token)
}

/// Held while `berth serve` runs, so only one takes messages in.
fn lock(paths: &Paths) -> Result<std::fs::File> {
    use std::os::fd::AsRawFd;
    std::fs::create_dir_all(&paths.data_dir)?;
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(paths.data_dir.join("serve.lock"))?;
    // SAFETY: the descriptor is open; the lock goes when the file is closed.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err("berth serve is already running for this user".into());
    }
    Ok(file)
}

const USAGE: &str = "Usage: berth serve [--listen ADDRESS:PORT [--expose] [--web]]";

/// `berth serve [--listen ADDRESS:PORT [--expose] [--web]]`
///
/// Over TCP every request shows the token, but the connection is plain HTTP,
/// so an address other than this machine's own takes `--expose` as well: for a
/// container whose port is published, or behind a proxy that adds TLS. With
/// `--web` the address also gives the hosts as a page for a browser.
pub fn command(paths: &Paths, args: &[&str]) -> Result<()> {
    let (mut listen, mut expose, mut web) = (None, false, false);
    let mut words = args.iter();
    while let Some(word) = words.next() {
        match *word {
            "--listen" => listen = Some(*words.next().ok_or(USAGE)?),
            "--expose" => expose = true,
            "--web" => web = true,
            _ => return Err(USAGE.into()),
        }
    }
    if listen.is_none() && (expose || web) {
        return Err(USAGE.into());
    }
    if web && !cfg!(feature = "web") {
        return Err(
            "This berth is built without the page for a browser: the berth-web build has it".into(),
        );
    }
    let listen = match listen {
        None => None,
        Some(address) => {
            let address: std::net::SocketAddr = address
                .parse()
                .map_err(|_| format!("Not an address and port: {address}"))?;
            if !address.ip().is_loopback() && !expose {
                return Err(format!(
                    "{address} can be reached from other machines, over plain HTTP: add --expose to allow it, or use 127.0.0.1 or [::1]"
                )
                .into());
            }
            Some(address)
        }
    };
    let _lock = lock(paths)?;
    let mut store = open_store(paths)?;
    store.end_lost_jobs()?;
    let token = match listen {
        Some(_) => Some(token(paths)?),
        None => None,
    };
    let auth = auth::Auth::configured(paths, token)?;
    let admitted = auth.describe();
    let state = Serve::with(copy(paths), store, auth);
    std::fs::create_dir_all(&state.jobs_dir)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        state.refresh(true);
        state.prune();
        let daily = state.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(86400)).await;
                daily.prune();
            }
        });

        let socket = runtime_dir().join("serve.sock");
        let _ = std::fs::remove_file(&socket);
        let unix = tokio::net::UnixListener::bind(&socket)?;
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
        }
        println!("Listening on {}", socket.display());
        let local = api::router(state.clone(), false);
        let over_unix = tokio::spawn(async move { axum::serve(unix, local).await });
        let over_tcp = match listen {
            Some(address) => {
                let tcp = tokio::net::TcpListener::bind(address).await?;
                println!(
                    "Listening on http://{} for {admitted} (token in {})",
                    tcp.local_addr()?,
                    state.paths.data_dir.join("serve.token").display()
                );
                let guarded = api::router(state.clone(), true);
                #[cfg(feature = "web")]
                let guarded = match web {
                    true => {
                        println!("The hosts in a browser: http://{}/", tcp.local_addr()?);
                        guarded.merge(page::router())
                    }
                    false => guarded,
                };
                Some(tokio::spawn(async move { axum::serve(tcp, guarded).await }))
            }
            None => None,
        };
        let known = state.hosts.read().unwrap().len();
        println!("{known} host{}", if known == 1 { "" } else { "s" });
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = over_unix => {}
            _ = async { match over_tcp { Some(task) => { let _ = task.await; } None => std::future::pending().await } } => {}
        }
        let _ = std::fs::remove_file(&socket);
        Ok(())
    })
}

/// `berth inbox [HOST] [--topic PREFIX] [--all]`: what the store holds.
pub fn inbox_command(paths: &Paths, args: &[&str]) -> Result<()> {
    let (mut host, mut topic, mut all) = (None, None, false);
    let mut words = args.iter();
    while let Some(word) = words.next() {
        match *word {
            "--all" => all = true,
            "--topic" => topic = Some(*words.next().ok_or("--topic needs a prefix")?),
            word if word.starts_with('-') => {
                return Err("Usage: berth inbox [HOST] [--topic PREFIX] [--all]".into());
            }
            word => host = Some(word),
        }
    }
    let store = open_store(paths)?;
    let filter = Filter {
        host,
        topic,
        before: None,
        done: (!all).then_some(false),
        limit: 200,
    };
    let (messages, next) = store.inbox(&filter)?;
    for stored in messages.iter().rev() {
        let message = &stored.message;
        let mark = if stored.done { "✓" } else { " " };
        println!(
            "{mark} {}  {}#{}  {}  {}",
            message.time, stored.host, message.id, message.topic, message.body
        );
    }
    if next.is_some() {
        eprintln!("(older messages not shown)");
    }
    Ok(())
}

/// `berth kv get|set|rm|ls NAMESPACE [KEY] [JSON]`
pub fn kv_command(paths: &Paths, args: &[&str]) -> Result<()> {
    let mut store = open_store(paths)?;
    match *args {
        ["ls", namespace] => {
            for entry in store.list(namespace)? {
                println!("{}\t{}\t{}", entry.key, entry.revision, entry.value);
            }
        }
        ["get", namespace, key] => match store.get(namespace, key)? {
            Some(entry) => println!("{}", entry.value),
            None => return Err(format!("No {key} in {namespace}").into()),
        },
        ["set", namespace, key, value] => {
            // Anything that is not JSON is taken as text.
            let value = serde_json::from_str(value).unwrap_or_else(|_| json!(value));
            let revision = store.put(namespace, key, &value, When::Always)?;
            println!("{key} is at revision {revision}");
        }
        ["rm", namespace, key] => {
            if !store.remove(namespace, key, When::Always)? {
                return Err(format!("No {key} in {namespace}").into());
            }
        }
        _ => return Err("Usage: berth kv get|set|rm|ls NAMESPACE [KEY] [JSON]".into()),
    }
    Ok(())
}
