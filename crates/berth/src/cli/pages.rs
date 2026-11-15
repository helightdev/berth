//! The pages `berth plugin show`, `berth provisioner show` and `berth resource
//! show` print, and the lines of what an action answered.

use berth_api::{Action, PluginInfo, ProvisionerInfo, ProvisionerKind, ProvisionerState, Resource};
use std::collections::BTreeMap;

pub use berth_tui::provisioners::answer_lines;

/// What `berth plugin show` prints: each plugin with what its features add.
pub fn plugins(plugins: &[PluginInfo]) -> String {
    let mut text = String::new();
    for plugin in plugins {
        text += &format!(
            "{}  {} {}  {}\n",
            plugin.id, plugin.package, plugin.version, plugin.dir
        );
        for feature in &plugin.features {
            text += &format!(
                "  {}/{}  {} ({})\n",
                plugin.id, feature.id, feature.label, feature.group
            );
            for action in &feature.actions {
                let mut notes = vec![format!("as {}", action.user)];
                if action.agent {
                    notes.push("the agent may run it".into());
                }
                if !action.extends.is_empty() {
                    notes.push(format!("part of {}", action.extends.join(", ")));
                }
                for argument in &action.arguments {
                    notes.push(format!("asks {argument}"));
                }
                text += &format!(
                    "    action   {}:{}  {} ({})\n",
                    plugin.id,
                    action.name,
                    action.label,
                    notes.join("; ")
                );
            }
            for service in &feature.services {
                text += &format!(
                    "    service  {}/{}  as {}: {}\n",
                    plugin.id,
                    service.name,
                    service.user,
                    service.methods.join(", ")
                );
            }
            for channel in &feature.channels {
                text += &format!("    channel  {}/{channel}\n", plugin.id);
            }
        }
        for provisioner in &plugin.provisioners {
            text += &format!(
                "  provisioner  {}  {} ({})\n",
                provisioner.id,
                provisioner.label,
                provisioner_notes(provisioner).join("; ")
            );
        }
    }
    text
}

/// What a provisioner does and asks for, in a few words each.
pub fn provisioner_notes(provisioner: &ProvisionerInfo) -> Vec<String> {
    if !provisioner.problem.is_empty() && provisioner.kinds.is_empty() {
        return vec![provisioner.problem.clone()];
    }
    let makes = |kind: &ProvisionerKind| match (kind.id.as_str(), kind.setup.as_str()) {
        ("machine", "seed") => "machines that set themselves up from the cloud seed".to_owned(),
        ("machine", _) => "machines berth applies the profile to over SSH".to_owned(),
        _ => kind.label.clone(),
    };
    let kinds: Vec<String> = provisioner.kinds.iter().map(makes).collect();
    let mut notes = vec![];
    if !kinds.is_empty() {
        notes.push(format!("makes {}", kinds.join(" and ")));
    }
    if !provisioner.actions.is_empty() {
        let names: Vec<&str> = provisioner
            .actions
            .iter()
            .map(|action| action.name.as_str())
            .collect();
        notes.push(format!("actions {}", names.join(", ")));
    }
    if !provisioner.verbs.is_empty() {
        notes.push(format!("also {}", provisioner.verbs.join(", ")));
    }
    for argument in &provisioner.arguments {
        let kept = provisioner.settings.get(&argument.name);
        let mut note = match (argument.required, argument.default.as_str(), kept) {
            (_, _, Some(value)) => format!("asks {} (set to {value})", argument.name),
            (true, _, None) => format!("asks {}", argument.name),
            (false, "", None) => format!("may ask {}", argument.name),
            (false, default, None) => format!("asks {} (default {default})", argument.name),
        };
        if !argument.values.is_empty() {
            note += &format!(": {}", argument.values.join(" or "));
        }
        notes.push(note);
    }
    for secret in &provisioner.secrets {
        let missing = provisioner.missing_secrets.contains(secret);
        notes.push(match missing {
            true => format!("secret {secret}, not set"),
            false => format!("secret {secret}"),
        });
    }
    if !provisioner.problem.is_empty() {
        notes.push(format!("not usable: {}", provisioner.problem));
    }
    notes
}

