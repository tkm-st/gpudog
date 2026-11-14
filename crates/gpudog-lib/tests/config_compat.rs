//! Config compatibility: recorded results, plus hand-written cases.
//!
//! `golden/config_cases.txt` holds TOML fragments, and
//! `config_recorded_results.txt` what an earlier, independent loader made of
//! each. The record is frozen and cannot be regenerated: producing it with
//! this loader would test the loader against itself. It omits the
//! `min_priority` and `max_priority` values, which the daemon no longer has.
//! Every case must produce the same warnings and the same resulting values,
//! or the same error location. The only intended
//! divergences are listed in `DIVERGENCES`.
//!
//! New cases go in `config_cases_rust.txt`, with expectations written by
//! hand from the documented rules in `config_rust_results.txt`. The cases
//! there that tested parsing through a priority key now use
//! `max_queued_per_uid` instead.

use std::collections::BTreeMap;

use gpudog_lib::config::DaemonConfig;

/// Cases where the loader deliberately differs from the recorded result,
/// with the reason. Each is also asserted to still differ, so this list
/// cannot go stale.
const DIVERGENCES: &[(&str, &str)] = &[
    ("mode_leading_space", "strtoul skipped leading whitespace"),
    ("mode_plus", "strtoul accepted a sign"),
    (
        "mode_minus_zero",
        "strtoul accepted a sign, so \"-0\" meant 0",
    ),
    (
        "nul_in_string",
        "the recorded loader truncated the path at the NUL; it is now kept and validate() refuses it",
    ),
];

fn cases(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut name: Option<String> = None;
    let mut body = String::new();
    for line in text.split_inclusive('\n') {
        if let Some(next) = line.strip_prefix("=== ") {
            if let Some(done) = name.take() {
                out.insert(done, std::mem::take(&mut body));
            }
            name = Some(next.trim_end().to_owned());
        } else {
            body.push_str(line);
        }
    }
    if let Some(done) = name {
        out.insert(done, body);
    }
    out
}

/// Formats a result in the recorded format, minus `log_directory`
/// (see [`normalise`]).
fn run(text: &str) -> String {
    let mut config = DaemonConfig::default();
    match config.apply_text(text, "<config>") {
        Err(error) => {
            let at = error.0.rfind("(line ").map_or("?", |i| &error.0[i..]);
            format!("error {at}\n")
        }
        Ok(warnings) => {
            let mut out: String = warnings.iter().map(|w| format!("warning {w}\n")).collect();
            let s = &config.service;
            out += &format!(
                "value socket_path={} socket_mode={:o} require_secure_directory={} \
                 max_connections={} max_connections_per_uid={} handshake_timeout_ms={} \
                 max_held_per_uid={} max_queued_per_uid={}\n",
                s.socket_path.display(),
                s.socket_mode,
                u8::from(s.require_secure_directory),
                s.max_connections,
                s.max_connections_per_uid,
                s.handshake_timeout.as_millis(),
                s.scheduler_limits.max_held_per_uid,
                s.scheduler_limits.max_queued_per_uid,
            );
            out
        }
    }
}

/// Drops everything about `log_directory`, which is obsolete: the recorded
/// value field and warning, and the obsolete-key warning. The
/// rest of each case must still match exactly.
fn normalise(result: &str) -> String {
    result
        .split_inclusive('\n')
        .filter(|line| !(line.starts_with("warning ") && line.contains("'log_directory'")))
        .map(|line| match line.strip_prefix("value log_directory=") {
            Some(rest) => format!(
                "value {}",
                rest.split_once(' ').map_or("", |(_, tail)| tail)
            ),
            None => line.to_owned(),
        })
        .collect()
}

#[test]
fn loader_matches_the_recorded_results_on_every_case() {
    let inputs = cases(include_str!("golden/config_cases.txt"));
    let expected = cases(include_str!("golden/config_recorded_results.txt"));
    assert_eq!(
        inputs.keys().collect::<Vec<_>>(),
        expected.keys().collect::<Vec<_>>(),
        "the record is frozen: add new cases to config_cases_rust.txt instead"
    );

    let mut mismatches = Vec::new();
    for (name, input) in &inputs {
        let actual = run(input);
        // Wherever the input sets it, the loader must say it is obsolete.
        let mentions = input
            .lines()
            .any(|line| line.trim_start().starts_with("log_directory"));
        let warned = actual.contains("ignoring 'log_directory': gpudogd now logs to stderr");
        if mentions && !actual.starts_with("error") && !warned {
            mismatches.push(format!(
                "--- {name}: log_directory set but no obsolete warning"
            ));
        }
        let (actual, expected) = (normalise(&actual), normalise(&expected[name]));
        let diverges = DIVERGENCES.iter().any(|(case, _)| case == name);
        if (actual == expected) == diverges {
            mismatches.push(format!(
                "--- {name}{}\nexpected: {}actual:   {}",
                if diverges {
                    " (listed as a divergence, but now matches)"
                } else {
                    ""
                },
                expected,
                actual
            ));
        }
    }
    assert!(mismatches.is_empty(), "\n{}", mismatches.join("\n"));
}

#[test]
fn rust_matches_the_hand_written_expectations() {
    let inputs = cases(include_str!("golden/config_cases_rust.txt"));
    let expected = cases(include_str!("golden/config_rust_results.txt"));
    assert_eq!(
        inputs.keys().collect::<Vec<_>>(),
        expected.keys().collect::<Vec<_>>(),
        "every case needs an expectation"
    );
    let mismatches: Vec<String> = inputs
        .iter()
        .filter_map(|(name, input)| {
            let actual = run(input);
            (actual != expected[name])
                .then(|| format!("--- {name}\nexpected: {}actual:   {actual}", expected[name]))
        })
        .collect();
    assert!(mismatches.is_empty(), "\n{}", mismatches.join("\n"));
}
