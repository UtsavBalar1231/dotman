//! Names that hold terminal control sequences reach the terminal escaped, on stderr and in `list`,
//! while `list --porcelain` keeps them raw for scripts.

mod common;

use common::TestEnv;
use std::process::Output;

const OSC: &str = "d/\u{1b}]0;PWNED\u{7}\u{1b}[31mred\u{1b}[0m";
const OSC_SHOWN: &str = "d/\\u{1b}]0;PWNED\\u{7}\\u{1b}[31mred\\u{1b}[0m";
const CR: &str = "d/cr\rspoof";
const CR_SHOWN: &str = "d/cr\\rspoof";
const NL: &str = "d/nl\nforged";
const NL_SHOWN: &str = "d/nl\\nforged";

fn raw(bytes: &[u8]) -> bool {
    bytes.iter().any(|b| *b == 0x1b || *b == b'\r')
}

fn ok(out: &Output) -> (String, String) {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn names_with_control_sequences_are_shown_escaped() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    for name in [OSC, CR, NL] {
        env.write(name, b"x\n", 0o644);
    }
    let out = env.run(&["add", "--yes", "d"]);
    assert!(
        !raw(&out.stderr),
        "{:?}",
        String::from_utf8_lossy(&out.stderr)
    );

    let out = env.run(&["list"]);
    let (listed, _) = ok(&out);
    assert!(!raw(&out.stdout), "{listed:?}");
    // A dotman that refuses control characters in names manages none of these.
    if !listed.contains("d/") {
        return;
    }
    for shown in [OSC_SHOWN, CR_SHOWN, NL_SHOWN] {
        assert!(listed.contains(shown), "{listed}");
    }
    assert!(
        !listed.lines().any(|line| line.starts_with("forged")),
        "a newline in a key never starts a line: {listed}"
    );

    let out = env.run(&["list", "--porcelain"]);
    ok(&out);
    let records = String::from_utf8_lossy(&out.stdout);
    assert!(records.contains(&format!("file\t{OSC}\0")), "{records:?}");

    for name in [OSC, CR, NL] {
        env.write_repo(name, b"x2\n");
    }
    let out = env.run(&["apply"]);
    let (_, err) = ok(&out);
    assert!(!raw(&out.stderr), "{err:?}");
    for shown in [OSC_SHOWN, CR_SHOWN, NL_SHOWN] {
        assert!(err.contains(shown), "{err}");
    }
}
