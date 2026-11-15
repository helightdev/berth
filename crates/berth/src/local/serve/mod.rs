//! `berth serve`: one API over all hosts, with the store. It reaches each
//! host through a link, as the UI does, and adds nothing to a host's own API.

mod actions;
mod api;
mod auth;
mod connect;
mod docs;
pub mod here;
pub(crate) mod keys;
mod provisioners;
mod resources;
mod setup;
pub mod setup_key;
#[cfg(test)]
mod tests;

use crate::Result;
use berth_api::{Message, Version, client};
use berth_core::{
    host::Host,
    link::{Link, LinkSpec},
    paths::Paths,
    store::{self, Store, Stored},
};
use futures_util::StreamExt;
use serde_json::Value;
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex, RwLock},
    time::{Duration, Instant},
};
use tokio::sync::broadcast;

pub use docs::{asyncapi, openapi, operations};

/// How a host's answer starts when berth logged in and found no berthd.
pub(crate) const NO_BERTHD: &str = "no berthd on the host yet";

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
    /// A saved host was added, removed or renamed, or its profile changed.
    Host {
        name: String,
        change: &'static str,
        /// Its name before, when it was renamed.
        from: Option<String>,
    },
    /// A resource was created, removed, or its record changed.
    Resource {
        id: String,
        provisioner: String,
        kind: String,
        change: &'static str,
    },
}

/// What the store held of the hosts and resources when last looked at, so a
/// change to it can be told as an event whoever made it.
struct Watch {
    paths: Paths,
    events: broadcast::Sender<Event>,
    seen: Mutex<Seen>,
}

/// Unknown until the store could first be read, which tells nothing.
#[derive(Default)]
struct Seen {
    hosts: Option<BTreeMap<String, Value>>,
    resources: Option<BTreeMap<String, berth_core::resource::Record>>,
}

/// How each key of `now` differs from `before`: added, removed or changed.
fn differences<'a, T: PartialEq>(
    before: &'a BTreeMap<String, T>,
    now: &'a BTreeMap<String, T>,
) -> Vec<(&'a str, &'static str)> {
    let gone = before
        .keys()
        .filter(|name| !now.contains_key(*name))
        .map(|name| (name.as_str(), "removed"));
    let came = now
        .iter()
        .filter_map(|(name, value)| match before.get(name) {
            None => Some((name.as_str(), "added")),
            Some(was) if was != value => Some((name.as_str(), "changed")),
            Some(_) => None,
        });
    gone.chain(came).collect()
}

impl Watch {
    fn hosts(&self) -> Option<BTreeMap<String, Value>> {
        berth_core::profiles::list(&self.paths, store::OWN)
            .ok()
            .map(|saved| saved.into_iter().collect())
    }

    fn resources(&self) -> Option<BTreeMap<String, berth_core::resource::Record>> {
        let kept = berth_core::resource::list(&self.paths).ok()?;
        Some(
            kept.into_iter()
                .map(|kept| {
                    // When it was last looked at is no change of it.
                    let record = berth_core::resource::Record {
                        checked: String::new(),
                        ..kept.record
                    };
                    (record.id.clone(), record)
                })
                .collect(),
        )
    }

    /// Tell what changed in the store since it was last looked at.
    fn notice(&self) {
        let (hosts, resources) = (self.hosts(), self.resources());
        // Held while telling, so two callers cannot tell a change twice or out of order.
        let mut seen = self.seen.lock().unwrap();
        if let Some(now) = hosts {
            if let Some(before) = &seen.hosts {
                for (name, change) in differences(before, &now) {
                    let _ = self.events.send(Event::Host {
                        name: name.to_owned(),
                        change,
                        from: None,
                    });
                }
            }
            seen.hosts = Some(now);
        }
        if let Some(now) = resources {
            if let Some(before) = &seen.resources {
                for (id, change) in differences(before, &now) {
                    let record = now.get(id).or_else(|| before.get(id));
                    let change = match change {
                        "added" => "created",
                        other => other,
                    };
                    let _ = self.events.send(Event::Resource {
                        id: id.to_owned(),
                        provisioner: record.map(|r| r.provisioner.clone()).unwrap_or_default(),
                        kind: record.map(|r| r.kind.clone()).unwrap_or_default(),
                        change,
                    });
                }
            }
            seen.resources = Some(now);
        }
    }