/// What `berth provisioner show NAME` says of it.
pub fn provisioner(
    state: &ProvisionerState,
    actions: &[Action],
    given: &BTreeMap<String, String>,
) -> String {
    let info = &state.provisioner;
    let mut text = format!("{}  {}\n", info.id, info.label);
    if !info.description.is_empty() {
        text += &format!("{}\n", info.description);
    }
    // A reason that is long, or has several lines, goes below in full.
    let pieces: Vec<&str> = state
        .message
        .lines()
        .flat_map(|line| line.split(" | "))
        .map(str::trim)
        .filter(|piece| !piece.is_empty())
        .collect();
    let long = pieces.len() > 1 || state.message.chars().count() > 60;
    match (state.message.is_empty(), long) {
        (true, _) => text += &format!("\nState     {}\n", state.state),
        (false, false) => text += &format!("\nState     {} · {}\n", state.state, state.message),
        (false, true) => {
            text += &format!("\nState     {}\n", state.state);
            for piece in pieces {
                text += &format!("  {piece}\n");
            }
        }
    }
    let width = state
        .details
        .iter()
        .map(|row| row.label.chars().count())
        .max()
        .unwrap_or(0)
        .max(8);
    for row in &state.details {
        text += &format!("{:<width$}  {}\n", row.label, row.value);
    }
    if !info.node.is_empty() {
        text += &format!("Node      {}, whose feature offers it\n", info.node);
    }
    if !info.own.is_empty() {
        let set: Vec<String> = info
            .own
            .iter()
            .map(|name| match (given.get(name), info.settings.get(name)) {
                (Some(value), _) => format!("{name}={value} (given for this call, not saved)"),
                (None, Some(value)) => format!("{name}={value}"),
                (None, None) => format!("{name} not set"),
            })
            .collect();
        text += &format!("Settings  {}\n", set.join(" · "));
        if info.own.iter().any(|name| given.contains_key(name)) {
            text += &format!(
                "          To keep them: berth provisioner set {} NAME=VALUE\n",
                info.id
            );
        }
    }
    let mut kinds: Vec<&str> = vec![];
    for held in &state.resources {
        if !kinds.contains(&held.kind.as_str()) {
            kinds.push(&held.kind);
        }
    }
    if kinds.is_empty() {
        text += "\nWhat berth made with it\n  none\n";
    }
    for kind in kinds {
        let label = info
            .kinds
            .iter()
            .find(|known| known.id == kind)
            .map(|known| known.label.as_str())
            .filter(|label| !label.is_empty() && kind != "machine");
        text += &format!(
            "\n{} berth made with it\n",
            match (kind, label) {
                ("machine", _) => "Machines",
                (_, Some(label)) => label,
                (kind, None) => kind,
            }
        );
        for held in state.resources.iter().filter(|held| held.kind == kind) {
            let mut notes = vec![];
            if !held.state.is_empty() {
                notes.push(held.state.clone());
            }
            match held.resource.is_empty() {
                true => notes.push("berth has no record of it".into()),
                false => notes.push(held.resource.clone()),
            }
            let id = match held.id.as_str() {
                "" => "(being made)",
                id => id,
            };
            let name = match held.host.is_empty() {
                true => &held.name,
                false => &held.host,
            };
            text += &format!("  {name:<16} {id}  {}\n", notes.join(" · "));
        }
    }
    let mut groups: Vec<(&str, Vec<&Action>)> = vec![];
    for action in actions {
        let title = berth_tui::page::group_of(action);
        match groups
            .iter_mut()
            .find(|(known, _)| known.eq_ignore_ascii_case(title))
        {
            Some((_, list)) => list.push(action),
            None => groups.push((title, vec![action])),
        }
    }
    if !groups.is_empty() {
        text += &format!("\nActions (berth provisioner run {} ACTION)\n", info.id);
    }
    for (title, list) in groups {
        text += &format!("  {title}\n");
        for action in list {
            let why = match action.available {
                true => String::new(),
                false => format!("  ({})", action.why),
            };
            text += &format!("    {:<18} {}{why}\n", action.name, action.title());
        }
    }
    text
}

