//! What the server keeps for setting it up, as the page last heard it, and
//! the calls that change it. Answers arrive between frames; the screens read
//! what is here.

use crate::portal::{Frame, Portal};
use berth_api::{PluginInfo, PluginList};
use berth_tui::fields::Field;
use berth_tui::host::View;
use berth_tui::setup::Feature;
use serde_json::{Map, Value, json};
use std::cell::RefCell;
use std::rc::Rc;

/// Something that came back for the host being set up.
pub enum Arrived {
    /// Its profile, or why there is none.
    Profile(Result<Value, String>),
    /// The server keeps the profile as it is here now.
    Saved(Value),
    /// A value the server made up for a field, and the password behind it
    /// when it made that up too.
    Generated(Field, String, Option<String>),
    /// The seed that installs it on a new machine.
    Seed(String),
    /// Applying it needs the SSH user's sudo password.
    Password,
    /// A session on the host to look at.
    Session(Value),
}

#[derive(Default)]
pub struct Data {
    /// `view`, `operate` or `admin`; empty until the server has said.
    pub role: String,
    /// Whether the server lets an admin set it up from here.
    pub setup: bool,
    /// The profile a new host starts from.
    pub template: Value,
    pub features: Vec<Feature>,
    /// The server's own SSH public key; empty when it has none to give.
    pub own_key: String,
    pub registries: Map<String, Value>,
    /// Every secret's name; a registry's login is `registry/NAME`.
    pub secrets: Vec<String>,
    pub settings: Vec<Value>,
    pub plugins: Vec<PluginInfo>,
    /// Where the server keeps its own plugins.
    pub plugin_dir: String,
    /// Something to say on the status line.
    pub said: Option<String>,
    /// What the last check of a registry, or change of the plugins, said.
    pub report: Vec<String>,
    pub arrived: Vec<Arrived>,
    /// A job of the server, in a panel.
    pub job: Option<View>,
    /// What the last check of the plugins found.
    pub updates: Vec<berth_api::PluginUpdate>,
    /// The hosts the last update of the plugins left behind.
    pub behind: Vec<berth_api::HostBehind>,
    /// A roll-out runs, and the part of a line of its output not ended yet.
    pub rolling: Option<Vec<u8>>,
    /// The hosts are different now.
    pub hosts_changed: bool,
}

impl Data {
    pub fn admin(&self) -> bool {
        self.role == "admin" && self.setup
    }

    /// The secrets that are no registry's login.
    pub fn secret_names(&self) -> Vec<String> {
        self.secrets
            .iter()
            .filter(|name| !name.starts_with("registry/"))
            .cloned()
            .collect()
    }

    pub fn registry_names(&self) -> Vec<String> {
        self.registries.keys().cloned().collect()
    }

    pub fn setting(&self, key: &str) -> &str {
        self.settings
            .iter()
            .find(|setting| setting["key"] == key)
            .and_then(|setting| setting["value"].as_str())
            .unwrap_or("")
    }
}

#[derive(Clone)]
pub struct Admin {
    portal: Portal,
    pub data: Rc<RefCell<Data>>,
}

impl Admin {
    pub fn new(portal: Portal, data: Rc<RefCell<Data>>) -> Admin {
        Admin { portal, data }
    }

    /// Call the server; what goes wrong is said on the status line.
    fn call(&self, op: &str, args: Value, body: Value, then: impl FnOnce(&Admin, Value) + 'static) {
        let admin = self.clone();
        self.portal
            .call(op, None, args, body, move |result| match result {
                Ok(value) => then(&admin, value),
                Err(error) => admin.say(format!("✗ {error}")),
            });
    }

    pub fn say(&self, text: impl Into<String>) {
        self.data.borrow_mut().said = Some(text.into());
    }

    /// Ask who the caller is here, and with the role for it what there is to set up.
    pub fn load(&self) {
        self.call("access", json!({}), Value::Null, |admin, access| {
            {
                let mut data = admin.data.borrow_mut();
                data.role = access["role"].as_str().unwrap_or("view").to_owned();
                data.setup = access["setup"].as_bool().unwrap_or(false);
            }
            if admin.data.borrow().admin() {
                admin.catalogue();
                admin.refresh();
            }
        });
    }

