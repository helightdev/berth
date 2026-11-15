//! The spellings of berth 0.5 that mean something else now, or nothing.
//! Each fails with the spelling to use instead, so that a script or a
//! habit does not do something it did not mean.

use super::fail::Fail;

/// The flags of every command that take a value, which come before the
/// first word of the command.
const VALUED: &[&str] = &["--server", "--token-file", "-o", "--output", "--color"];

/// Old command words and what to type now.
const OLD: &[(&[&str], &str)] = &[
    (&["list"], "berth host list, or berth ls"),
    (&["remove"], "berth host remove NAME"),
    (
        &["profile"],
        "berth host export NAME, berth host import NAME FILE or berth host set NAME KEY=VALUE",
    ),
    (&["credentials"], "berth host credentials NAME"),
    (&["adopt"], "berth host adopt NAME [ADDRESS]"),
    (&["iso"], "berth host iso NAME"),
    (&["seed"], "berth host seed NAME"),
    (&["inventory"], "berth host inventory [NAME…]"),
    (&["provisioners"], "berth provisioner list"),
    (
        &["provision"],
        "berth host create NAME --provisioner PROVISIONER",
    ),
    (
        &["instance"],
        "berth host start NAME, berth host stop NAME or berth host remove NAME --destroy",
    ),
    (
        &["deploy"],
        "berth resource create NAME --provisioner PROVISIONER --spec FILE",
    ),
    (&["actions"], "berth action list HOST"),
    (&["manage"], "berth run HOST ACTION"),
    (&["attach"], "berth session attach HOST [ID]"),
    (&["sessions"], "berth session list HOST"),
    (&["output"], "berth session logs HOST ID"),
    (&["call"], "berth service call HOST SERVICE METHOD"),
    (&["plugins"], "berth plugin list, add, remove or update"),
    (&["connect"], "berth login URL"),
    (&["setup-key"], "berth server setup-key"),
    (&["store"], "berth server store"),
    (&["openapi"], "berth server api openapi"),
    (&["asyncapi"], "berth server api asyncapi"),
    (&["operations"], "berth server api operations"),
    (
        &["publish"],
        "berth registry publish DESTINATION --host HOST",
    ),
    (&["image"], "berth registry push-image DESTINATION"),
    (
        &["host", "install"],
        "berth host apply NAME for a managed host, or berth run HOST install-controller",
    ),
    (&["host", "type"], "berth host set NAME type=TYPE"),
    (&["key", "new"], "berth key create NAME"),
    (&["key", "add"], "berth key import NAME FILE"),
    (&["key", "host"], "berth host verify NAME [--accept]"),
    (
        &["registry", "repo"],
        "berth registry add-repo NAME FORMAT REPOSITORY",
    ),
    (&["apply", "--behind"], "berth rollout"),
    (&["apply", "--all"], "berth rollout --all"),
    (&["channel"], "berth channel listen HOST"),
];

/// The words of the command, without the flags before them.
fn words(args: &[String]) -> Vec<&str> {
    let mut words = vec![];
    let mut rest = args.iter().skip(1);
    while let Some(arg) = rest.next() {
        if VALUED.contains(&arg.as_str()) {
            rest.next();
            continue;
        }
        if arg.starts_with('-') && words.is_empty() {
            continue;
        }
        words.push(arg.as_str());
    }
    words
}

/// Why these arguments are refused, when they are a spelling of before.
pub fn refuse(args: &[String]) -> Option<Fail> {
    if args
        .iter()
        .skip(1)
        .any(|arg| arg == "--token" || arg.starts_with("--token="))
    {
        return Some(Fail::usage(
            "--token puts the token where other users can read it. Use --token-file FILE or --token-stdin, or keep it with berth login URL",
        ));
    }
    let words = words(args);
    let first = *words.first()?;
    for (old, new) in OLD {
        let matches = old
            .iter()
            .enumerate()
            .all(|(index, word)| words.get(index) == Some(word));
        if !matches {
            continue;
        }
        // `berth channel listen` is the new spelling of the old `channel`.
        if old == &["channel"] && words.get(1) == Some(&"listen") {
            continue;
        }
        // A word that is a command now as well, with its new grammar.
        if old.len() == 1 && first == "channel" && words.len() < 2 {
            continue;
        }
        return Some(Fail::usage(format!("berth {} is {new} now", old.join(" "))));
    }
    // `berth resource ID remove` and the like: the action goes first now.
    if let [
        noun @ ("resource" | "provisioner" | "host"),
        name,
        action,
        ..,
    ] = words[..]
        && !name.starts_with('-')
        && !is_action(noun, name)
        && is_action(noun, action)
    {
        return Some(Fail::usage(format!(
            "The action comes before the name now: berth {noun} {action} {name}"
        )));
    }
    None
}

fn is_action(noun: &str, word: &str) -> bool {
    let command = super::command();
    command
        .find_subcommand(noun)
        .is_some_and(|thing| thing.find_subcommand(word).is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn said(line: &str) -> Option<String> {
        let args: Vec<String> = std::iter::once("berth")
            .chain(line.split_whitespace())
            .map(String::from)
            .collect();
        refuse(&args).map(|fail| fail.message)
    }

    #[test]
    fn old_spellings_name_the_new_ones() {
        assert_eq!(
            said("remove studio").as_deref(),
            Some("berth remove is berth host remove NAME now")
        );
        assert_eq!(
            said("--server https://berth.example apply --behind").as_deref(),
            Some("berth apply --behind is berth rollout now")
        );
        assert_eq!(
            said("resource r1 remove").as_deref(),
            Some("The action comes before the name now: berth resource remove r1")
        );
        assert!(said("channel studio").is_some());
        assert!(said("--token s3cret ls").unwrap().contains("--token-file"));
        for fine in [
            "host remove studio",
            "apply studio",
            "channel listen studio",
            "ls",
            "resource show r1",
        ] {
            assert_eq!(said(fine), None, "{fine}");
        }
    }
}
