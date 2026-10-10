# Linux sandbox (bubblewrap)

Every `run_command`/`verify` invocation on Linux runs inside a
[bubblewrap](https://github.com/containers/bubblewrap) namespace:
read-only system roots, tmpfs `/tmp`, workspace (+ declared writable
dirs) bound read-write, `--unshare-net` unless a policy rule grants
network, and the kill cascade (`--die-with-parent --new-session
--unshare-pid`) so cancellation and timeouts tear down the whole
command tree. macOS uses `sandbox-exec` (Seatbelt) instead — see
`crates/loom-tools/src/sandbox.rs`.

This document covers the one way the Linux sandbox fails for reasons
that are not Loom's fault: **host policy denying unprivileged user
namespaces**. When that happens the wrapper dies before the command
runs, with a `bwrap:`-prefixed line.

## Preflight

`loom run` probes the sandbox **at setup**, before the task starts
(`loom_core::tools::sandbox::preflight()`; embedders should call it
themselves). The probe is exactly CI's smoke command:

```sh
bwrap --die-with-parent --new-session --unshare-pid --unshare-net \
  --ro-bind-try /usr /usr --proc /proc --tmpfs /tmp -- true
```

If the probe fails, `loom run` exits with the raw `bwrap:` error plus
remediation instead of failing every sandboxed command mid-task.
Escape hatch: `LOOM_SKIP_SANDBOX_PREFLIGHT=1 loom run …` attempts the
run anyway.

The same remediation is appended to any failed `run_command` whose
stderr carries a `bwrap:` setup failure (ordinary command failures are
never annotated).

## Distro / environment matrix

| Environment | Default state | Fix |
|---|---|---|
| Debian 11+, Fedora, RHEL 8+, openSUSE, Ubuntu ≤23.10 | Works | — |
| Ubuntu 24.04+ / 25.04+ | **Broken**: `kernel.apparmor_restrict_unprivileged_userns=1`; error `bwrap: setting up uid map: Permission denied` (Launchpad #2144531) | 1 → 2 → 3 below |
| Arch (linux-hardened kernel) | Broken: hardened config restricts unprivileged userns | `pacman -S bubblewrap-suid` |
| Docker / Podman | Broken by default: default seccomp profile blocks `clone`/`unshare` flags + AppArmor userns denial | `--security-opt seccomp=unconfined --security-opt apparmor=unconfined`, or run on the host |
| macOS | Works (`/usr/bin/sandbox-exec`, no probe needed) | — |
| Windows | Not supported (sandbox passes through; scope-out) | — |

## Fixes, in order of preference

### 1. AppArmor profile scoped to the `loom` binary (Ubuntu's intended path)

Ubuntu deliberately refuses a *generic* `bwrap` AppArmor profile: the
`userns,` permission is granted per invoking binary — flatpak, steam
and Chrome each shipped their own. Loom follows the same pattern. Put
this in `/etc/apparmor.d/loom` (adjust the path to your install):

```
/usr/local/bin/loom flags=(attach_disconnected) {
  #include <abstractions/base>

  # bubblewrap, executed by loom, sets up unprivileged user
  # namespaces to build the sandbox.
  userns,
}
```

Load and verify:

```sh
sudo apparmor_parser -r /etc/apparmor.d/loom
sudo aa-status | grep loom
dmesg | grep -i 'apparmor.*loom'   # should show no denials
```

Then re-run `loom run …` — the preflight probe should pass.

### 2. Setuid bubblewrap

`bubblewrap-suid` (Arch and derivatives) installs a setuid `bwrap`
that sets up namespaces with privileges, sidestepping the
unprivileged-userns restriction entirely:

```sh
pacman -S bubblewrap-suid     # Arch / linux-hardened
# Debian/Ubuntu package name stays "bubblewrap"; the AppArmor path
# above applies there instead.
```

### 3. Host-wide sysctl (last resort)

Relaxes the restriction for **every** unprivileged binary on the host:

```sh
sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0   # non-persistent
# persistent: echo 'kernel.apparmor_restrict_unprivileged_userns=0' \
#   | sudo tee /etc/sysctl.d/60-loom-sandbox.conf
```

## Opting out entirely

Per-command escape hatch in `.loom/commands.toml` (the manifest is the
escalation layer, exactly like `network = true`):

```toml
[[command]]
match = "cargo *"
sandbox = false
```

This runs the command unsandboxed on the host. Prefer fixing the host
(1 or 2 above) over blanket opt-outs.

## Why not Landlock?

Landlock needs in-process syscalls; the workspace forbids `unsafe`
(`#![forbid(unsafe_code)]`). The wrapper is an external program so no
unsafe pre-exec hooks exist — that constraint is deliberate and stays.