    /// Ask what a new host starts from and what the plugins can install.
    fn catalogue(&self) {
        self.call("setup.catalogue", json!({}), Value::Null, |admin, found| {
            let mut data = admin.data.borrow_mut();
            data.template = found["template"].clone();
            data.own_key = found["key"].as_str().unwrap_or("").to_owned();
            data.features = found["features"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|feature| {
                    let text = |key: &str| feature[key].as_str().unwrap_or("").to_owned();
                    Feature {
                        key: text("key"),
                        label: text("label"),
                        group: text("group"),
                        help: text("help"),
                    }
                })
                .collect();
        });
    }

    /// Read the registries, the secrets' names, the settings and the plugins again.
    pub fn refresh(&self) {
        self.call("registries.list", json!({}), Value::Null, |admin, found| {
            let mut fresh = found["registries"].as_object().cloned().unwrap_or_default();
            let mut data = admin.data.borrow_mut();
            // One being added here has no address yet, and the server none of it.
            for (name, registry) in &data.registries {
                if registry["url"].as_str().unwrap_or("").is_empty() && !fresh.contains_key(name) {
                    fresh.insert(name.clone(), registry.clone());
                }
            }
            data.registries = fresh;
        });
        self.call("secrets.list", json!({}), Value::Null, |admin, found| {
            admin.data.borrow_mut().secrets = found["names"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|name| name.as_str().map(str::to_owned))
                .collect();
        });
        self.call("settings.list", json!({}), Value::Null, |admin, found| {
            admin.data.borrow_mut().settings = found.as_array().cloned().unwrap_or_default();
        });
        self.call("plugins.list", json!({}), Value::Null, |admin, found| {
            let listing: PluginList = serde_json::from_value(found).unwrap_or_default();
            let mut data = admin.data.borrow_mut();
            data.plugins = listing.plugins;
            data.plugin_dir = listing.dir;
        });
    }

    /// Change the server's plugins. What npm said is shown, and what a host
    /// can have installed is asked for again.
    fn change_plugins(&self, op: &str, args: Value, body: Value) {
        let admin = self.clone();
        self.portal.call(op, None, args, body, move |result| {
            let lines: Vec<String> = match result {
                Ok(report) => report["lines"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|line| line.as_str().map(str::to_owned))
                    .collect(),
                Err(error) => error.lines().map(str::to_owned).collect(),
            };
            {
                let mut data = admin.data.borrow_mut();
                data.said = Some(berth_tui::plugins::status(&lines));
                data.report = lines;
            }
            admin.catalogue();
            admin.refresh();
        });
    }

    pub fn add_plugin(&self, spec: &str) {
        self.say(format!("Installing {spec} on the server…"));
        self.change_plugins("plugins.add", json!({}), json!({"spec": spec}));
    }

    /// Send a .tgz for the server to install.
    pub fn upload_plugin(&self, name: &str, package: &[u8]) {
        if package.len() > berth_api::ops::PORTAL_BYTES {
            return self.say(format!(
                "✗ {name} is larger than the 8 MiB a server is sent"
            ));
        }
        self.say(format!("Sending {name} to the server…"));
        let body = json!({"base64": berth_api::b64(package)});
        self.change_plugins("plugins.upload", json!({}), body);
    }

    pub fn remove_plugin(&self, id: &str) {
        self.change_plugins("plugins.remove", json!({"id": id}), Value::Null);
    }

    /// Ask the server which of its plugins have a newer version.
    pub fn check_plugins(&self) {
        self.say("Asking the registries…".to_owned());
        let admin = self.clone();
        self.portal.call(
            "plugins.check",
            None,
            json!({}),
            Value::Null,
            move |result| {
                let mut data = admin.data.borrow_mut();
                match result.and_then(|found| {
                    serde_json::from_value::<berth_api::PluginUpdates>(found)
                        .map_err(|error| error.to_string())
                }) {
                    Ok(checked) => {
                        let mut tab = berth_tui::plugins::Tab::default();
                        data.report = tab.checked(checked.plugins);
                        data.updates = tab.updates;
                    }
                    Err(error) => data.report = error.lines().map(str::to_owned).collect(),
                }
                data.said = Some(berth_tui::plugins::status(&data.report));
            },
        );
    }

