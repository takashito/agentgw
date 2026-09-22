//! **This repository is public.** Nothing that describes the maintainer's own network belongs in it.
//!
//! The rule is easy to break by accident: while chasing something on a real machine you paste the
//! output you just captured into a doc comment or a test, and a hostname, a LAN address or an ssh
//! target rides along. That is how 78 of them got in before this check existed.
//!
//! Examples use the ranges reserved for them: `192.0.2.x` / `198.51.100.x` / `203.0.113.x`
//! (RFC 5737 — never routed), `example.com`, `100.64.0.x` for a tailnet, and made-up machine names
//! like `build-box` or `hub`.

use std::path::Path;

/// Addresses that are fine to write down: loopback, "any", and the ranges reserved for documentation.
fn allowed(a: [u8; 4]) -> bool {
    matches!(a, [127, ..] | [0, 0, 0, 0])
        || a[..3] == [192, 0, 2]
        || a[..3] == [198, 51, 100]
        || a[..3] == [203, 0, 113]
        // The first addresses of the tailnet range, kept for examples
        || a[..3] == [100, 64, 0]
}

/// Addresses that only exist on somebody's actual network.
fn someones_network(a: [u8; 4]) -> Option<&'static str> {
    match a {
        [10, ..] => Some("a private address (10/8)"),
        [192, 168, ..] => Some("a private address (192.168/16)"),
        [172, b, ..] if (16..=31).contains(&b) => Some("a private address (172.16/12)"),
        [100, b, ..] if (64..=127).contains(&b) => Some("a tailnet address (100.64/10)"),
        _ => None,
    }
}

/// Every IPv4-looking token in a line.
fn addresses(line: &str) -> Vec<[u8; 4]> {
    let mut out = Vec::new();
    for token in line.split(|c: char| !(c.is_ascii_digit() || c == '.')) {
        let parts: Vec<&str> = token.split('.').collect();
        if parts.len() != 4 {
            continue;
        }
        let octets: Option<Vec<u8>> = parts.iter().map(|p| p.parse::<u8>().ok()).collect();
        if let Some(o) = octets {
            out.push([o[0], o[1], o[2], o[3]]);
        }
    }
    out
}

/// What else gives a place away. Assembled at run time so this file does not trip its own check.
fn telltale(line: &str) -> Option<String> {
    if line.contains(&["root", "@"].concat()) {
        return Some("an ssh target with a real account — write user@host".into());
    }
    // A tailnet name is the network's own name. An example has to look like one
    if line.contains(&[".ts", ".net"].concat()) && !line.contains("example") && !line.contains("tail1234")
    {
        return Some("a tailnet name — use something obviously made up".into());
    }
    None
}

fn files_that_ship(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            files_that_ship(&p, out);
        } else if p
            .extension()
            .is_some_and(|x| x == "rs" || x == "sh" || x == "md" || x == "toml")
            && p.file_name().is_some_and(|n| n != "no_local_details.rs")
        {
            out.push(p);
        }
    }
}

/// The published repository must not describe the network it was written on.
#[test]
fn nothing_in_here_names_the_maintainers_network() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    for dir in ["src", "scripts", "tests"] {
        files_that_ship(&root.join(dir), &mut files);
    }
    for f in ["README.md", "README.ja.md", "Cargo.toml"] {
        files.push(root.join(f));
    }

    let mut found = Vec::new();
    for file in files {
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        let at = |n: usize| {
            format!(
                "{}:{}",
                file.strip_prefix(root).unwrap_or(&file).display(),
                n + 1
            )
        };
        for (n, line) in text.lines().enumerate() {
            for a in addresses(line) {
                if !allowed(a)
                    && let Some(why) = someones_network(a)
                {
                    found.push(format!("{}: {why}", at(n)));
                }
            }
            if let Some(why) = telltale(line) {
                found.push(format!("{}: {why}", at(n)));
            }
        }
    }
    assert!(
        found.is_empty(),
        "this repository is public:\n  {}",
        found.join("\n  ")
    );
}
