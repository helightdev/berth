//! What berth passes on of a machine it waits for: the latest lines of its
//! console, and the progress of its first boot. Each line is cleaned of what
//! a terminal would act on, kept short, and has secrets hidden before it is
//! shown among berth's own lines.

use std::{collections::VecDeque, iter::Peekable, str::Chars};

/// How a passed-on line starts, so it stands apart from berth's own.
pub const PREFIX: &str = "  │ ";
/// The longest a passed-on line is, in characters.
const WIDTH: usize = 160;
/// The most lines passed on from one look at the machine.
pub const AT_ONCE: usize = 6;
/// How many of the first boot's last lines are kept, to show when it fails.
const KEPT: usize = 15;
/// What the first boot's own steps start with in its log.
const STEP: &str = "==> ";

/// The line as a terminal would end up showing it, without escape sequences
/// or control characters, and shortened to `WIDTH`.
pub fn clean(line: &str) -> String {
    let line = line.trim_end_matches(['\r', '\n']);
    // A carriage return draws over the line, so what follows the last one is what shows.
    let line = line.rsplit('\r').next().unwrap_or("");
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\u{1b}' => skip_escape(&mut chars),
            '\t' => out.push(' '),
            // Marks that reorder or hide text would make a line read as another.
            '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' => {}
            '\u{feff}' => {}
            ch if ch.is_control() => {}
            ch => out.push(ch),
        }
    }
    let out = out.trim();
    match out.chars().count() > WIDTH {
        true => out.chars().take(WIDTH - 1).chain(['…']).collect(),
        false => out.to_owned(),
    }
}

fn skip_escape(chars: &mut Peekable<Chars>) {
    match chars.next() {
        // A control sequence: parameters, then one final character from @ to ~.
        Some('[') => {
            for ch in chars.by_ref() {
                if ('@'..='~').contains(&ch) {
                    break;
                }
            }
        }
        // A string such as a window title, ended by BEL or ESC \.
        Some(']' | 'P' | '_' | '^' | 'X') => {
            while let Some(ch) = chars.next() {
                if ch == '\u{7}' {
                    break;
                }
                if ch == '\u{1b}' {
                    chars.next_if_eq(&'\\');
                    break;
                }
            }
        }
        _ => {}
    }
}

/// The line with each of `hidden` and anything shaped like a tailnet key or
/// a password hash replaced by `[secret]`.
pub fn scrub(line: String, hidden: &[String]) -> String {
    let mut line = hidden
        .iter()
        .filter(|text| !text.is_empty())
        .fold(line, |line, text| line.replace(text.as_str(), "[secret]"));
    for marker in ["tskey-", "hskey-", "$6$", "$5$", "$y$", "$2b$", "$2y$"] {
        line = hide_from(&line, marker);
    }
    line
}

/// Replace each word that starts with `marker`, up to a space or a quote.
fn hide_from(line: &str, marker: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(at) = rest.find(marker) {
        out.push_str(&rest[..at]);
        out.push_str("[secret]");
        let word = &rest[at..];
        let end = word
            .find(|ch: char| ch.is_whitespace() || "\"'`,;)]}".contains(ch))
            .unwrap_or(word.len());
        rest = &word[end..];
    }
    out.push_str(rest);
    out
}

/// No more than `AT_ONCE` lines, the latest, with a note of how many were left out.
pub fn capped(mut lines: Vec<String>) -> Vec<String> {
    if lines.len() <= AT_ONCE {
        return lines;
    }
    let left = lines.len() - (AT_ONCE - 1);
    let mut kept = vec![format!("… {left} lines left out")];
    kept.extend(lines.drain(left..));
    kept
}

/// The latest lines of something that only grows, such as a console log, as
/// they are asked for again and again.
#[derive(Default)]
pub struct Tail {
    last: Vec<String>,
}