    /// Update one of the server's plugins, or all, and say which hosts are behind.
    pub fn update_plugins(&self, id: Option<&str>) {
        self.say("Updating on the server…".to_owned());
        let admin = self.clone();
        self.portal.call(
            "plugins.update",
            None,
            json!({}),
            json!({"id": id}),
            move |result| {
                {
                    let mut data = admin.data.borrow_mut();
                    match result.and_then(|found| {
                        serde_json::from_value::<berth_api::PluginReport>(found)
                            .map_err(|error| error.to_string())
                    }) {
                        Ok(report) => {
                            let mut tab = berth_tui::plugins::Tab::default();
                            data.report = tab.updated(report);
                            data.updates.clear();
                            data.behind = tab.behind;
                        }
                        Err(error) => data.report = error.lines().map(str::to_owned).collect(),
                    }
                    data.said = Some(berth_tui::plugins::status(&data.report));
                }
                admin.catalogue();
                admin.refresh();
            },
        );
    }

    /// Roll out to the hosts that are behind as a job of the server, its
    /// own lines shown on the tab as they come.
    pub fn roll_out(&self, options: &berth_api::Rollout) {
        if self.data.borrow().rolling.is_some() {
            return self.say("A roll-out runs already".to_owned());
        }
        {
            let mut data = self.data.borrow_mut();
            data.rolling = Some(vec![]);
            data.behind.clear();
            data.report = vec!["Rolling out…".into()];
        }
        let admin = self.clone();
        let body = serde_json::to_value(options).unwrap_or_default();
        self.portal
            .call("hosts.rollout", None, json!({}), body, move |result| {
                let job = match result {
                    Ok(job) => job,
                    Err(error) => {
                        let mut data = admin.data.borrow_mut();
                        data.rolling = None;
                        data.report = vec![format!("✗ {error}")];
                        return;
                    }
                };
                let id = job["id"].clone();
                let follower = admin.clone();
                admin
                    .portal
                    .stream("jobs.follow", None, json!({"id": id}), move |frame| {
                        let mut data = follower.data.borrow_mut();
                        match frame {
                            Frame::Bytes(bytes) => {
                                let Some(pending) = &mut data.rolling else {
                                    return;
                                };
                                pending.extend_from_slice(&bytes);
                                let mut lines = vec![];
                                while let Some(end) = pending.iter().position(|byte| *byte == b'\n')
                                {
                                    let line: Vec<u8> = pending.drain(..=end).collect();
                                    let line = String::from_utf8_lossy(&line).trim_end().to_owned();
                                    if berth_tui::plugins::rollout_line(&line) {
                                        lines.push(line);
                                    }
                                }
                                if let Some(last) = lines.last() {
                                    data.said = Some(last.clone());
                                }
                                data.report.extend(lines);
                            }
                            Frame::Data(_) => {}
                            Frame::End(_) => {
                                data.rolling = None;
                                data.hosts_changed = true;
                                drop(data);
                                let ended = follower.clone();
                                follower.portal.call(
                                    "jobs.get",
                                    None,
                                    json!({"id": id}),
                                    Value::Null,
                                    move |result| {
                                        let report = result
                                            .ok()
                                            .and_then(|job| {
                                                serde_json::from_value(job["report"].clone()).ok()
                                            })
                                            .unwrap_or_default();
                                        let mut data = ended.data.borrow_mut();
                                        data.report.extend(berth_tui::plugins::summary(&report));
                                    },
                                );
                            }
                        }
                    });
            });
    }

    /// Change something the server keeps, say so, and read it all again.
    fn change(&self, op: &str, args: Value, body: Value, said: String) {
        self.call(op, args, body, move |admin, _| {
            if !said.is_empty() {
                admin.say(said);
            }
            admin.refresh();
        });
    }

    pub fn save_registry(&self, name: &str, registry: &Value, said: &str) {
        // What is shown changes at once; the server's answer follows.
        self.data
            .borrow_mut()
            .registries
            .insert(name.to_owned(), registry.clone());
        self.change(
            "registries.save",
            json!({"name": name}),
            registry.clone(),
            said.to_owned(),
        );
    }

    pub fn remove_registry(&self, name: &str, said: String) {
        self.data.borrow_mut().registries.remove(name);
        self.change(
            "registries.remove",
            json!({"name": name}),
            Value::Null,
            said,
        );
    }

    pub fn check_registry(&self, name: &str) {
        let checked = name.to_owned();
        self.say(format!("Checking {name}…"));
        self.call(
            "registries.check",
            json!({"name": name}),
            Value::Null,
            move |admin, found| {
                let mut data = admin.data.borrow_mut();
                data.report = vec![format!("Checked {checked}")];
                data.report.extend(
                    found["lines"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|line| line.as_str().map(str::to_owned)),
                );
                data.said = Some(format!("Checked {checked}"));
            },
        );
    }

