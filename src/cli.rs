//! The options each subcommand takes, checked **before** the subcommand runs.
//!
//! Most subcommands used to ignore arguments they didn't know, so `agentgw install --help`
//! went straight on to install, and `uninstall --help` removed the service. Here
//! `--help` / `-h` print the subcommand's usage and stop, and an option the subcommand
//! doesn't take stops with an error before anything is done.

/// What `main` should do with a subcommand's arguments.
#[derive(Debug, PartialEq, Eq)]
pub enum Check {
    /// Go on and run the subcommand
    Run,
    /// Print this to stdout and exit 0 (`--help`)
    Help(String),
    /// Print this to stderr and exit 2 (an option the subcommand doesn't take)
    Refuse(String),
}

/// One subcommand: the options it takes and its usage.
struct Spec {
    /// Options that are a flag on their own
    flags: &'static [&'static str],
    /// Options followed by a value (`--name x` or `--name=x`)
    valued: &'static [&'static str],
}

fn spec(cmd: &str) -> Option<Spec> {
    let (flags, valued): (&[&str], &[&str]) = match cmd {
        "serve" | "start" | "restart" | "shutdown" | "uninstall" | "update" => (&[], &[]),
        "install" => (&["--no-restart-hint"], &[]),
        "status" => (&["--raw"], &[]),
        "supervise" => (&[], &["--state-dir"]),
        // `-` is not an option: it means "read the connection string from standard input"
        "link" => (&["-"], &["--name"]),
        "add-machine" => (
            &["-p", "--password", "-n", "--new", "--print"],
            &["--name", "--from", "-i", "--identity"],
        ),
        "route" | "unroute" | "remove-machine" => (&["--yes", "-y"], &[]),
        _ => return None,
    };
    Some(Spec { flags, valued })
}

fn usage(cmd: &str) -> String {
    match cmd {
        "install" => crate::t!(
            "Usage: agentgw install\n  \
             Set up this machine (as the gateway or as a machine), write the service and start it.\n  \
             Asks for what it needs; takes no options.",
            "使い方: agentgw install\n  \
             このマシンを設定し(ゲートウェイかマシンかを訊きます)、サービスを書いて起動します。\n  \
             必要なことは対話で訊きます。オプションはありません。"
        ),
        "uninstall" => crate::t!(
            "Usage: agentgw uninstall\n  \
             Stop the service and remove its definition. Tokens and the state directory are kept.",
            "使い方: agentgw uninstall\n  \
             サービスを止めて定義を消します。トークンと状態ディレクトリは残します。"
        ),
        "start" => crate::t!("Usage: agentgw start\n  Start the service.", "使い方: agentgw start\n  サービスを起動します。"),
        "restart" => crate::t!(
            "Usage: agentgw restart\n  Restart the service. Running agents keep running.",
            "使い方: agentgw restart\n  サービスを再起動します。動いているエージェントはそのまま残ります。"
        ),
        "shutdown" => crate::t!(
            "Usage: agentgw shutdown\n  Stop the service and close its agents.",
            "使い方: agentgw shutdown\n  サービスを止め、エージェントも閉じます。"
        ),
        "status" => crate::t!(
            "Usage: agentgw status [--raw]\n  \
             Show the service, this machine's role and (on the gateway) every machine.\n  \
             --raw   the service manager's full output",
            "使い方: agentgw status [--raw]\n  \
             サービスの状態、このマシンの役割、(ゲートウェイなら)各マシンを出します。\n  \
             --raw   サービス管理の出力をそのまま出す"
        ),
        "update" => crate::t!(
            "Usage: agentgw update [<version> | local]\n  \
             Update the gateway, then every machine one at a time.\n  \
             <version>   a named release such as v0.75.0 (the only way to go down)\n  \
             local       this folder's `cargo dist` build instead of a release",
            "使い方: agentgw update [<版> | local]\n  \
             ゲートウェイ、次に各マシンを1台ずつ上げます。\n  \
             <版>    v0.75.0 のように名指しした release(下げられるのはこのときだけ)\n  \
             local   release の代わりに、このフォルダで cargo dist した物"
        ),
        "link" => crate::t!(
            "Usage: agentgw link [--name <name>] [<connection string> | -]\n  \
             Put a connection string from the gateway into this machine's settings.\n  \
             -   read it from standard input",
            "使い方: agentgw link [--name <名前>] [<接続文字列> | -]\n  \
             ゲートウェイの接続文字列をこのマシンの設定に書きます。\n  \
             -   標準入力から読む"
        ),
        "add-machine" => crate::t!(
            "Usage: agentgw add-machine <ssh destination> [-i <key>] [-p] [-n] [--name <name>] [--from <binary>]\n       \
             agentgw add-machine --print <name>\n  \
             Install agentgw on another machine and connect it to this gateway.\n  \
             -i, --identity <key>   ssh key\n  \
             -p, --password         ask for the ssh password\n  \
             -n, --new              work the route out again\n  \
             --name <name>          the machine's name\n  \
             --from <binary>        send this binary instead of the release\n  \
             --print                only print the connection string (for Windows)",
            "使い方: agentgw add-machine <ssh先> [-i <鍵>] [-p] [-n] [--name <名前>] [--from <バイナリ>]\n        \
             agentgw add-machine --print <名前>\n  \
             ほかのマシンに agentgw を入れて、このゲートウェイにつなぎます。\n  \
             -i, --identity <鍵>    ssh の鍵\n  \
             -p, --password         ssh のパスワードを訊く\n  \
             -n, --new              経路を測り直す\n  \
             --name <名前>          マシンの名前\n  \
             --from <バイナリ>      release の代わりにこのバイナリを送る\n  \
             --print                接続文字列を出すだけ(Windows 用)"
        ),
        "route" | "unroute" | "remove-machine" => crate::bridge::gateway::admin::usage(cmd),
        "serve" => crate::t!(
            "Usage: agentgw serve\n  What the service runs; you don't need to type it.",
            "使い方: agentgw serve\n  サービスが呼ぶ本体です(手で打つ必要はありません)。"
        ),
        "supervise" => crate::t!(
            "Usage: agentgw supervise [--state-dir <dir>]\n  What the Windows scheduled task runs; you don't need to type it.",
            "使い方: agentgw supervise [--state-dir <ディレクトリ>]\n  Windows のタスクが呼ぶ物です(手で打つ必要はありません)。"
        ),
        _ => String::new(),
    }
}

