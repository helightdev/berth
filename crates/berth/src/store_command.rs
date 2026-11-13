//! `berth store`: where the store is, sealing its values with a key, and
//! copying it to another database.

use crate::Result;
use berth_core::paths::Paths;
use berth_core::seal::{self, Key, Keys};
use berth_core::store::{self, Installation, Store, Target};
use std::fs;
use std::io::{BufRead, IsTerminal, Write};

/// Asks a yes-or-no question.
type Confirm<'a> = &'a mut dyn FnMut(&str) -> Result<bool>;

const USAGE: &str = "Usage: berth store [encrypt | rekey | decrypt [--yes]]
       berth store copy TARGET | - | --target-file FILE [--into-existing]

copy writes everything of this store into TARGET, a SQLite file or a
postgres:// URL, as the store setting takes it. A URL with a password shows
up in the process list: give it on standard input (-) or in a file instead.";

/// `berth store [encrypt | rekey | decrypt [--yes] | copy …]`
pub fn command(paths: &Paths, args: &[&str]) -> Result<()> {
    let installation = Installation::of(paths);
    let mut out = std::io::stdout().lock();
    let mut input = std::io::stdin().lock();
    run(paths, &installation, args, &mut out, &mut ask, &mut input)
}

/// A yes or no on the terminal; without one there is nobody to ask.
fn ask(question: &str) -> Result<bool> {
    if !std::io::stdin().is_terminal() {
        return Err("This asks first: run it on a terminal, or add --yes".into());
    }
    print!("{question} [y/N] ");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer)?;
    Ok(answer.trim().eq_ignore_ascii_case("y"))
}

fn run(
    paths: &Paths,
    installation: &Installation,
    args: &[&str],
    out: &mut dyn Write,
    confirm: Confirm<'_>,
    input: &mut dyn BufRead,
) -> Result<()> {
    match *args {
        [] => status(installation, out),
        ["copy", ref rest @ ..] => copy(paths, installation, rest, out, input),
        ["encrypt"] => encrypt(paths, installation, out),
        ["rekey"] => rekey(paths, installation, out),
        ["decrypt"] => decrypt(paths, installation, out, Some(confirm)),
        ["decrypt", "--yes"] => decrypt(paths, installation, out, None),
        _ => Err(USAGE.into()),
    }
}

fn status(installation: &Installation, out: &mut dyn Write) -> Result<()> {
    let store = installation.open()?;
    let file = installation.key_file.display();
    writeln!(out, "Store      {}", installation.target.describe())?;
    writeln!(out, "Schema     {}", store.schema_version()?)?;
    match Key::read(&installation.key_file)? {
        Some(key) => writeln!(
            out,
            "Encrypted  yes, with key {} in {file}",
            seal::hex(&key.id())
        )?,
        None => writeln!(
            out,
            "Encrypted  no; berth store encrypt would keep its key in {file}"
        )?,
    }
    let report = store.seal_report()?;
    writeln!(
        out,
        "Values     {} sealed, {} plain",
        report.sealed, report.plain
    )?;
    if !report.key_ids.is_empty() {
        writeln!(out, "Keys used  {}", report.key_ids.join(", "))?;
    }
    Ok(())
}

/// Held while the key changes. A running `berth serve` would go on sealing
/// with the key it loaded, so it has to be stopped first.
fn hold_serve(paths: &Paths) -> Result<fs::File> {
    store::hold_lock(&paths.data_dir.join("serve.lock"))?
        .ok_or_else(|| "berth serve runs on this data directory: stop berth serve first".into())
}

fn hold_keys(installation: &Installation) -> Result<fs::File> {
    installation
        .lock_keys()?
        .ok_or_else(|| "Another berth is changing the store's key".into())
}

/// Write a new key to `<file>.new` and rewrite every row with it. The
/// caller holds the lock of key changes, which made the key's directory, and
/// moves the key into place; a failure removes it again.
fn rewrite_with_new_key(
    installation: &Installation,
    store: &berth_core::store::Store,
) -> Result<(usize, [u8; 8])> {
    let key = Key::generate()?;
    let id = key.id();
    let new_file = installation.new_key_file();
    key.write_new(&new_file)?;
    let keys = Keys::new(key, vec![]);
    match store.reseal(Some(&keys)) {
        Ok(count) => Ok((count, id)),
        Err(error) => {
            let _ = fs::remove_file(&new_file);
            Err(error.into())
        }
    }
}

