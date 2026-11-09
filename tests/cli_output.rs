//! Names that hold terminal control sequences reach the terminal escaped, on stderr and in `list`,
//! while `list --porcelain` keeps them raw for scripts.

mod common;

use common::TestEnv;
use std::process::Output;

const OSC: &str = "d/\u{1b}]0;PWNED\u{7}\u{1b}[31mred\u{1b}[0m";
const OSC_SHOWN: &str = "d/\\u{1b}]0;PWNED\\u{7}\\u{1b}[31mred\\u{1b}[0m";
const CR: &str = "d/cr\rspoof";
const CR_SHOWN: &str = "d/cr\\rspoof";

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
    env.write(OSC, b"x\n", 0o644);
    env.write(CR, b"y\n", 0o644);
    let out = env.run(&["add", "--yes", "d"]);
    ok(&out);
    assert!(
        !raw(&out.stderr),
        "{:?}",
        String::from_utf8_lossy(&out.stderr)
    );

    let out = env.run(&["list"]);
    let (listed, _) = ok(&out);
    assert!(!raw(&out.stdout), "{listed:?}");
    assert!(
        listed.contains(OSC_SHOWN) && listed.contains(CR_SHOWN),
        "{listed}"
    );

    let out = env.run(&["list", "--porcelain"]);
    ok(&out);
    let records = String::from_utf8_lossy(&out.stdout);
    assert!(records.contains(&format!("file\t{OSC}\0")), "{records:?}");

    env.write_repo(OSC, b"x2\n");
    env.write_repo(CR, b"y2\n");
    let out = env.run(&["apply"]);
    let (_, err) = ok(&out);
    assert!(!raw(&out.stderr), "{err:?}");
    assert!(err.contains(OSC_SHOWN) && err.contains(CR_SHOWN), "{err}");
}