/// Check `cmd`'s arguments. A subcommand this module doesn't know runs as before.
pub fn check(cmd: &str, args: &[String]) -> Check {
    let Some(spec) = spec(cmd) else {
        return Check::Run;
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let a = a.as_str();
        if a == "--help" || a == "-h" {
            return Check::Help(usage(cmd));
        }
        // After `--` everything is a value, as usual
        if a == "--" {
            break;
        }
        if !a.starts_with('-') || spec.flags.contains(&a) {
            continue;
        }
        if spec.valued.contains(&a) {
            it.next();
            continue;
        }
        if let Some((name, _)) = a.split_once('=') {
            if spec.valued.contains(&name) {
                continue;
            }
        }
        let refusal = crate::t!(
            "agentgw {cmd}: unknown option {a} — nothing was done.",
            "agentgw {cmd}: 知らないオプションです: {a} — 何もしていません。"
        );
        return Check::Refuse(format!("{refusal}\n\n{}", usage(cmd)));
    }
    Check::Run
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn help_stops_every_subcommand_before_it_runs() {
        for cmd in [
            "install", "uninstall", "start", "restart", "shutdown", "status", "update", "link",
            "add-machine", "route", "unroute", "remove-machine", "serve", "supervise",
        ] {
            for h in ["--help", "-h"] {
                match check(cmd, &a(&[h])) {
                    Check::Help(text) => assert!(text.contains(&format!("agentgw {cmd}")), "{cmd}: {text}"),
                    other => panic!("{cmd} {h}: {other:?}"),
                }
            }
        }
    }

    #[test]
    fn help_wins_wherever_it_sits() {
        assert!(matches!(check("link", &a(&["--name", "pve", "--help"])), Check::Help(_)));
        assert!(matches!(check("route", &a(&["#audit", "dock", "-h"])), Check::Help(_)));
    }

    #[test]
    fn unknown_options_are_refused() {
        for (cmd, opt) in [("install", "--force"), ("uninstall", "--yes"), ("restart", "-f"), ("update", "--local")] {
            match check(cmd, &a(&[opt])) {
                Check::Refuse(text) => {
                    assert!(text.contains(opt) && text.contains("nothing was done"), "{text}");
                    assert!(text.contains(&format!("Usage: agentgw {cmd}")), "{text}");
                }
                other => panic!("{cmd} {opt}: {other:?}"),
            }
        }
    }

    #[test]
    fn the_options_callers_pass_today_still_run() {
        // install.sh, add-machine, install.ps1 and the Windows task pass these
        for (cmd, args) in [
            ("install", vec!["--no-restart-hint"]),
            ("link", vec!["--name", "pve", "-"]),
            ("link", vec!["--name=pve", "agentgw://x"]),
            ("supervise", vec!["--state-dir", "C:\\Users\\x\\AppData\\Local\\agentgw"]),
            ("status", vec!["--raw"]),
            ("update", vec!["v0.75.0"]),
            ("update", vec!["local"]),
            ("add-machine", vec!["user@host", "-i", "~/.ssh/k", "-p", "-n", "--name", "m", "--from=./a"]),
            ("add-machine", vec!["--print", "3dwin"]),
            ("route", vec!["#audit", "dock:/srv", "--yes"]),
            ("remove-machine", vec!["mac", "-y"]),
        ] {
            assert_eq!(check(cmd, &a(&args)), Check::Run, "{cmd} {args:?}");
        }
    }

    #[test]
    fn a_value_that_looks_like_an_option_is_still_a_value() {
        // The word after an option that takes a value is that value, even when it starts with `-`
        assert_eq!(check("add-machine", &a(&["host", "--from", "-weird-path"])), Check::Run);
    }
}