impl Tail {
    /// The lines of `latest` that come after the ones seen last time. The two
    /// overlap where the end of the last ones is the start of these.
    pub fn fresh(&mut self, latest: Vec<String>) -> Vec<String> {
        let most = self.last.len().min(latest.len());
        let overlap = (1..=most)
            .rev()
            .find(|&count| self.last[self.last.len() - count..] == latest[..count])
            .unwrap_or(0);
        let fresh = latest[overlap..].to_vec();
        self.last = latest;
        fresh
    }
}

/// What has been seen of a seeded machine's first boot: its provision state
/// and the log of the service that runs it.
#[derive(Default)]
pub struct FirstBoot {
    /// Where in the log the next look goes on from.
    pub cursor: String,
    state: String,
    tasks: usize,
    task: String,
    kept: VecDeque<String>,
}

impl FirstBoot {
    /// Take what a look at the machine printed: the provision state on the
    /// first line, then new lines of the log and the cursor after them. The
    /// answer is what to say of it: a changed state, the first boot's own
    /// steps, and the Ansible task it is at.
    pub fn take(&mut self, printed: &str, hidden: &[String]) -> Vec<String> {
        let mut lines = printed.lines();
        let state = lines
            .next()
            .and_then(|line| line.strip_prefix("state "))
            .map(clean)
            .unwrap_or_default();
        let mut said = vec![];
        if !state.is_empty() && state != "none" && state != self.state {
            said.push(format!("first boot: {state}"));
            self.state = state;
        }
        let (mut steps, mut task) = (vec![], None);
        for line in lines {
            if let Some(cursor) = line.strip_prefix("-- cursor: ") {
                if valid_cursor(cursor) {
                    self.cursor = cursor.to_owned();
                }
                continue;
            }
            let line = scrub(clean(line), hidden);
            if line.is_empty() {
                continue;
            }
            if self.kept.len() == KEPT {
                self.kept.pop_front();
            }
            self.kept.push_back(line.clone());
            if let Some(name) = ansible_task(&line) {
                self.tasks += 1;
                task = Some(name);
            } else if let Some(step) = line.strip_prefix(STEP) {
                steps.push(step.to_owned());
            }
        }
        let skip = steps.len().saturating_sub(3);
        said.extend(steps.into_iter().skip(skip));
        if let Some(task) = task.filter(|task| *task != self.task) {
            said.push(format!("Ansible task {}: {task}", self.tasks));
            self.task = task;
        }
        said
    }

    /// Whether the first boot says it failed.
    pub fn failed(&self) -> bool {
        self.state == "failed"
    }

    /// The last lines of its log that were seen.
    pub fn last_lines(&self) -> Vec<String> {
        self.kept.iter().cloned().collect()
    }
}

/// A journal cursor, which goes back to the machine on a command line.
pub fn valid_cursor(cursor: &str) -> bool {
    !cursor.is_empty()
        && cursor.len() < 512
        && cursor
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '=' || ch == ';')
}