    pub fn set_secret(&self, name: &str, value: &str, said: String) {
        self.change(
            "secrets.set",
            json!({"name": name}),
            json!({"value": value}),
            said,
        );
    }

    pub fn remove_secret(&self, name: &str, said: String) {
        self.data.borrow_mut().secrets.retain(|known| known != name);
        self.change("secrets.remove", json!({"name": name}), Value::Null, said);
    }

    pub fn set_setting(&self, key: &str, value: &str, said: String) {
        self.change(
            "settings.set",
            json!({"key": key}),
            json!({"value": value}),
            said,
        );
    }

    pub fn remove_host(&self, name: &str) {
        let said = format!("Removed {name}. The host itself is as it was.");
        self.call(
            "hosts.remove",
            json!({"name": name}),
            Value::Null,
            move |admin, _| {
                let mut data = admin.data.borrow_mut();
                data.said = Some(said);
                data.hosts_changed = true;
            },
        );
    }

    fn arrive(&self, what: Arrived) {
        self.data.borrow_mut().arrived.push(what);
    }

    pub fn profile(&self, name: &str) {
        let admin = self.clone();
        self.portal.call(
            "hosts.profile",
            None,
            json!({"name": name}),
            Value::Null,
            move |result| admin.arrive(Arrived::Profile(result)),
        );
    }

    /// Save a profile, and go on with `then` once the server has it.
    pub fn save(&self, name: &str, profile: &Value, then: impl FnOnce(&Admin) + 'static) {
        let kept = profile.clone();
        self.call(
            "hosts.save",
            json!({"name": name}),
            profile.clone(),
            move |admin, _| {
                admin.arrive(Arrived::Saved(kept));
                admin.data.borrow_mut().hosts_changed = true;
                then(admin);
            },
        );
    }

    pub fn generate(&self, field: Field, asked: Value) {
        self.call("setup.generate", json!({}), asked, move |admin, made| {
            let note = made["note"].as_str().unwrap_or("").to_owned();
            if !note.is_empty() {
                admin.say(note);
            }
            admin.arrive(Arrived::Generated(
                field,
                made["value"].as_str().unwrap_or("").to_owned(),
                made["password"].as_str().map(str::to_owned),
            ));
        });
    }

    pub fn seed(&self, name: &str) {
        self.say("Rendering the seed…");
        self.call(
            "hosts.seed",
            json!({"name": name}),
            Value::Null,
            |admin, seed| {
                admin.say("");
                admin.arrive(Arrived::Seed(
                    seed["user_data"].as_str().unwrap_or("").to_owned(),
                ));
            },
        );
    }

    pub fn send_secrets(&self, name: &str) {
        self.call(
            "hosts.secrets",
            json!({"name": name}),
            Value::Null,
            |admin, session| admin.arrive(Arrived::Session(session)),
        );
    }

    /// Apply a host's profile to it, and follow the job in a panel.
    pub fn apply(&self, name: &str, password: Option<&str>, size: (u16, u16)) {
        let body = match password {
            Some(password) => json!({"sudo_password": password}),
            None => Value::Null,
        };
        let title = format!("Apply this configuration · {name}");
        let admin = self.clone();
        self.say("Starting…");
        self.portal.call(
            "hosts.apply",
            None,
            json!({"name": name}),
            body,
            move |result| match result {
                Ok(job) => {
                    admin.say("");
                    admin.data.borrow_mut().job = Some(View::plain(&title, size.0, size.1));
                    let data = admin.data.clone();
                    admin.portal.stream(
                        "jobs.follow",
                        None,
                        json!({"id": job["id"]}),
                        move |frame| {
                            let mut data = data.borrow_mut();
                            let Some(view) = &mut data.job else {
                                return;
                            };
                            match frame {
                                Frame::Bytes(bytes) => {
                                    view.feed(&bytes);
                                }
                                Frame::Data(said) => {
                                    if let Some(code) = berth_tui::host::exited(&said) {
                                        view.ended = Some(code);
                                    }
                                }
                                Frame::End(why) => {
                                    if view.ended.is_none() {
                                        if let Some(why) = why {
                                            view.feed(format!("\n{why}\n").as_bytes());
                                        }
                                        view.ended = Some(None);
                                    }
                                    data.hosts_changed = true;
                                }
                            }
                        },
                    );
                }
                Err(error) if error.contains("sudo_password") => admin.arrive(Arrived::Password),
                Err(error) => admin.say(format!("✗ {error}")),
            },
        );
    }
}