/// What `berth resource show ID` says of it.
pub fn resource(found: &Resource, actions: &[Action]) -> String {
    let mut text = format!(
        "{}  {} {} of {}\n",
        found.id, found.kind, found.name, found.provisioner
    );
    let mut row = |label: &str, value: &str| {
        if !value.is_empty() {
            text += &format!("{label:<11}{value}\n");
        }
    };
    row("State", &found.state);
    row("Problem", &found.problem);
    row("Made as", &found.external_id);
    row("Host", found.host.as_deref().unwrap_or(""));
    row("Address", &found.address);
    row("Created", &found.created);
    row("Asked", &found.checked);
    row("Spec", &found.spec_sha256);
    let secrets: Vec<String> = found
        .secrets
        .iter()
        .map(|secret| format!("{} as {}", secret.name, secret.variable()))
        .collect();
    row("Secrets", &secrets.join(", "));
    let arguments: Vec<String> = found
        .arguments
        .iter()
        .filter(|(_, value)| !value.is_empty())
        .map(|(key, value)| format!("{key}={value}"))
        .collect();
    row("Arguments", &arguments.join(" "));
    if !found.endpoints.is_empty() {
        text += &format!("\nEndpoints (berth port forward {} ENDPOINT)\n", found.id);
        let width = found
            .endpoints
            .iter()
            .map(|endpoint| endpoint.name.chars().count())
            .max()
            .unwrap_or(0);
        for endpoint in &found.endpoints {
            let mut notes = vec![];
            if !endpoint.hint.is_empty() {
                notes.push(endpoint.hint.clone());
            }
            if endpoint.local {
                notes.push("on the server's machine".to_owned());
            }
            let notes = match notes.is_empty() {
                true => String::new(),
                false => format!("  {}", notes.join(" · ")),
            };
            text += &format!(
                "  {:<width$}  {}:{}/{}{notes}\n",
                endpoint.name, endpoint.address, endpoint.port, endpoint.protocol
            );
        }
    }
    if !actions.is_empty() {
        text += &format!("\nActions (berth resource run {} ACTION)\n", found.id);
    }
    for action in actions {
        let why = match action.available {
            true => String::new(),
            false => format!("  ({})", action.why),
        };
        text += &format!("  {:<18} {}{why}\n", action.name, action.title());
    }
    if found.verbs.iter().any(|verb| verb == "logs") {
        text += &format!("\nIts log: berth resource logs {} [--follow]\n", found.id);
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use berth_api::{Detail, ListedResource, ResourceSecret};
    use serde_json::json;

    #[test]
    fn a_provisioners_page_says_how_it_is_what_it_made_and_what_it_can_do() {
        let state = ProvisionerState {
            provisioner: ProvisionerInfo {
                id: "incus/vm".into(),
                label: "Incus virtual machine".into(),
                own: vec!["server".into(), "project".into()],
                settings: [("server".to_owned(), "https://192.0.2.10:8443".to_owned())].into(),
                ..Default::default()
            },
            state: "ready".into(),
            message: "Incus 6.18 answers".into(),
            details: vec![Detail {
                label: "Kernel".into(),
                value: "Linux 6.17".into(),
            }],
            resources: vec![
                ListedResource {
                    id: "berth-studio".into(),
                    kind: "machine".into(),
                    name: "studio".into(),
                    host: "studio".into(),
                    state: "running".into(),
                    resource: "r-studio".into(),
                },
                ListedResource {
                    id: "berth-attic".into(),
                    kind: "machine".into(),
                    name: "attic".into(),
                    host: "attic".into(),
                    ..ListedResource::default()
                },
            ],
            ..Default::default()
        };
        let actions = [
            Action {
                name: "pools".into(),
                label: "Storage pools".into(),
                group: "Storage".into(),
                ..Action::default()
            },
            Action {
                name: "settings".into(),
                label: "Settings".into(),
                group: "Set-Up".into(),
                available: false,
                why: "no".into(),
                ..Action::default()
            },
        ];
        let text = provisioner(&state, &actions, &BTreeMap::new());
        for line in [
            "State     ready · Incus 6.18 answers\n",
            "Kernel    Linux 6.17\n",
            "Settings  server=https://192.0.2.10:8443 · project not set\n",
            "  studio           berth-studio  running · r-studio\n",
            "  attic            berth-attic  berth has no record of it\n",
            "Actions (berth provisioner run incus/vm ACTION)\n",
            "    pools              Storage pools\n",
            "    settings           Settings  (no)\n",
        ] {
            assert!(text.contains(line), "{line:?} in\n{text}");
        }
        let given: BTreeMap<String, String> =
            [("server".to_owned(), "https://192.0.2.11:8443".to_owned())].into();
        let long = ProvisionerState {
            state: "unreachable".into(),
            message: "docker version failed: Cannot connect to the Docker daemon | error during connect: ssh: handshake failed".into(),
            ..state.clone()
        };
        let text = provisioner(&long, &[], &given);
        for line in [
            "State     unreachable\n",
            "  docker version failed: Cannot connect to the Docker daemon\n",
            "  error during connect: ssh: handshake failed\n",
            "server=https://192.0.2.11:8443 (given for this call, not saved) · project not set\n",
            "berth provisioner set incus/vm NAME=VALUE\n",
        ] {
            assert!(text.contains(line), "{line:?} in\n{text}");
        }
        let lines = answer_lines(&json!({"message": "2 pools", "details": [
            {"label": "default", "value": "zfs"}, {"label": "fast-ssd", "value": "dir"}]}));
        assert_eq!(lines, ["2 pools", "  default   zfs", "  fast-ssd  dir"]);
    }

    #[test]
    fn a_resource_is_described_with_what_can_be_done_to_it() {
        let found = Resource {
            id: "r-wiki".into(),
            provisioner: "docker/container".into(),
            kind: "deployment".into(),
            name: "wiki".into(),
            state: "running".into(),
            external_id: "wiki".into(),
            secrets: vec![ResourceSecret {
                name: "wiki_db".into(),
                env: "DB_PASSWORD".into(),
            }],
            spec_sha256: "ab12".into(),
            verbs: vec!["logs".into()],
            ..Resource::default()
        };
        let actions = [Action {
            name: "remove".into(),
            label: "Remove the deployment".into(),
            ..Action::default()
        }];
        let text = resource(&found, &actions);
        for line in [
            "r-wiki  deployment wiki of docker/container\n",
            "State      running\n",
            "Secrets    wiki_db as DB_PASSWORD\n",
            "  remove             Remove the deployment\n",
            "Its log: berth resource logs r-wiki [--follow]\n",
        ] {
            assert!(text.contains(line), "{line:?} in\n{text}");
        }
    }

    #[test]
    fn a_plugin_is_shown_with_its_actions_and_its_provisioners() {
        let plugin = PluginInfo {
            id: "cloud".into(),
            package: "berth-plugin-cloud".into(),
            version: "1.0.0".into(),
            dir: "/srv/plugins/cloud".into(),
            features: vec![berth_api::PluginFeature {
                id: "cli".into(),
                label: "CLI".into(),
                group: "Tools".into(),
                actions: vec![berth_api::PluginAction {
                    name: "go".into(),
                    label: "Go".into(),
                    user: "agent".into(),
                    agent: true,
                    arguments: vec!["where".into()],
                    ..Default::default()
                }],
                channels: vec!["notes".into()],
                ..Default::default()
            }],
            provisioners: vec![ProvisionerInfo {
                id: "cloud/vm".into(),
                label: "A VM".into(),
                kinds: vec![ProvisionerKind {
                    id: "machine".into(),
                    setup: "seed".into(),
                    ..Default::default()
                }],
                verbs: vec!["stop".into()],
                secrets: vec!["key".into()],
                arguments: vec![berth_api::Argument {
                    name: "cpus".into(),
                    default: "4".into(),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        };
        let text = plugins(&[plugin]);
        for line in [
            "cloud  berth-plugin-cloud 1.0.0  /srv/plugins/cloud\n",
            "  cloud/cli  CLI (Tools)\n",
            "    action   cloud:go  Go (as agent; the agent may run it; asks where)\n",
            "    channel  cloud/notes\n",
            "  provisioner  cloud/vm  A VM (makes machines that set themselves up from the cloud seed; also stop; asks cpus (default 4); secret key)\n",
        ] {
            assert!(text.contains(line), "{line:?} in\n{text}");
        }
    }
}