fn encrypt(paths: &Paths, installation: &Installation, out: &mut dyn Write) -> Result<()> {
    let _serve = hold_serve(paths)?;
    let store = installation.open()?;
    let file = &installation.key_file;
    if store.encrypted() || file.exists() {
        return Err(format!(
            "The store is encrypted already, with the key in {}. berth store rekey changes the key",
            file.display()
        )
        .into());
    }
    let _keys = hold_keys(installation)?;
    let (count, id) = rewrite_with_new_key(installation, &store)?;
    fs::rename(installation.new_key_file(), file)?;
    writeln!(
        out,
        "Sealed {count} values with key {}. The key is in {}: back it up apart from the database, which cannot be read without it.",
        seal::hex(&id),
        file.display()
    )?;
    Ok(())
}

fn rekey(paths: &Paths, installation: &Installation, out: &mut dyn Write) -> Result<()> {
    let _serve = hold_serve(paths)?;
    let store = installation.open()?;
    if !store.encrypted() {
        return Err("The store is not encrypted: berth store encrypt makes its key".into());
    }
    let _keys = hold_keys(installation)?;
    let (count, id) = rewrite_with_new_key(installation, &store)?;
    let (file, old) = (&installation.key_file, installation.old_key_file());
    fs::rename(file, &old)?;
    fs::rename(installation.new_key_file(), file)?;
    writeln!(
        out,
        "Sealed {count} values with the new key {} in {}. The previous key is in {}, for backups of the database made before; back up the new one apart from the database.",
        seal::hex(&id),
        file.display(),
        old.display()
    )?;
    Ok(())
}

fn decrypt(
    paths: &Paths,
    installation: &Installation,
    out: &mut dyn Write,
    confirm: Option<Confirm<'_>>,
) -> Result<()> {
    let _serve = hold_serve(paths)?;
    let store = installation.open()?;
    if !store.encrypted() {
        return Err("The store is not encrypted".into());
    }
    let (file, old) = (&installation.key_file, installation.old_key_file());
    if let Some(confirm) = confirm {
        let question = format!(
            "Write every value of the store plain, readable to anyone who can read the database, and move the key to {}?",
            old.display()
        );
        if !confirm(&question)? {
            return Err("Not changed".into());
        }
    }
    let _keys = hold_keys(installation)?;
    // The rows first: a key beside plain rows is harmless, sealed rows without one are lost.
    let count = store.reseal(None)?;
    fs::rename(file, &old)?;
    writeln!(
        out,
        "Wrote {count} values plain. The key is in {} now, for backups of the database made before.",
        old.display()
    )?;
    Ok(())
}

/// The first line of what is read, trimmed.
fn first_line(reader: &mut dyn BufRead) -> Result<String> {
    let mut line = String::new();
    reader.read_line(&mut line)?;
    Ok(line.trim().to_owned())
}

/// Whether two targets are the same store, as far as can be told here.
fn same(one: &Target, other: &Target) -> bool {
    match (one, other) {
        (Target::Sqlite(one), Target::Sqlite(other)) => {
            one == other
                || matches!(
                    (fs::canonicalize(one), fs::canonicalize(other)),
                    (Ok(one), Ok(other)) if one == other
                )
        }
        (one, other) => one == other,
    }
}

