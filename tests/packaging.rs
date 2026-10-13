//! What ships besides the binary: the systemd units, which must run cachy the
//! way the README documents it.

use std::fs;

/// The config path that the README documents and the shipped service uses.
const CONFIG_PATH: &str = "/etc/cachy/cachy.yml";

fn shipped(path: &str) -> String {
    fs::read_to_string(format!("{}/{path}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

/// The values of `key` in the `[section]` of a systemd unit file.
fn unit_values<'a>(unit: &'a str, section: &str, key: &str) -> Vec<&'a str> {
    let mut in_section = false;
    let mut values = Vec::new();
    for line in unit.lines().map(str::trim) {
        if line.starts_with('[') {
            in_section = line == format!("[{section}]");
        } else if in_section
            && let Some((k, v)) = line.split_once('=')
            && k.trim() == key
        {
            values.push(v.trim());
        }
    }
    values
}

#[test]
fn service_runs_cachy_once_with_the_documented_config() {
    let service = shipped("packaging/systemd/cachy.service");

    assert_eq!(unit_values(&service, "Service", "Type"), ["oneshot"]);
    assert_eq!(
        unit_values(&service, "Service", "ExecStart"),
        [format!("/usr/local/bin/cachy run --config {CONFIG_PATH}")]
    );
    assert!(shipped("README.md").contains(CONFIG_PATH));
}

#[test]
fn service_sets_up_the_default_state_dir() {
    let service = shipped("packaging/systemd/cachy.service");

    // systemd creates /var/lib/<StateDirectory>, the default state_dir.
    assert_eq!(
        unit_values(&service, "Service", "StateDirectory"),
        ["cachy"]
    );
}

#[test]
fn timer_starts_the_service_on_a_schedule_once_enabled() {
    let timer = shipped("packaging/systemd/cachy.timer");

    // A timer starts the service of its own name unless it sets Unit=.
    assert!(unit_values(&timer, "Timer", "Unit").is_empty());
    assert!(!unit_values(&timer, "Timer", "OnCalendar").is_empty());
    assert_eq!(
        unit_values(&timer, "Install", "WantedBy"),
        ["timers.target"]
    );
}

/// The keys that `cachy.example.yml` sets or shows commented out, such as
/// `max_age` and the run hook's `timeout`.
fn example_keys() -> Vec<String> {
    let mut keys = Vec::new();
    for line in shipped("cachy.example.yml").lines() {
        let line = line.strip_prefix('#').unwrap_or(line).trim_start();
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        let is_key = !key.is_empty() && key.bytes().all(|b| b.is_ascii_lowercase() || b == b'_');
        if is_key && (rest.is_empty() || rest.starts_with(' ')) && !keys.iter().any(|k| k == key) {
            keys.push(key.to_owned());
        }
    }
    keys
}

#[test]
fn readme_documents_every_config_key() {
    let readme = shipped("README.md");
    let keys = example_keys();
    // A known sample, so that a broken key parser cannot pass vacuously.
    for key in [
        "cache_branch",
        "state_dir",
        "max_age",
        "run_hook",
        "timeout",
    ] {
        assert!(keys.iter().any(|k| k == key), "{key} not among {keys:?}");
    }

    let undocumented: Vec<_> = keys
        .iter()
        .filter(|key| !readme.contains(&format!("`{key}`")))
        .collect();
    assert!(undocumented.is_empty(), "README lacks {undocumented:?}");
}

#[test]
fn readme_documents_the_exit_codes() {
    let readme = shipped("README.md");
    let section = readme
        .split("\n## Exit codes\n")
        .nth(1)
        .expect("README has an Exit codes section");
    let section = section.split("\n## ").next().unwrap();

    for code in ["`0`", "`1`", "`2`"] {
        assert!(section.contains(code), "Exit codes section lacks {code}");
    }
}
