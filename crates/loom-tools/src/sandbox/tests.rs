#[cfg(target_os = "linux")]
use super::bubblewrap::find_bwrap;
use super::bubblewrap::{BubblewrapWritable, bubblewrap_argv};
use super::seatbelt::{escape_seatbelt_path, seatbelt_profile};
use super::*;
use std::path::{Path, PathBuf};

#[test]
fn seatbelt_denies_writes_and_network_by_default() {
    let profile = seatbelt_profile(&[PathBuf::from("/ws")], &[], &[], false);
    assert!(profile.contains("(deny file-write*)"));
    assert!(profile.contains("(deny network*)"));
    assert!(profile.contains("(deny appleevent-send)"));
    assert!(profile.contains("(subpath \"/usr/bin/osascript\")"));
    assert!(profile.contains("(subpath \"/ws\")"));
    assert!(profile.contains("(subpath \"/dev/null\")"));
    // The subpath allow must come after the blanket deny.
    let deny = profile.find("(deny file-write*)").unwrap();
    let allow = profile.find("(allow file-write*").unwrap();
    assert!(deny < allow);
}

#[test]
fn seatbelt_re_denies_control_state_after_the_writable_allow() {
    // Later filters win, so the control-state deny must land after
    // the workspace allow it carves out of.
    let workspace = PathBuf::from("/ws");
    let sandbox = Sandbox::resolve(&workspace, SandboxGrants::from_rule(None));
    let profile = seatbelt_profile(&[workspace], &sandbox.protected(), &[], false);
    for relative in [".loom", ".cargo", ".git/config", ".git/hooks"] {
        assert!(
            profile.contains(&format!("(subpath \"/ws/{relative}\")")),
            "{relative} missing from the control-state deny in:\n{profile}"
        );
    }
    let allow = profile.find("(allow file-write*").unwrap();
    let deny = profile.rfind("(deny file-write*").unwrap();
    assert!(
        allow < deny,
        "control-state deny must follow the allow:\n{profile}"
    );
}

#[test]
fn grants_override_the_control_state_defaults() {
    // An operator-granted writable dir is the explicit escalation
    // layer; the defaults must not fight it.
    let workspace = PathBuf::from("/ws");
    let grants = SandboxGrants {
        enabled: true,
        network: false,
        extra_writable: vec![PathBuf::from("/ws/.loom")],
    };
    let sandbox = Sandbox::resolve(&workspace, grants);
    assert!(
        !sandbox
            .protected()
            .iter()
            .any(|path| path.starts_with("/ws/.loom")),
        "granted writable dir still protected: {:?}",
        sandbox.protected()
    );
    assert!(
        sandbox
            .protected()
            .iter()
            .any(|path| path.ends_with(".git/config")),
        "ungranted control state must stay protected"
    );
}

#[test]
fn seatbelt_network_grant_omits_the_deny() {
    let profile = seatbelt_profile(&[PathBuf::from("/ws")], &[], &[], true);
    assert!(!profile.contains("(deny network*)"));
    // The Apple-event deny is unconditional: there is no grant
    // axis for IPC to other applications.
    assert!(profile.contains("(deny appleevent-send)"));
}

/// Runs `program args...` under the real Seatbelt wrapper with the
/// shipped profile, in a scratch workspace (integration check;
/// macOS only).
#[cfg(target_os = "macos")]
fn run_under_seatbelt(program: &str, args: &[&str]) -> std::process::Output {
    let workspace = tempfile::tempdir().expect("tempdir");
    let sandbox = Sandbox::resolve(workspace.path(), SandboxGrants::from_rule(None));
    let profile = seatbelt_profile(
        &sandbox.writable(),
        &sandbox.protected(),
        &sandbox.home_secrets(),
        false,
    );
    std::process::Command::new("/usr/bin/sandbox-exec")
        .arg("-p")
        .arg(&profile)
        .arg("--")
        .arg(program)
        .args(args)
        .output()
        .expect("spawn under sandbox-exec")
}

