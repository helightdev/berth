//! `berth connect URL`: use the berth server at URL, with the token a setup
//! key collects. The key comes on standard input, so it is in no argv, and
//! the server and token are kept as berth's settings.

use crate::Result;
use berth_page::net;
use berth_page::settings::{self, Settings};
use serde_json::Value;
use std::io::{IsTerminal, Read};

/// `berth connect URL`, keeping the server in the settings of this user.
pub fn command(url: &str) -> Result<()> {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err(format!(
            "{url} is no address of a berth server: https://HOST or http://HOST:PORT"
        )
        .into());
    }
    let key = read_secret("Setup key")?;
    let settings = Settings::new(settings::folder(|name| std::env::var(name).ok()));
    let (url, version) = connect_with(&settings, url, key.trim())?;
    println!("This berth now uses the berth server at {url}, berth {version}");
    if std::env::var_os("BERTH_SERVER").is_some() {
        eprintln!("BERTH_SERVER is set in this shell and is used before the setting");
    }
    Ok(())
}

/// A secret from the terminal without showing it, or all of standard input.
fn read_secret(prompt: &str) -> Result<String> {
    let mut value = String::new();
    if std::io::stdin().is_terminal() {
        use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
        eprint!("{prompt}: ");
        crossterm::terminal::enable_raw_mode()?;
        let read = (|| -> std::io::Result<bool> {
            loop {
                if let Event::Key(key) = event::read()?
                    && key.kind != KeyEventKind::Release
                {
                    match key.code {
                        KeyCode::Enter => return Ok(true),
                        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            return Ok(false);
                        }
                        KeyCode::Char(ch) => value.push(ch),
                        KeyCode::Backspace => {
                            value.pop();
                        }
                        _ => {}
                    }
                }
            }
        })();
        let _ = crossterm::terminal::disable_raw_mode();
        eprintln!();
        if !read? {
            return Err("Nothing entered".into());
        }
    } else {
        std::io::stdin().read_to_string(&mut value)?;
    }
    let value = value.trim_end_matches(['\n', '\r']).to_owned();
    match value.is_empty() {
        true => Err("Nothing entered".into()),
        false => Ok(value),
    }
}

/// [`command`] with the key in hand: the server's address and version.
/// Nothing is kept unless all of it works.
pub fn connect_with(settings: &Settings, url: &str, key: &str) -> Result<(String, String)> {
    let url = url.trim().trim_end_matches('/').to_owned();
    let key = key.to_owned();
    let ask = |method: &'static str| {
        let (url, key) = (url.clone(), key.clone());
        async move {
            let (status, body) = net::http(method, &url, "/v1/setup-key", &key).await?;
            let said: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
            match status {
                200 => Ok(said),
                401 => Err(
                    "The server did not take the setup key: it is wrong, used up or expired. Make a new one where the server runs: berth setup-key".to_owned(),
                ),
                404 | 405 => Err("The server knows no setup keys: it is an older berth".to_owned()),
                status => Err(match said["error"]["message"].as_str() {
                    Some(message) => format!("HTTP {status}: {message}"),
                    None => format!("HTTP {status}"),
                }),
            }
        }
    };
    let (version, token) = net::runtime().block_on(async {
        let checked = ask("GET").await?;
        let claimed = ask("POST").await?;
        let token = claimed["token"].as_str().unwrap_or("").to_owned();
        if token.is_empty() {
            return Err("The server sent no token".to_owned());
        }
        // Kept only once the token is seen to open the server.
        let target = net::target(&url, berth_api::ops::PORTAL)?;
        let stream = net::connect(&target).await?;
        let socket =
            berth_api::client::websocket_at(stream, &target.authority, &target.path, &token)
                .await
                .map_err(|error| net::describe(&error))?;
        berth_api::portal::Portal::over(socket)
            .call("access", None, Value::Null, None)
            .await
            .map_err(|error| net::describe(&error))?;
        let version = checked["version"].as_str().unwrap_or("").to_owned();
        Ok::<_, String>((version, token))
    })?;
    settings.set(&[(settings::SERVER, &url), (settings::TOKEN, &token)])?;
    if let Err(error) = net::runtime().block_on(ask("DELETE")) {
        eprintln!(
            "The setup key could not be ended ({error}). It stops by itself within a day, or with berth setup-key --remove where the server runs."
        );
    }
    Ok((url, version))
}
