//! Actions: the host's named operations. Running one starts a session whose
//! steps are the action and the actions that extend it, each as its own user.

use crate::error::{E, R};
use crate::sessions::{Plan, Wait, new_id, started};
use crate::state::{Caller, State};
use axum::{
    Json,
    extract::{Extension, Path, Query, State as AppState},
};
use berth_api::{self as api, CommandLine, HostAction, SessionKind};
use std::{collections::BTreeMap, sync::Arc};

fn allowed(caller: &Caller, action: &HostAction) -> bool {
    caller.host() || action.agent
}

fn find(state: &State, caller: &Caller, name: &str) -> R<HostAction> {
    let config = state.config();
    let action = config
        .actions
        .iter()
        .find(|action| action.name == name)
        .ok_or_else(|| E::missing(format!("action {name}")))?;
    if !allowed(caller, action) {
        return Err(E::forbidden(format!("The agent may not run {name}")));
    }
    Ok(action.clone())
}

/// The name an argument has in the command's environment.
pub fn variable(name: &str) -> String {
    let name: String = name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    format!("BERTH_ARG_{name}")
}

pub async fn list(
    AppState(state): AppState<Arc<State>>,
    Extension(caller): Extension<Caller>,
) -> Json<Vec<HostAction>> {
    let config = state.config();
    let actions = config
        .actions
        .iter()
        .filter(|action| allowed(&caller, action));
    Json(
        actions
            .map(|action| HostAction {
                run: String::new(),
                ..action.clone()
            })
            .collect(),
    )
}

pub async fn run(
    AppState(state): AppState<Arc<State>>,
    Extension(caller): Extension<Caller>,
    Path(name): Path<String>,
    Query(wait): Query<Wait>,
    body: Option<Json<api::RunAction>>,
) -> R<Json<api::Started>> {
    let request = body.map(|Json(request)| request).unwrap_or_default();
    let action = find(&state, &caller, &name)?;
    // BERTH_TASK is what plugins written for berth 0.5 read.
    let mut env = BTreeMap::from([
        ("BERTH_ACTION".to_owned(), action.name.clone()),
        ("BERTH_TASK".to_owned(), action.name.clone()),
    ]);
    for argument in &action.arguments {
        let value = request
            .arguments
            .get(&argument.name)
            .cloned()
            .unwrap_or_else(|| argument.default.clone());
        if argument.required && value.is_empty() {
            return Err(E::bad(format!(
                "{name} needs the argument {}",
                argument.name
            )));
        }
        env.insert(variable(&argument.name), value);
    }
    if let Some(unknown) = request.arguments.keys().find(|key| {
        action
            .arguments
            .iter()
            .all(|argument| &argument.name != *key)
    }) {
        return Err(E::bad(format!("{name} has no argument {unknown}")));
    }
    let input = match (&action.input, &request.input_b64) {
        (Some(what), None) => return Err(E::bad(format!("{name} needs {what} sent as input_b64"))),
        (_, Some(data)) => Some(api::unb64(data).ok_or_else(|| E::bad("input_b64 is not base64"))?),
        (None, None) => None,
    };
    // The action itself, then what extends it, in the order of the file.
    let config = state.config();
    let extenders = config
        .actions
        .iter()
        .filter(|other| other.extends.contains(&action.name) && other.name != action.name);
    let mut steps = vec![];
    for step in std::iter::once(&action).chain(extenders) {
        let command = crate::users::command(
            &state,
            &step.user,
            &CommandLine::Shell(step.run.clone()),
            None,
            &env,
        )?;
        steps.push((
            Some(api::Step {
                name: step.name.clone(),
                user: step.user.clone(),
                exit_code: None,
            }),
            command,
        ));
    }
    let plan = Plan {
        id: new_id(),
        kind: if action.terminal {
            SessionKind::Terminal
        } else {
            SessionKind::Process
        },
        title: if action.label.is_empty() {
            action.name.clone()
        } else {
            action.label.clone()
        },
        command: vec![action.name.clone()],
        user: action.user.clone(),
        action: Some(action.name.clone()),
        steps,
        input,
        close_input: true,
        timeout: None,
        rows: request.rows.unwrap_or(24).clamp(2, 1000),
        cols: request.cols.unwrap_or(80).clamp(2, 1000),
    };
    let session = state.sessions.start(&state, &caller, plan)?;
    Ok(Json(started(&session, &wait).await?))
}

pub async fn choices(
    AppState(state): AppState<Arc<State>>,
    Extension(caller): Extension<Caller>,
    Path((name, argument)): Path<(String, String)>,
) -> R<Json<api::Choices>> {
    let action = find(&state, &caller, &name)?;
    let argument = action
        .arguments
        .iter()
        .find(|known| known.name == argument)
        .ok_or_else(|| E::missing(format!("argument {argument}")))?;
    if argument.choices.is_empty() {
        return Ok(Json(api::Choices { choices: vec![] }));
    }
    let line = CommandLine::Shell(argument.choices.clone());
    let (code, output, errors) =
        crate::users::capture(&state, &action.user, &line, 15, 256 << 10).await?;
    if code != 0 {
        return Err(E::internal(format!(
            "The choices command failed ({code}): {errors}"
        )));
    }
    let text = String::from_utf8_lossy(&output);
    let choices = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect();
    Ok(Json(api::Choices { choices }))
}