#[cfg(target_os = "macos")]
#[test]
fn seatbelt_blocks_apple_events() {
    // Review item 2.4: a model-reachable child must not be able
    // to script other applications. The exec deny is the
    // operative layer on current macOS (see the profile docs).
    let output = run_under_seatbelt(
        "/usr/bin/osascript",
        &["-e", "tell application \"Finder\" to get name"],
    );
    assert!(
        !output.status.success(),
        "osascript automation escaped the sandbox: {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[cfg(target_os = "macos")]
#[test]
fn seatbelt_still_runs_real_toolchains() {
    // The added IPC/exec denies must not break the programs the
    // profile exists for. Probes are self-contained binaries:
    // /usr/bin/git and /usr/bin/python3 are xcode-select *shims*
    // on some machines and the shims write /dev/null, which the
    // pre-existing (deny file-write*) has always refused — a
    // machine artifact, not this profile's regression. Where the
    // real CLT toolchain exists, exercise it directly.
    let mut probes: Vec<(String, Vec<&str>)> = vec![
        ("/usr/bin/sw_vers".to_owned(), vec![]),
        ("/bin/ls".to_owned(), vec!["/usr/bin"]),
    ];
    let clt_git = "/Library/Developer/CommandLineTools/usr/bin/git";
    if std::path::Path::new(clt_git).exists() {
        probes.push((clt_git.to_owned(), vec!["--version"]));
    }
    for (program, args) in probes {
        let output = run_under_seatbelt(&program, &args);
        assert!(
            output.status.success(),
            "{program} no longer runs under the profile: {:?}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

/// The sandbox-escape regression from the adversarial review
/// (F-02/F-03/F-04), inverted: a sandboxed `sh` must not be able
/// to plant policy manifests or git hook/config state, while the
/// ordinary git workflow (status/add — index and object writes)
/// keeps working. macOS integration check.
#[cfg(target_os = "macos")]
#[test]
fn seatbelt_blocks_control_state_writes_but_not_git() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let root = workspace.path();
    assert!(
        std::process::Command::new("git")
            .arg("init")
            .arg("-q")
            .arg(root)
            .status()
            .expect("git init")
            .success(),
        "fixture needs git"
    );
    std::fs::write(root.join("tracked.txt"), "content\n").expect("fixture file");
    std::fs::create_dir_all(root.join(".loom")).expect("loom dir");

    let sandbox = Sandbox::resolve(root, SandboxGrants::from_rule(None));
    let profile = seatbelt_profile(
        &sandbox.writable(),
        &sandbox.protected(),
        &sandbox.home_secrets(),
        false,
    );
    let run = |script: &str| {
        std::process::Command::new("/usr/bin/sandbox-exec")
            .arg("-p")
            .arg(&profile)
            .arg("--")
            .arg("/bin/sh")
            .arg("-c")
            .arg(script)
            .current_dir(root)
            .output()
            .expect("spawn under sandbox-exec")
    };

    for target in [".loom/verify.toml", ".git/hooks/pre-commit", ".git/config"] {
        let output = run(&format!("echo pwned > {target}"));
        assert!(
            !output.status.success(),
            "sandboxed write to {target} succeeded: {:?}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert!(
        !root.join(".loom/verify.toml").exists(),
        ".loom/verify.toml must not be creatable from inside the sandbox"
    );
    assert!(
        !root.join(".git/hooks/pre-commit").exists(),
        "hooks must not be plantable from inside the sandbox"
    );
    // The on-disk config byte-for-byte unchanged (the write must
    // fail, not truncate-then-fail).
    assert!(
        std::fs::read_to_string(root.join(".git/config"))
            .expect("git config")
            .contains("[core]"),
        "git config was damaged by the blocked write"
    );

    // The carve-out must not break ordinary git: `status` refreshes
    // the index, `add` writes the index and the object database.
    for git_args in [["status", "--porcelain"], ["add", "-A"]] {
        let output = run(&format!("git {}", git_args.join(" ")));
        assert!(
            output.status.success(),
            "git {} no longer runs under the profile: {:?}",
            git_args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

/// F-08 regression: `open -g -j ./P.app` reached LaunchServices
/// through `(allow default)` and started the bundle *outside* the
/// sandbox (verified in the review with a real bundle). The
/// blanket mach deny must seal it: the launch fails and the
/// bundle's executable never runs. macOS integration check.
#[cfg(target_os = "macos")]
#[test]
fn seatbelt_blocks_launchservices_app_launch() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let root = workspace.path();
    let app = root.join("P.app/Contents/MacOS");
    std::fs::create_dir_all(&app).expect("bundle dir");
    std::fs::write(
        app.join("P"),
        format!("#!/bin/sh\ntouch {}\n", root.join("probe").display()),
    )
    .expect("executable");
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(app.join("P"), std::fs::Permissions::from_mode(0o755))
            .expect("exec bit");
    }
    std::fs::write(
        root.join("P.app/Contents/Info.plist"),
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\"><dict>\
         <key>CFBundleExecutable</key><string>P</string>\
         <key>CFBundleIdentifier</key><string>com.loomtest.P</string>\
         </dict></plist>\n",
    )
    .expect("Info.plist");

    let sandbox = Sandbox::resolve(root, SandboxGrants::from_rule(None));
    let profile = seatbelt_profile(
        &sandbox.writable(),
        &sandbox.protected(),
        &sandbox.home_secrets(),
        false,
    );
    let output = std::process::Command::new("/usr/bin/sandbox-exec")
        .arg("-p")
        .arg(&profile)
        .arg("--")
        .arg("/usr/bin/open")
        .arg("-g")
        .arg("-j")
        .arg(root.join("P.app"))
        .output()
        .expect("spawn under sandbox-exec");
    // LaunchServices is unreachable, so `open` itself fails.
    assert!(
        !output.status.success(),
        "open should not reach LaunchServices: {:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    // The decisive assertion: even if a future macOS reports
    // success, the bundle must never have run.
    std::thread::sleep(std::time::Duration::from_millis(2_000));
    assert!(
        !root.join("probe").exists(),
        "P.app launched outside the sandbox via LaunchServices"
    );
}

/// F-11 regression: home credential material must be unreadable
/// from inside the sandbox. Skips (loudly) on hosts without
/// `~/.ssh` so it never reads the operator's real keys to pass.
#[cfg(target_os = "macos")]
#[test]
fn seatbelt_blocks_home_secret_reads() {
    let home = std::env::var_os("HOME").expect("HOME set");
    let ssh = std::path::Path::new(&home).join(".ssh");
    if !ssh.exists() {
        eprintln!("no ~/.ssh on this host; skipping");
        return;
    }
    let output = run_under_seatbelt("/bin/ls", &[&ssh.to_string_lossy()]);
    assert!(
        !output.status.success(),
        "~/.ssh must be unreadable from inside the sandbox"
    );
}

#[test]
fn seatbelt_escapes_path_metacharacters() {
    let profile = seatbelt_profile(&[PathBuf::from("/ws/na\"me\\dir")], &[], &[], false);
    assert!(profile.contains("/ws/na\\\"me\\\\dir"));
}

#[test]
fn seatbelt_denies_launchservices_and_home_secrets() {
    // F-08: the blanket mach deny is the LaunchServices seal; F-11:
    // home credential material is denied read even under
    // `(allow default)`.
    let workspace = PathBuf::from("/ws");
    let sandbox = Sandbox::resolve(&workspace, SandboxGrants::from_rule(None));
    let profile = seatbelt_profile(
        &[workspace],
        &sandbox.protected(),
        &sandbox.home_secrets(),
        false,
    );
    assert!(profile.contains("(deny mach-lookup)"));
    for program in ["/usr/bin/open", "/usr/bin/automator", "/usr/bin/shortcuts"] {
        assert!(
            profile.contains(&format!("(subpath \"{program}\")")),
            "{program} exec deny missing from:\n{profile}"
        );
    }
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        for secret in [
            ".ssh",
            ".gnupg",
            ".aws",
            ".kube",
            ".config/loom",
            ".local/share/loom",
        ] {
            assert!(
                profile.contains(&format!(
                    "(subpath \"{}\")",
                    escape_seatbelt_path(&home.join(secret))
                )),
                "{secret} missing from the home-secrets deny in:\n{profile}"
            );
        }
    }
}

#[test]
fn grants_override_the_home_secrets_defaults() {
    // The operator's escalation axis applies to secrets the same
    // way it applies to control state: an explicitly granted
    // writable dir (e.g. for sandboxed `ssh`) is not denied read.
    let workspace = PathBuf::from("/ws");
    let ssh = std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join(".ssh"))
        .expect("HOME set");
    let grants = SandboxGrants {
        enabled: true,
        network: false,
        extra_writable: vec![ssh.clone()],
    };
    let sandbox = Sandbox::resolve(&workspace, grants);
    assert!(
        !sandbox.home_secrets().contains(&ssh),
        "granted ~/.ssh still denied: {:?}",
        sandbox.home_secrets()
    );
}

#[test]
#[cfg(unix)]
fn writable_entries_are_canonicalized() {
    // F-01: a grant spelled through a symlink must land in the
    // writable set as its real path, or the Seatbelt subpath allow
    // and the bwrap bind never match the child's syscalls.
    let base = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(base.path().join("real")).expect("real dir");
    std::os::unix::fs::symlink(base.path().join("real"), base.path().join("link"))
        .expect("symlink");
    let granted = base.path().join("link/pending/dir");
    let sandbox = Sandbox::resolve(
        base.path(),
        SandboxGrants {
            enabled: true,
            network: false,
            extra_writable: vec![granted],
        },
    );
    let expected = base.path().join("real").canonicalize().expect("canonical");
    let expected = expected.join("pending/dir");
    assert!(
        sandbox.writable().contains(&expected),
        "canonical grant path missing from {:?}",
        sandbox.writable()
    );
}

#[test]
fn grants_default_to_sandboxed_without_network() {
    let grants = SandboxGrants::from_rule(None);
    assert!(grants.enabled);
    assert!(!grants.network);
    assert!(grants.extra_writable.is_empty());
}

/// The bwrap argv for a `cargo build --release` shaped command, with
/// a cargo home and a granted writable directory that does not
/// exist on this machine.
fn bwrap_argv(network: bool, cargo_home: Option<&str>) -> Vec<String> {
    let optional: Vec<PathBuf> = cargo_home
        .into_iter()
        .map(PathBuf::from)
        .chain(std::iter::once(PathBuf::from("/loom-absent-granted-dir")))
        .collect();
    let sandbox = Sandbox::resolve(Path::new("/ws"), SandboxGrants::from_rule(None));
    bubblewrap_argv(
        "cargo",
        &["build".to_owned(), "--release".to_owned()],
        &BubblewrapWritable {
            workspace: Path::new("/ws"),
            optional: &optional,
            temp: Path::new("/tmp"),
        },
        &sandbox.protected(),
        &sandbox.home_secrets(),
        network,
        Some("/home/dev"),
    )
}

/// Every `flag SRC DEST` triple in `argv`, for bind-style flags.
fn bind_pairs(argv: &[String], flag: &str) -> Vec<[String; 2]> {
    argv.iter()
        .enumerate()
        .filter(|(_, arg)| arg.as_str() == flag)
        .map(|(index, _)| [argv[index + 1].clone(), argv[index + 2].clone()])
        .collect()
}

#[test]
fn bubblewrap_argv_closes_the_kill_cascade() {
    let argv = bwrap_argv(false, None);
    for flag in ["--die-with-parent", "--new-session", "--unshare-pid"] {
        assert!(
            argv.iter().any(|arg| arg == flag),
            "{flag} missing from {argv:?}"
        );
    }
    // `--unshare-pid` makes bwrap mount a fresh procfs instead of
    // bind-mounting the host one, and that only happens when the
    // sandbox asks for /proc.
    let proc = argv
        .iter()
        .position(|arg| arg == "--proc")
        .unwrap_or_else(|| panic!("--proc missing from {argv:?}"));
    assert_eq!(argv[proc + 1], "/proc");
}

#[test]
fn bubblewrap_binds_only_the_workspace_strictly() {
    let argv = bwrap_argv(false, Some("/home/dev/.cargo"));
    // A strict `--bind` on anything but the workspace aborts the whole
    // sandbox when that directory is absent — which on a Linux host
    // with no cargo home broke every single run_command invocation.
    for [src, dest] in bind_pairs(&argv, "--bind") {
        assert_eq!(
            (src.as_str(), dest.as_str()),
            ("/ws", "/ws"),
            "strict bind on an optional path in {argv:?}"
        );
    }
    // Optional writable dirs are bound with the tolerant form, whether
    // or not they exist right now.
    let tolerant = bind_pairs(&argv, "--bind-try");
    assert!(
        tolerant.contains(&["/home/dev/.cargo".to_owned(), "/home/dev/.cargo".to_owned()]),
        "cargo home not bound read-write in {tolerant:?}"
    );
    assert!(
        tolerant.contains(&[
            "/loom-absent-granted-dir".to_owned(),
            "/loom-absent-granted-dir".to_owned()
        ]),
        "granted writable dir not bound read-write in {tolerant:?}"
    );
}

#[test]
fn bubblewrap_exposes_local_toolchains_read_only() {
    let argv = bwrap_argv(false, Some("/usr/local/cargo"));
    let read_only = bind_pairs(&argv, "--ro-bind-try");
    assert!(
        read_only.contains(&["/usr/local".to_owned(), "/usr/local".to_owned()]),
        "/usr/local toolchain directory not exposed read-only in {argv:?}"
    );
    let writable = bind_pairs(&argv, "--bind-try");
    assert!(
        writable.contains(&["/usr/local/cargo".to_owned(), "/usr/local/cargo".to_owned()]),
        "configured cargo home should remain writable in {argv:?}"
    );
}

#[test]
fn bubblewrap_writable_local_cargo_home_shadows_read_only_toolchain_mount() {
    let argv = bwrap_argv(false, Some("/usr/local/cargo"));
    let read_only_index = argv
        .windows(3)
        .position(|window| window == ["--ro-bind-try", "/usr/local", "/usr/local"])
        .expect("read-only /usr/local mount");
    let writable_index = argv
        .windows(3)
        .position(|window| window == ["--bind-try", "/usr/local/cargo", "/usr/local/cargo"])
        .expect("writable Cargo home mount");
    assert!(
        read_only_index < writable_index,
        "writable Cargo home must shadow the broader read-only toolchain bind: {argv:?}"
    );
}

#[test]
fn bubblewrap_tolerates_an_absent_cargo_home() {
    // No cargo home, no grants: the only strict bind left is the
    // workspace, so a host without `~/.cargo` still gets a sandbox.
    let argv = bwrap_argv(false, None);
    let strict = bind_pairs(&argv, "--bind");
    assert_eq!(strict.len(), 1, "unexpected strict binds in {argv:?}");
    assert_eq!(strict[0], ["/ws".to_owned(), "/ws".to_owned()]);
}

#[test]
fn bubblewrap_binds_control_state_read_only_after_every_writable_bind() {
    // Later mounts shadow earlier ones, so the control-state
    // `--ro-bind-try`s must come after the workspace `--bind` (and
    // after every optional writable bind) to carve `.loom`,
    // `.cargo`, and git's config/hooks back out of the writable
    // workspace.
    let argv = bwrap_argv(false, Some("/home/dev/.cargo"));
    let read_only = bind_pairs(&argv, "--ro-bind-try");
    for relative in [".loom", ".cargo", ".git/config", ".git/hooks"] {
        let path = format!("/ws/{relative}");
        assert!(
            read_only
                .iter()
                .any(|[src, dest]| src == &path && dest == &path),
            "{path} not bound read-only in {argv:?}"
        );
    }
    // Every read-only control-state bind lands after the last
    // read-write bind of any kind.
    let last_rw = argv
        .iter()
        .enumerate()
        .filter(|(_, arg)| arg.as_str() == "--bind" || arg.as_str() == "--bind-try")
        .map(|(index, _)| index)
        .next_back()
        .unwrap_or_else(|| panic!("no writable binds in {argv:?}"));
    for (index, arg) in argv.iter().enumerate() {
        if arg == "--ro-bind-try" && argv[index + 1].starts_with("/ws/.") {
            assert!(
                index > last_rw,
                "control-state bind before a writable bind in {argv:?}"
            );
        }
    }
}

#[test]
fn bubblewrap_masks_home_secrets_with_an_empty_tmpfs() {
    // F-11 parity on Linux: the home dir is read-only-mounted
    // whole, so `~/.ssh` et al. need their own tmpfs shadow to
    // read empty instead of credential material. `home_secrets()`
    // derives from the process HOME, so assert against that.
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .expect("HOME set");
    let argv = bwrap_argv(false, Some("/home/dev/.cargo"));
    let tmpfs: Vec<String> = argv
        .iter()
        .enumerate()
        .filter(|(_, arg)| arg.as_str() == "--tmpfs")
        .map(|(index, _)| argv[index + 1].clone())
        .collect();
    for secret in [
        home.join(".ssh"),
        home.join(".gnupg"),
        home.join(".aws"),
        home.join(".kube"),
        home.join(".config/loom"),
        home.join(".local/share/loom"),
    ] {
        if secret.is_dir() {
            assert!(
                tmpfs.contains(&secret.to_string_lossy().into_owned()),
                "{} not masked by a tmpfs in {argv:?}",
                secret.display()
            );
        } else if secret.is_file() {
            let mounts = bind_pairs(&argv, "--ro-bind");
            assert!(
                mounts.contains(&[
                    "/dev/null".to_owned(),
                    secret.to_string_lossy().into_owned()
                ]),
                "{} not masked with /dev/null in {argv:?}",
                secret.display()
            );
        } else {
            assert!(
                !tmpfs.contains(&secret.to_string_lossy().into_owned()),
                "missing secret path unexpectedly mounted in {argv:?}"
            );
        }
    }
}

#[test]
fn bubblewrap_skips_missing_home_secrets() {
    let base = tempfile::tempdir().expect("tempdir");
    let absent = base.path().join(".ssh");
    let argv = bubblewrap_argv(
        "true",
        &[],
        &BubblewrapWritable {
            workspace: base.path(),
            optional: &[],
            temp: base.path(),
        },
        &[],
        std::slice::from_ref(&absent),
        false,
        Some(base.path().to_str().unwrap()),
    );
    assert!(
        !argv
            .windows(2)
            .any(|window| window == ["--tmpfs", absent.to_str().unwrap()]),
        "missing secret path was mounted in {argv:?}"
    );
}

#[test]
fn bubblewrap_granted_network_drops_the_unshare() {
    assert!(
        !bwrap_argv(true, None)
            .iter()
            .any(|arg| arg == "--unshare-net"),
        "network was granted but still unshared"
    );
    assert!(
        bwrap_argv(false, None)
            .iter()
            .any(|arg| arg == "--unshare-net"),
        "network was not granted but is shared"
    );
}

#[test]
fn bubblewrap_appends_the_command_and_its_args_last() {
    let argv = bwrap_argv(false, None);
    let command = argv.len() - 3..;
    assert_eq!(
        &argv[command],
        &[
            "cargo".to_owned(),
            "build".to_owned(),
            "--release".to_owned()
        ],
        "COMMAND must be the argv tail in {argv:?}"
    );
}

/// The probe must be a no-op where the wrapper ships with the OS
/// (macOS) or the sandbox passes through — `loom run` calls it at
/// setup on every platform.
#[test]
fn preflight_is_ok_where_no_wrapper_is_needed() {
    #[cfg(not(target_os = "linux"))]
    assert_eq!(preflight(), Ok(()));
    // On Linux the probe must at least resolve to a real verdict;
    // run it only when a wrapper exists so a dev box without bwrap
    // tests the classification below instead of failing here.
    #[cfg(target_os = "linux")]
    if find_bwrap().is_some() {
        assert_eq!(preflight(), Ok(()), "bwrap present but probe failed");
    }
}

/// Ubuntu 24.04+'s AppArmor denial, verbatim in shape: the wrapper
/// dies at uid-map setup and the command never runs.
#[test]
fn remediation_maps_the_apparmor_userns_denial() {
    let stderr = "bwrap: setting up uid map: Permission denied\n";
    let hint = bwrap_remediation(stderr).expect("uid-map denial must be annotated");
    assert!(hint.contains("user namespaces"), "{hint}");
    assert!(
        hint.contains("apparmor_restrict_unprivileged_userns"),
        "{hint}"
    );
    assert!(hint.contains(".loom/commands.toml"), "{hint}");
}

#[test]
fn remediation_maps_container_seccomp_denials() {
    let stderr = "bwrap: Failed to clone: Operation not permitted\nseccomp\n";
    let hint = bwrap_remediation(stderr).expect("seccomp denial must be annotated");
    assert!(hint.contains("seccomp=unconfined"), "{hint}");
}

/// A bare wrapper failure still gets the generic pointer to the
/// distro matrix rather than nothing.
#[test]
fn remediation_covers_unknown_bwrap_failures() {
    let hint = bwrap_remediation("bwrap: mount /proc: No such file or directory\n")
        .expect("bwrap-prefixed failure must be annotated");
    assert!(hint.contains("docs/linux-sandbox.md"), "{hint}");
}

/// Ordinary command failures are never annotated: no `bwrap:`
/// prefix, no wrapper involvement.
#[test]
fn remediation_leaves_ordinary_failures_alone() {
    assert_eq!(
        bwrap_remediation("error: no such command `buld`\nPermission denied\n"),
        None
    );
    // A command that merely *mentions* bubblewrap in its own output.
    assert_eq!(
        bwrap_remediation("warning: bwrap not used in this configuration\n"),
        None
    );
}
