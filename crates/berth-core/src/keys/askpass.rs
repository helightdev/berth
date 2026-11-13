//! berth as ssh's askpass helper. ssh runs `SSH_ASKPASS` with the prompt as
//! its argument and reads the answer from its standard output. berth sets
//! `SSH_ASKPASS` to itself and [`SOURCE`] to where the password is: the
//! secret of the host in the store (`secret:NAME`), or a private file that
//! lives as long as one login (`file:PATH`). The password itself is never in
//! an argument or in the environment of ssh, which may live on for minutes
//! as the master of shared connections.

use crate::Result;
use crate::paths::Paths;

/// The variable that says where the password is.
pub const SOURCE: &str = "BERTH_ASKPASS";

/// The answer to ssh's prompt. Only a prompt for a password is answered;
/// anything else, such as a question about a host key, is refused.
pub fn answer(prompt: &str, source: &str, paths: Option<&Paths>) -> Result<String> {
    if !prompt.to_lowercase().contains("password") {
        return Err(format!("berth answers only a password prompt, not: {prompt}").into());
    }
    if let Some(file) = source.strip_prefix("file:") {
        let text = std::fs::read_to_string(file).map_err(|error| format!("{file}: {error}"))?;
        return Ok(text.trim_end_matches(['\n', '\r']).to_owned());
    }
    if let Some(name) = source.strip_prefix("secret:") {
        let paths = match paths {
            Some(paths) => paths.clone(),
            None => Paths::locate()?,
        };
        let store = crate::store::shared(&paths)?;
        return crate::registry::Store::own(store)
            .secret(name)?
            .ok_or_else(|| format!("No secret {name} in berth's store").into());
    }
    Err(format!("{SOURCE} says where the password is: secret:NAME or file:PATH").into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_password_comes_from_its_file_or_its_secret_and_nothing_else_is_answered() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("password");
        std::fs::write(&file, "made-up-pw\n").unwrap();
        let source = format!("file:{}", file.display());
        assert_eq!(
            answer("ops@192.0.2.10's password: ", &source, None).unwrap(),
            "made-up-pw"
        );
        assert!(
            answer(
                "Are you sure you want to continue connecting (yes/no)?",
                &source,
                None
            )
            .is_err()
        );
        let paths = Paths {
            config_dir: dir.path().join("config"),
            data_dir: dir.path().join("data"),
        };
        let store = crate::store::shared(&paths).unwrap();
        crate::registry::Store::own(store)
            .set_secret("ssh/studio", Some("kept-pw"))
            .unwrap();
        assert_eq!(
            answer("Password: ", "secret:ssh/studio", Some(&paths)).unwrap(),
            "kept-pw"
        );
        assert!(answer("Password: ", "secret:ssh/attic", Some(&paths)).is_err());
    }
}