/// `berth store copy TARGET | - | --target-file FILE [--into-existing]`:
/// every table of this store into another. It does not switch to it.
fn copy(
    paths: &Paths,
    installation: &Installation,
    args: &[&str],
    out: &mut dyn Write,
    input: &mut dyn BufRead,
) -> Result<()> {
    let (mut given, mut into_existing) = (None, false);
    let mut words = args.iter();
    while let Some(word) = words.next() {
        let target = match *word {
            "--into-existing" => {
                into_existing = true;
                continue;
            }
            "--target-file" => {
                let file = words.next().ok_or(USAGE)?;
                let mut reader = std::io::BufReader::new(
                    fs::File::open(file).map_err(|error| format!("{file}: {error}"))?,
                );
                first_line(&mut reader)?
            }
            "-" => first_line(input)?,
            word if word.starts_with('-') => return Err(USAGE.into()),
            word => word.trim().to_owned(),
        };
        if given.replace(target).is_some() {
            return Err(USAGE.into());
        }
    }
    let given = given.filter(|target| !target.is_empty()).ok_or(USAGE)?;
    let target = Target::parse(Some(&given), &store::default_file(paths));
    if same(&target, &installation.target) {
        return Err(format!(
            "The {} is the store this berth uses already",
            target.describe()
        )
        .into());
    }
    let _serve = hold_serve(paths)?;
    // Opened as every command opens it, so the files of an older berth are in it first.
    let from = store::shared(paths)?;
    let to = Store::connect_trusting(&target, installation.ca.as_deref())?;
    let report = store::copy(&from, &to, into_existing)?;
    writeln!(
        out,
        "Copied the {} into the {}:",
        installation.target.describe(),
        target.describe()
    )?;
    for copied in &report.tables {
        let kept = copied.found - copied.added;
        match kept {
            0 => writeln!(out, "  {:<9} {} rows", copied.table.name(), copied.added)?,
            kept => writeln!(
                out,
                "  {:<9} {} rows, and {kept} the target had already, which it keeps",
                copied.table.name(),
                copied.added
            )?,
        }
    }
    if report.left_out > 0 {
        writeln!(
            out,
            "Left out {} rows of what this berth knows of berth servers (scopes served:…), which the next mirror of each fills again.",
            report.left_out
        )?;
    }
    let shown = match &target {
        Target::Sqlite(path) => path.display().to_string(),
        // The URL may hold a password, which is not printed.
        Target::Postgres(_) => "URL".to_owned(),
    };
    writeln!(
        out,
        "This berth still uses the {}. To switch: berth config store {shown}, or set BERTH_STORE.",
        installation.target.describe()
    )?;
    if from.encrypted() || from.seal_report()?.sealed > 0 {
        writeln!(
            out,
            "The values are sealed with the key in {}: wherever the new store is used, that key file has to be there too (see the store_key setting).",
            installation.key_file.display()
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use berth_core::store::kind;

    struct Scratch {
        _dir: tempfile::TempDir,
        paths: Paths,
        installation: Installation,
        /// What standard input holds.
        input: String,
    }

    fn scratch() -> Scratch {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: dir.path().join("config"),
            data_dir: dir.path().join("data"),
        };
        // The settings of the files only, whatever the environment of the tests holds.
        let installation = Installation::with(&paths, &|key| paths.setting_with(key, None));
        let store = installation.open().unwrap();
        store.write(kind::SECRET, "", "api", b"hunter2").unwrap();
        store
            .write(kind::PROFILE, "", "studio", b"{\"hostname\":\"studio\"}")
            .unwrap();
        Scratch {
            _dir: dir,
            paths,
            installation,
            input: String::new(),
        }
    }

    impl Scratch {
        fn run(&self, args: &[&str]) -> Result<String> {
            self.answering(args, false)
        }

        fn answering(&self, args: &[&str], yes: bool) -> Result<String> {
            let mut out = Vec::new();
            let mut confirm = |_: &str| -> Result<bool> { Ok(yes) };
            run(
                &self.paths,
                &self.installation,
                args,
                &mut out,
                &mut confirm,
                &mut self.input.as_bytes(),
            )?;
            Ok(String::from_utf8(out).unwrap())
        }

        fn key_id(&self) -> String {
            let key = Key::read(&self.installation.key_file).unwrap().unwrap();
            seal::hex(&key.id())
        }
    }

    #[test]
    fn encrypting_rekeying_and_decrypting() {
        let s = scratch();
        let shown = s.run(&[]).unwrap();
        assert!(shown.contains("Encrypted  no"), "{shown}");
        assert!(shown.contains("0 sealed, 2 plain"), "{shown}");
        assert!(shown.contains("SQLite file"), "{shown}");

        let said = s.run(&["encrypt"]).unwrap();
        let file = s.installation.key_file.display().to_string();
        assert!(
            said.contains(&file) && said.contains("back it up"),
            "{said}"
        );
        let first = s.key_id();
        let shown = s.run(&[]).unwrap();
        assert!(shown.contains("2 sealed, 0 plain"), "{shown}");
        assert!(
            shown.contains(&format!("Encrypted  yes, with key {first}")),
            "{shown}"
        );
        assert!(
            s.run(&["encrypt"]).is_err(),
            "a key file is never overwritten"
        );
        let store = s.installation.open().unwrap();
        assert_eq!(
            store.read(kind::SECRET, "", "api").unwrap().unwrap(),
            b"hunter2"
        );
        drop(store);

        s.run(&["rekey"]).unwrap();
        let second = s.key_id();
        assert_ne!(first, second);
        let old = Key::read(&s.installation.old_key_file()).unwrap().unwrap();
        assert_eq!(seal::hex(&old.id()), first);
        let shown = s.run(&[]).unwrap();
        assert!(shown.contains(&format!("Keys used  {second}\n")), "{shown}");
        assert!(!s.installation.new_key_file().exists());

        // On a terminal the answer counts.
        assert_eq!(
            s.answering(&["decrypt"], false).unwrap_err().to_string(),
            "Not changed"
        );
        assert!(s.installation.key_file.exists());
        s.run(&["decrypt", "--yes"]).unwrap();
        assert!(!s.installation.key_file.exists());
        let old = Key::read(&s.installation.old_key_file()).unwrap().unwrap();
        assert_eq!(seal::hex(&old.id()), second);
        let shown = s.run(&[]).unwrap();
        assert!(
            shown.contains("Encrypted  no") && shown.contains("0 sealed, 2 plain"),
            "{shown}"
        );
        let store = s.installation.open().unwrap();
        assert_eq!(
            store.read(kind::SECRET, "", "api").unwrap().unwrap(),
            b"hunter2"
        );
        assert!(s.run(&["rekey"]).is_err());
        assert!(s.run(&["decrypt", "--yes"]).is_err());
        assert!(s.run(&["seal"]).is_err());
    }

    #[test]
    fn nothing_changes_while_berth_serve_runs() {
        let s = scratch();
        let _serve = hold_serve(&s.paths).unwrap();
        for args in [&["encrypt"][..], &["rekey"], &["decrypt", "--yes"]] {
            let error = s.run(args).unwrap_err().to_string();
            assert!(error.contains("stop berth serve first"), "{error}");
        }
        assert!(!s.installation.key_file.exists());
        // Looking is fine.
        assert!(s.run(&[]).is_ok());
    }

    #[test]
    fn a_copy_goes_to_another_file_and_leaves_this_one_in_use() {
        let mut s = scratch();
        s.run(&["encrypt"]).unwrap();
        let store = store::shared(&s.paths).unwrap();
        store
            .write(kind::PROFILE, "served:http://192.0.2.7:7777", "yard", b"{}")
            .unwrap();
        store
            .put("ui", "k", &serde_json::json!(1), store::When::Always)
            .unwrap();
        let target = s._dir.path().join("elsewhere/copy.db");
        let said = s.run(&["copy", target.to_str().unwrap()]).unwrap();
        assert!(said.contains("  state     "), "{said}");
        assert!(said.contains("  kv        1 rows"), "{said}");
        assert!(said.contains("Left out 1 rows"), "{said}");
        assert!(
            said.contains(&format!("berth config store {}", target.display())),
            "{said}"
        );
        assert!(said.contains("key file has to be there"), "{said}");
        // The copy opens with the same key, and the settings did not change.
        let copied = Installation {
            target: Target::Sqlite(target.clone()),
            ..s.installation.clone()
        }
        .open()
        .unwrap();
        assert_eq!(
            copied.read(kind::SECRET, "", "api").unwrap().unwrap(),
            b"hunter2"
        );
        assert!(
            copied
                .names(kind::PROFILE, "served:http://192.0.2.7:7777")
                .unwrap()
                .is_empty()
        );
        assert_eq!(s.paths.setting_with("store", None), None);

        // A target that has rows is refused, unless it is to be added to.
        let error = s
            .run(&["copy", target.to_str().unwrap()])
            .unwrap_err()
            .to_string();
        assert!(error.contains("--into-existing"), "{error}");
        let said = s
            .run(&["copy", target.to_str().unwrap(), "--into-existing"])
            .unwrap();
        assert!(said.contains("the target had already"), "{said}");

        // From a file or standard input, and never into the store in use.
        let file = s._dir.path().join("target");
        let other = s._dir.path().join("other.db");
        fs::write(&file, format!("{}\nignored\n", other.display())).unwrap();
        s.run(&["copy", "--target-file", file.to_str().unwrap()])
            .unwrap();
        assert!(other.is_file());
        s.input = format!("{}\n", s._dir.path().join("third.db").display());
        s.run(&["copy", "-"]).unwrap();
        assert!(s._dir.path().join("third.db").is_file());
        let current = store::default_file(&s.paths);
        let error = s
            .run(&["copy", current.to_str().unwrap()])
            .unwrap_err()
            .to_string();
        assert!(error.contains("uses already"), "{error}");
        assert!(s.run(&["copy"]).is_err() && s.run(&["copy", "a", "b"]).is_err());
    }

    #[test]
    fn a_copy_waits_for_berth_serve_and_keeps_a_password_to_itself() {
        let s = scratch();
        let target = s._dir.path().join("copy.db");
        let serve = hold_serve(&s.paths).unwrap();
        let error = s
            .run(&["copy", target.to_str().unwrap()])
            .unwrap_err()
            .to_string();
        assert!(error.contains("stop berth serve first"), "{error}");
        assert!(!target.exists());
        drop(serve);
        // Nothing listens on port 1, and the error does not show the password.
        let url = "postgres://berth:made-up-pw@127.0.0.1:1/berth";
        let error = s.run(&["copy", url]).unwrap_err().to_string();
        assert!(!error.contains("made-up-pw"), "{error}");
    }

    /// Directories of an installation, in a folder of its own.
    fn installation_in(dir: &std::path::Path) -> Paths {
        Paths {
            config_dir: dir.join("config"),
            data_dir: dir.join("data"),
        }
    }

    /// Point fresh directories at a store and a key file, as on a new machine.
    fn pointed_at(dir: &std::path::Path, store: &std::path::Path, key: &std::path::Path) -> Paths {
        let paths = installation_in(dir);
        paths.set("store", store.to_str().unwrap()).unwrap();
        paths.set("store_key", key.to_str().unwrap()).unwrap();
        paths
    }

    /// Everything of an installation that `restored` finds.
    fn set_up(paths: &Paths) -> String {
        use berth_core::profiles::Place;
        let installation = Installation::of(paths);
        fs::create_dir_all(&paths.config_dir).unwrap();
        Key::generate()
            .unwrap()
            .write_new(&installation.key_file)
            .unwrap();
        paths.set("default_host", "studio").unwrap();
        let registries = berth_core::registry::Store::new(paths).unwrap();
        let nexus =
            serde_json::json!({"type": "nexus", "url": "https://nexus.example", "username": "ci"});
        let mut map = serde_json::Map::new();
        map.insert("nexus".into(), nexus);
        registries.save_registries(map).unwrap();
        registries
            .set_secret("github", Some("ghp-made-up"))
            .unwrap();
        for (name, address) in [("studio", "192.0.2.10"), ("attic", "192.0.2.11")] {
            let place = Place::own(name).unwrap();
            let profile = serde_json::json!({"hostname": name, "builder": {"ssh_host": address}});
            place.save(paths, &profile).unwrap();
            berth_core::credentials::save(paths, &place, "ops", &format!("pw-of-{name}")).unwrap();
        }
        berth_core::provision::Record {
            provisioner: "cloud/vm".into(),
            id: "vm-1".into(),
            token: "t0k".into(),
            ..Default::default()
        }
        .save(paths, "studio")
        .unwrap();
        // A key of its own from files, so no ssh-keygen is needed.
        fs::create_dir_all(&paths.data_dir).unwrap();
        fs::write(paths.data_dir.join("id_ed25519"), "made-up private key\n").unwrap();
        fs::write(
            paths.data_dir.join("id_ed25519.pub"),
            "ssh-ed25519 AAAA berth@studio\n",
        )
        .unwrap();
        paths.own_key().unwrap();
        let token = crate::serve::token(paths).unwrap();
        // A plugin from an upload and one copied in as a folder.
        let own = berth_core::plugins::own_dir(&paths.config_dir);
        fs::create_dir_all(own.join("uploads")).unwrap();
        fs::write(own.join("uploads/1.tgz"), "an upload").unwrap();
        fs::write(
            own.join("package.json"),
            r#"{"dependencies": {"berth-plugin-up": "file:uploads/1.tgz"}}"#,
        )
        .unwrap();
        fs::create_dir_all(own.join("hand-made")).unwrap();
        fs::write(
            own.join("hand-made/package.json"),
            r#"{"name": "berth-plugin-local", "berth": {}}"#,
        )
        .unwrap();
        berth_core::plugin_state::remember(paths).unwrap();
        assert!(store::shared(paths).unwrap().seal_report().unwrap().plain == 0);
        token
    }

    /// Check that the installation at `paths` has all that `set_up` made.
    fn restored(paths: &Paths, token: &str, key: &[u8]) {
        use berth_core::profiles::Place;
        assert_eq!(
            paths.hosts(),
            [
                ("attic".to_owned(), "192.0.2.11".to_owned()),
                ("studio".to_owned(), "192.0.2.10".to_owned())
            ]
        );
        let studio = Place::own("studio").unwrap();
        assert_eq!(studio.load(paths).unwrap().unwrap()["hostname"], "studio");
        assert_eq!(
            berth_core::credentials::load(paths, &studio).unwrap()["ops"],
            "pw-of-studio"
        );
        let record = berth_core::provision::Record::load(paths, "studio")
            .unwrap()
            .unwrap();
        assert_eq!(record.id, "vm-1");
        let registries = berth_core::registry::Store::new(paths).unwrap();
        assert_eq!(
            registries.secret("github").unwrap().as_deref(),
            Some("ghp-made-up")
        );
        assert_eq!(registries.registries().unwrap()["nexus"]["username"], "ci");
        assert_eq!(
            paths.setting_with("default_host", None).as_deref(),
            Some("studio")
        );
        // The key and the token are written back as the files they are used as.
        let file = paths.own_key().unwrap();
        assert_eq!(file, paths.data_dir.join("id_ed25519"));
        assert_eq!(fs::read(&file).unwrap(), key);
        assert_eq!(crate::serve::token(paths).unwrap(), token);
        assert_eq!(
            fs::read_to_string(paths.data_dir.join("serve.token"))
                .unwrap()
                .trim(),
            token
        );
        // The plugins: package.json and the upload are written, the folder unpacked.
        let own = berth_core::plugins::own_dir(&paths.config_dir);
        let store = store::shared(paths).unwrap();
        let plan = berth_core::plugin_state::plan(&own, &store)
            .unwrap()
            .unwrap();
        let files: Vec<_> = plan
            .files
            .iter()
            .map(|(file, _)| file.display().to_string())
            .collect();
        assert_eq!(files, ["package.json", "uploads/1.tgz"]);
        let folders: Vec<_> = plan.folders.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(folders, ["local"]);
        assert_eq!(plan.install, berth_core::plugin_state::Install::All);
        assert!(plan.left_out.is_empty());
    }

    #[test]
    fn an_installation_comes_back_from_its_database_and_key() {
        let dir = tempfile::tempdir().unwrap();
        let first = installation_in(&dir.path().join("a"));
        let token = set_up(&first);
        let key = fs::read(first.data_dir.join("id_ed25519")).unwrap();
        let database = store::default_file(&first);
        let key_copy = dir.path().join("kept/store.key");
        fs::create_dir_all(key_copy.parent().unwrap()).unwrap();
        fs::copy(Installation::of(&first).key_file, &key_copy).unwrap();

        // A machine with nothing but the database and the key file.
        let second = pointed_at(&dir.path().join("b"), &database, &key_copy);
        restored(&second, &token, &key);
        assert!(!second.data_dir.join("berth.db").exists());

        // Without the key file, what is sealed does not open, and the error names the file.
        let missing = dir.path().join("nowhere/store.key");
        let keyless = pointed_at(&dir.path().join("d"), &database, &missing);
        let error = berth_core::registry::Store::new(&keyless)
            .unwrap()
            .secret("github")
            .unwrap_err()
            .to_string();
        assert!(error.contains(&missing.display().to_string()), "{error}");

        // The same through a copy into another file.
        let copied = dir.path().join("moved/berth.db");
        let mut out = Vec::new();
        run(
            &first,
            &Installation::of(&first),
            &["copy", copied.to_str().unwrap()],
            &mut out,
            &mut |_: &str| -> Result<bool> { Ok(false) },
            &mut &b""[..],
        )
        .unwrap();
        let third = pointed_at(&dir.path().join("c"), &copied, &key_copy);
        restored(&third, &token, &key);
    }
}