    /// Tell a rename as one, and not as a host removed and another added.
    fn renamed(&self, old: &str, new: &str) {
        let mut seen = self.seen.lock().unwrap();
        if let Some(hosts) = seen.hosts.as_mut()
            && let Some(profile) = hosts.remove(old)
        {
            hosts.insert(new.to_owned(), profile);
        }
        let _ = self.events.send(Event::Host {
            name: new.to_owned(),
            change: "renamed",
            from: Some(old.to_owned()),
        });
    }
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
    /// The saved profile it was taken up from, by which a change to the store
    /// from another process is noticed.
    profile: Option<Value>,
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
            profile: None,
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
        // A host that refused the login a short while ago is not asked by
        // the list: each refused login counts against the server's address
        // at the host's sshd.
        if let Some(refused) = link.refused() {
            return Err(refused);
        }
        let asked = tokio::time::timeout(Duration::from_secs(15), link.version()).await;
        let answer = match asked {
            Ok(Ok(version)) => Ok(version),
            Ok(Err(error)) if Link::missing(&error) => Err(format!("{NO_BERTHD}: {error}")),
            Ok(Err(error)) => Err(error.to_string()),
            Err(_) => Err("no answer in time".into()),
        };
        *self.seen.lock().unwrap() = Some((Instant::now(), answer.clone()));
        answer
    }
}

pub struct Serve {
    pub paths: Paths,
    pub store: Arc<Store>,
    pub hosts: RwLock<BTreeMap<String, Arc<Remote>>>,
    pub events: broadcast::Sender<Event>,
    /// Who is let in over TCP, and as what.
    pub auth: auth::Auth,
    pub jobs_dir: PathBuf,
    /// What runs the jobs, with their records in the store.
    pub runner: berth_core::runner::Runner,
    /// The tasks that take in the hosts' channels, by host.
    followers: Mutex<BTreeMap<String, tokio::task::AbortHandle>>,
    /// The sudo passwords given at the start, which outlive a changed profile.
    passwords: Mutex<BTreeMap<String, String>>,
    /// How long provisioners may take and how a new machine is waited for,
    /// which tests make quick.
    pub provision: fn() -> berth_core::provision::Options,
    /// Whether the hosts' channels are taken in, which only `berth serve`
    /// does, holding its lock.
    pub follows: bool,
    watch: Arc<Watch>,
}

impl Serve {
    /// With only the server's own token to get in by.
    #[cfg(test)]
    pub fn new(paths: Paths, store: Store, token: Option<String>) -> Arc<Serve> {
        Serve::with(paths, Arc::new(store), auth::Auth::token(token))
    }

    pub fn with(paths: Paths, store: Arc<Store>, auth: auth::Auth) -> Arc<Serve> {
        let jobs_dir = berth_core::runner::jobs_dir(&paths);
        let events = broadcast::channel(1024).0;
        let watch = Arc::new(Watch {
            paths: paths.clone(),
            events: events.clone(),
            seen: Mutex::new(Seen::default()),
        });
        watch.notice();
        let (told, watching) = (events.clone(), watch.clone());
        let notify = Arc::new(move |job: &store::Job| {
            let _ = told.send(Event::Job(job.json()));
            // A job is what changes hosts and resources, besides the routes.
            watching.notice();
        });
        let runner =
            berth_core::runner::Runner::new(paths.clone(), store.clone(), jobs_dir.clone(), notify);
        Arc::new(Serve {
            paths,
            store,
            hosts: RwLock::new(BTreeMap::new()),
            events,
            auth,
            jobs_dir,
            runner,
            followers: Mutex::new(BTreeMap::new()),
            passwords: Mutex::new(BTreeMap::new()),
            provision: Default::default,
            follows: true,
            watch,
        })
    }