/// The name of the task in a line where Ansible starts one, without its role.
fn ansible_task(line: &str) -> Option<String> {
    let rest = line
        .strip_prefix("TASK [")
        .or_else(|| line.strip_prefix("RUNNING HANDLER ["))?;
    let name = &rest[..rest.rfind(']')?];
    let name = match name.split_once(" : ") {
        Some((role, task)) if !role.contains(' ') => task,
        _ => name,
    };
    Some(name.trim().to_owned()).filter(|name| !name.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_is_what_a_terminal_would_show_and_no_longer_than_a_row() {
        assert_eq!(
            clean("\u{1b}[0;32mok: [localhost]\u{1b}[0m\r\n"),
            "ok: [localhost]"
        );
        assert_eq!(clean("\u{1b}]0;title\u{7}after"), "after");
        assert_eq!(clean("\u{1b}]8;;https://x\u{1b}\\link"), "link");
        assert_eq!(clean("10%\r50%\r100% done"), "100% done");
        assert_eq!(clean("a\tb\u{7}c\u{202e}d\u{9b}e"), "a bcde");
        let long = "x".repeat(400);
        let shown = clean(&long);
        assert_eq!(shown.chars().count(), WIDTH);
        assert!(shown.ends_with('…'));
    }

    #[test]
    fn secrets_and_what_looks_like_one_are_hidden() {
        let hidden = vec!["hunter2-but-longer".to_owned()];
        assert_eq!(
            scrub("token hunter2-but-longer used".into(), &hidden),
            "token [secret] used"
        );
        assert_eq!(
            scrub("--authkey=tskey-auth-k1234-ABCD up".into(), &[]),
            "--authkey=[secret] up"
        );
        assert_eq!(
            scrub(r#"{"passwd": "$6$salt$abcdef/123.", "x": 1}"#.into(), &[]),
            r#"{"passwd": "[secret]", "x": 1}"#
        );
        assert_eq!(scrub("costs $6 a month".into(), &[]), "costs $6 a month");
    }

    #[test]
    fn only_new_lines_of_a_growing_log_are_fresh() {
        let lines = |range: std::ops::Range<usize>| -> Vec<String> {
            range.map(|n| format!("line {n}")).collect()
        };
        let mut tail = Tail::default();
        assert_eq!(tail.fresh(lines(0..3)), lines(0..3));
        assert_eq!(tail.fresh(lines(0..3)), Vec::<String>::new());
        assert_eq!(tail.fresh(lines(0..5)), lines(3..5));
        // A window of the latest lines that moved on.
        assert_eq!(tail.fresh(lines(2..7)), lines(5..7));
        // Nothing in common: all of it is new.
        assert_eq!(tail.fresh(lines(20..22)), lines(20..22));
    }

    #[test]
    fn many_lines_are_cut_to_the_latest_with_a_note() {
        let lines: Vec<String> = (0..10).map(|n| n.to_string()).collect();
        assert_eq!(
            capped(lines),
            ["… 5 lines left out", "5", "6", "7", "8", "9"]
        );
        assert_eq!(capped(vec!["a".into()]), ["a"]);
    }

    #[test]
    fn the_first_boot_is_told_by_its_state_steps_and_ansible_task() {
        let mut boot = FirstBoot::default();
        let said = boot.take(
            "state provisioning\n==> Installing the packages the first boot needs\nGet:1 http://archive.ubuntu.com noble InRelease\n==> Installing Ansible\n-- cursor: s=ab12;i=3f\n",
            &[],
        );
        assert_eq!(
            said,
            [
                "first boot: provisioning",
                "Installing the packages the first boot needs",
                "Installing Ansible"
            ]
        );
        assert_eq!(boot.cursor, "s=ab12;i=3f");
        let said = boot.take(
            "state provisioning\n==> Applying the role\nTASK [devbox : Check the profile] ****\nok: [localhost]\nTASK [devbox : Install the base packages] ***\nchanged: [localhost]\n-- cursor: s=ab12;i=40\n",
            &[],
        );
        assert_eq!(
            said,
            [
                "Applying the role",
                "Ansible task 2: Install the base packages"
            ]
        );
        // Nothing new is nothing said, and a bad cursor is not taken.
        assert!(
            boot.take("state provisioning\n-- cursor: $(reboot)\n", &[])
                .is_empty()
        );
        assert_eq!(boot.cursor, "s=ab12;i=40");
        assert!(!boot.failed());
        let said = boot.take(
            "state failed\nTASK [devbox : Join the tailnet] ***\nfatal: [localhost]: FAILED! => {\"msg\": \"tskey-auth-xyz rejected\"}\n",
            &[],
        );
        assert_eq!(
            said,
            ["first boot: failed", "Ansible task 3: Join the tailnet"]
        );
        assert!(boot.failed());
        let last = boot.last_lines();
        assert_eq!(
            last.last().unwrap(),
            "fatal: [localhost]: FAILED! => {\"msg\": \"[secret] rejected\"}"
        );
        assert_eq!(
            last.first().unwrap(),
            "==> Installing the packages the first boot needs"
        );
    }

    #[test]
    fn a_first_boot_that_has_not_started_says_nothing() {
        let mut boot = FirstBoot::default();
        assert!(boot.take("state none\n", &[]).is_empty());
        assert!(boot.take("", &[]).is_empty());
    }
}