    /// Tell whoever follows changes what changed among the saved hosts and
    /// the resources since this was last asked.
    pub fn notice(&self) {
        self.watch.notice();
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
        if remote.link.is_some() && self.follows {
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

    /// Drop what is known of a host that was given another name, and keep
    /// the sudo password it was given at the start for its new one. The next
    /// [`Serve::refresh`] takes it up under that name.
    pub fn renamed(&self, old: &str, new: &str) {
        self.forget(old);
        self.watch.renamed(old, new);
        let mut passwords = self.passwords.lock().unwrap();
        if let Some(password) = passwords.remove(old) {
            passwords.insert(new.to_owned(), password);
        }
    }

    /// Take up saved hosts that are not known yet: the server's own, whatever
    /// its settings say. A host whose profile went or changed meanwhile, as by
    /// a berth command beside the server, is dropped or taken up again. Only at
    /// the start may a password be asked for on the terminal.
    pub fn refresh(self: &Arc<Self>, start: bool) {
        // A store that cannot be read says nothing of what was removed.
        let Ok(saved) = berth_core::profiles::list(&self.paths, store::OWN) else {
            return;
        };
        self.notice();
        let stale: Vec<String> = {
            let hosts = self.hosts.read().unwrap();
            hosts
                .values()
                .filter(|remote| {
                    remote.profile.as_ref().is_some_and(|taken| {
                        !saved
                            .iter()
                            .any(|(name, config)| *name == remote.name && config == taken)
                    })
                })
                .map(|remote| remote.name.clone())
                .collect()
        };
        for name in stale {
            self.forget(&name);
        }
        for (name, config) in saved {
            if self.host(&name).is_some() {
                continue;
            }
            let Ok(place) = berth_core::profiles::Place::own(&name) else {
                continue;
            };
            let host = Host {
                name,
                place,
                config,
            };
            // A link that is a command of its own goes through no sudo of ours.
            let asks = host.asks_password() && host.access().link.is_none();
            let password = match (start, asks) {
                (_, false) => Ok(None),
                (true, true) => host
                    .sudo_password(&self.paths)
                    .map_err(|error| error.to_string()),
                (false, true) => self
                    .passwords
                    .lock()
                    .unwrap()
                    .get(&host.name)
                    .cloned()
                    .or_else(|| host.kept_sudo_password(&self.paths))
                    .map(Some)
                    .ok_or_else(|| {
                        format!(
                            "{name}'s SSH user's sudo asks for a password, which this server was not given. Keep it with berth secret set sudo/{name}, or run berth serve on a terminal, which asks for it",
                            name = host.name
                        )
                    }),
            };
            if let Ok(Some(password)) = &password {
                let mut passwords = self.passwords.lock().unwrap();
                passwords.insert(host.name.clone(), password.clone());
            }
            let reachable = host.access().reachable();
            let link = password.clone().and_then(|password| match reachable {
                true => host
                    .link(&self.paths, password.as_deref())
                    .map_err(|error| error.to_string()),
                false => Err("its profile has no SSH address yet".into()),
            });
            let remote = match link {
                Ok(spec) => Remote::new(&host.name, &host.address(), spec),
                Err(problem) => Remote {
                    name: host.name.clone(),
                    address: host.address(),
                    link: None,
                    password: None,
                    problem: Some(problem),
                    profile: None,
                    seen: Mutex::new(None),
                },
            };
            self.add(Remote {
                password: password.ok().flatten(),
                profile: Some(host.config.clone()),
                ..remote
            });
        }
    }

    /// A message from a host: store what is kept and tell whoever listens.
    fn accept(&self, host: &str, message: Message) {
        let taken = self.store.take_in(host, &message);
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
            .setting("retention_days")
            .and_then(|days| days.parse().ok())
            .unwrap_or(store::RETENTION_DAYS);
        if let Ok(files) = self.store.prune(days) {
            // Only files of this machine's own jobs: a row of a shared store
            // can name any file.
            for file in files
                .iter()
                .filter_map(|file| berth_core::runner::output_file(&self.paths, file))
            {
                // A roll-out's report is beside its output.
                let _ = std::fs::remove_file(api::report_path(&file));
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
        // A refused login is not tried again by itself until its back-off
        // has passed or someone logged in with it; the store says which.
        if link.refused().is_some() {
            tokio::time::sleep(Duration::from_secs(60)).await;
            continue;
        }
        let since = state.store.cursor(&remote.name).unwrap_or(0);
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

fn open_store(paths: &Paths) -> Result<Arc<Store>> {
    store::shared(paths)
}

/// The token clients over TCP have to show, made on first use. The store
/// holds it, so a server restored from its database keeps its clients;
/// `serve.token` is written from it for whoever reads the file.
pub(crate) fn token(paths: &Paths) -> Result<String> {
    let store = open_store(paths)?;
    let values = berth_core::kept::serve_token(paths).settle(
        paths,
        &store,
        &berth_core::kept::generate_token,
    )?;
    let token = String::from_utf8_lossy(&values[0]).trim().to_owned();
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

const USAGE: &str = "Usage: berth serve [--listen ADDRESS:PORT [--expose]]";

/// `berth serve [--listen ADDRESS:PORT [--expose]]`
///
/// Over TCP every request shows the token, but the connection is plain HTTP,
/// so an address other than this machine's own takes `--expose` as well: for a
/// container whose port is published, or behind a proxy that adds TLS.
pub fn command(paths: &Paths, args: &[&str]) -> Result<()> {
    let (mut listen, mut expose) = (None, false);
    let mut words = args.iter();
    while let Some(word) = words.next() {
        match *word {
            "--listen" => listen = Some(*words.next().ok_or(USAGE)?),
            "--expose" => expose = true,
            _ => return Err(USAGE.into()),
        }
    }
    if listen.is_none() && expose {
        return Err(USAGE.into());
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
    let store = open_store(paths)?;
    store.end_lost_jobs()?;
    // From the next step on the store holds keys and passwords.
    if store.backend_name() == "postgres" && !store.encrypted() {
        eprintln!(
            "berth serve: the PostgreSQL database holds this installation's keys and passwords readable; berth server store encrypt seals them"
        );
    }
    crate::local::restore_plugins(paths);
    let token = match listen {
        Some(_) => Some(token(paths)?),
        None => None,
    };
    let auth = auth::Auth::configured(paths, token)?;
    let admitted = auth.describe();
    let state = Serve::with(paths.clone(), store, auth);
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
        // Another berth beside the server changes the store too.
        let watching = state.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                if watching.events.receiver_count() > 0 {
                    watching.notice();
                }
            }
        });

        let socket = here::socket();
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
