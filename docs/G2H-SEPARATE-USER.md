# Running g2hd as its own OS user (opt-in)

Status: implemented, opt-in, **not yet run in production**. The code paths are covered by tests that simulate
the bots' uid (see "What is tested" below); nobody has yet run this recipe end to end on a real second account.

By default g2hd, both bots and you share one user, so a bot with a shell can edit `config.toml`, change the
approver or read the state. In separate-user mode:

- g2hd runs as a dedicated user (`_g2h` on macOS, `g2h` on Linux). At startup in this mode it checks, and
  refuses to start otherwise:
  - it is not running as root (its real effective uid), and that uid is not in `allowed_uids`;
  - the home and **every directory above it, up to `/`**, are owned by that user or root and not group/other-writable
    (a root-owned sticky directory such as `/tmp` is allowed as a parent);
  - every file of these that exists in the home is a regular file (not a symlink), owned by that user, and not
    group/other-writable: `config.toml`, `state.json`, `credentials`, `inbox.credentials`, `g2h.log.jsonl`,
    `g2hd.lock`, `sessions.json`, `approvals.txt`;
  - `[ops_approver] command[0]` is an absolute path, is not an interpreter (`sh`, `bash`, `env`, `python3`, `perl`,
    `node` …, whose script would go unchecked), and it and every directory above it are owned by that user or root
    and not group/other-writable.

  Each time it reads a credentials file it also refuses one that is not 0600 or not owned by the uid it runs as.
  It does **not** check the `g2h`/`g2hd` binaries themselves, rotated logs, or anything outside the home; that part
  is the recipe below.
- The home is `0750` and the socket `0660`, both in a group (`g2h`) that holds the g2h user and you. Every
  connection's peer uid (`SO_PEERCRED` / `getpeereid`) must be g2hd's, root's, or in `allowed_uids`; other uids are
  dropped and logged (`peer_refused`).
- Your bots run as your user, so they can still `ask`, `send`, `send-op`, `recv`. They **cannot approve**:
  `approve` is accepted only from g2hd's uid or root (`sudo -u _g2h …`), even with the right code.
- The one-time code goes out through `[ops_approver]`, which runs as the g2h user. Point it at something your user
  cannot read: a file in the g2h home (below), or a notifier whose token lives only in the g2h home.

What it does **not** do: it does not sandbox the bots. A bot with a shell as you can still talk to g2hd, send ops
to the allowlist, use `deny`, and act on your files directly, without g2h. What it adds is that the allowlist, the
approver and the approval step are out of that bot's reach, **only if all of these hold**:

1. **Every path component** of the `g2h` and `g2hd` binaries, the approver and the g2h home is owned by root (or the
   g2h user) and not writable by your user. Your usual `~/.local/bin/g2h` does **not** qualify: your bots can
   replace it. Install root-owned copies (step 2 below). Avoid `/usr/local` on Intel Macs with Homebrew, where
   `/usr/local/bin` and `/usr/local/var` belong to the admin user; the recipe uses `/opt/g2h/bin` and `/var/db/g2h`
   (macOS) or `/var/lib/g2h` (Linux).
2. **Every command run as the g2h user uses absolute paths** (`/usr/bin/sudo`, `/bin/cat`, `/opt/g2h/bin/g2h`),
   because `sudo` on macOS keeps your `PATH`, and your bots can edit it.
3. **You approve from a session your bots cannot modify**: a separate admin account, or a clean login shell
   (for example `/usr/bin/env -i /bin/zsh -f`, then `/usr/bin/sudo …`). Your bots run as you, so they control your
   shell's rc files, aliases and functions (including a fake `sudo`), and could capture your password or swap the
   command the first time you type it there.
4. Your bots cannot run `sudo` (no passwordless sudo for your user, no cached sudo ticket they can use).

Run these yourself. Replace `501` with your uid (`id -u`) and `you` with your login. Build `g2h` and `g2hd`
first (`cargo build --release`, or the release archive) and run the install from that directory.

## macOS (dscl + launchd)

```sh
# 1. a group and a hidden system user (pick ids that `dscl . -list /Users UniqueID` shows free)
D=/Users   # dscl's user records
sudo /usr/bin/dscl . -create /Groups/g2h PrimaryGroupID 450
sudo /usr/bin/dscl . -create "$D/_g2h" UniqueID 450
sudo /usr/bin/dscl . -create "$D/_g2h" PrimaryGroupID 450
sudo /usr/bin/dscl . -create "$D/_g2h" UserShell /usr/bin/false
sudo /usr/bin/dscl . -create "$D/_g2h" NFSHomeDirectory /var/db/g2h
sudo /usr/bin/dscl . -create "$D/_g2h" IsHidden 1
sudo /usr/sbin/dseditgroup -o edit -a you -t user g2h
sudo /usr/sbin/dseditgroup -o edit -a _g2h -t user g2h

# 2. root-owned binaries in root-owned directories
sudo /usr/bin/install -d -o root -g wheel -m 0755 /opt /opt/g2h /opt/g2h/bin /opt/g2h/libexec
sudo /usr/bin/install -o root -g wheel -m 0755 target/release/g2h target/release/g2hd /opt/g2h/bin/

# 3. the home, approver and config
sudo /usr/bin/install -d -o _g2h -g g2h -m 0750 /var/db/g2h
printf '#!/bin/sh\n# appends each approval request to a file only _g2h (and root) can read\numask 077; /bin/cat >> /var/db/g2h/approvals.txt\n' \
  | sudo /usr/bin/tee /opt/g2h/libexec/g2h-approver >/dev/null
sudo /usr/sbin/chown root:wheel /opt/g2h/libexec/g2h-approver
sudo /bin/chmod 0755 /opt/g2h/libexec/g2h-approver
sudo -u _g2h /usr/bin/tee /var/db/g2h/config.toml >/dev/null <<'EOF'
peers = ["grok"]
[separate_user]
allowed_uids = [501]
socket_gid = 450
[ops_approver]
command = ["/opt/g2h/libexec/g2h-approver"]
[ops]
export = "approve"
EOF
sudo /bin/chmod 0600 /var/db/g2h/config.toml

# 4. move the credentials in (owned by _g2h, 0600); a plain cp -p or rsync -a would keep your uid as owner,
#    and g2hd refuses that
sudo /usr/bin/install -o _g2h -g g2h -m 0600 ~/.g2h/credentials /var/db/g2h/credentials
# only if you use one:
sudo /usr/bin/install -o _g2h -g g2h -m 0600 ~/.g2h/inbox.credentials /var/db/g2h/inbox.credentials
```

Do not copy `state.json` or the log; g2hd starts them fresh. Afterwards delete the old credentials from `~/.g2h`
(your bots can read them there).

5. A LaunchDaemon (system-wide, so it can set `UserName`), `/Library/LaunchDaemons/io.g2h.daemon-user.plist`,
owned by root, mode 0644:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>io.g2h.daemon-user</string>
  <key>UserName</key><string>_g2h</string>
  <key>GroupName</key><string>g2h</string>
  <key>ProgramArguments</key><array><string>/opt/g2h/bin/g2hd</string></array>
  <key>EnvironmentVariables</key><dict><key>G2H_HOME</key><string>/var/db/g2h</string></dict>
  <key>RunAtLoad</key><true/><key>KeepAlive</key><true/>
</dict></plist>
```

```sh
sudo /bin/launchctl bootstrap system /Library/LaunchDaemons/io.g2h.daemon-user.plist
```

Stop any same-user g2hd first (`g2h uninstall`), or run the two with different homes.

## Linux (useradd + systemd)

```sh
# 1. a group and a system user
sudo /usr/sbin/groupadd --system g2h
sudo /usr/sbin/useradd --system --gid g2h --home-dir /var/lib/g2h --shell /usr/sbin/nologin g2h
sudo /usr/sbin/usermod -aG g2h you          # log out and in again for the group to apply

# 2. root-owned binaries in root-owned directories
sudo /usr/bin/install -d -o root -g root -m 0755 /opt/g2h /opt/g2h/bin /opt/g2h/libexec
sudo /usr/bin/install -o root -g root -m 0755 target/release/g2h target/release/g2hd /opt/g2h/bin/

# 3. the home, approver and config
sudo /usr/bin/install -d -o g2h -g g2h -m 0750 /var/lib/g2h
printf '#!/bin/sh\numask 077; /bin/cat >> /var/lib/g2h/approvals.txt\n' \
  | sudo /usr/bin/tee /opt/g2h/libexec/g2h-approver >/dev/null
sudo /usr/bin/chown root:root /opt/g2h/libexec/g2h-approver
sudo /usr/bin/chmod 0755 /opt/g2h/libexec/g2h-approver
GID=$(/usr/bin/getent group g2h | /usr/bin/cut -d: -f3)
sudo -u g2h /usr/bin/tee /var/lib/g2h/config.toml >/dev/null <<EOF
peers = ["grok"]
[separate_user]
allowed_uids = [501]
socket_gid = $GID
[ops_approver]
command = ["/opt/g2h/libexec/g2h-approver"]
[ops]
export = "approve"
EOF
sudo /usr/bin/chmod 0600 /var/lib/g2h/config.toml

# 4. move the credentials in (owned by g2h, 0600)
sudo /usr/bin/install -o g2h -g g2h -m 0600 ~/.g2h/credentials /var/lib/g2h/credentials
```

`/etc/systemd/system/g2hd.service`:

```ini
[Unit]
Description=g2h talk bridge (separate user)
[Service]
User=g2h
Group=g2h
Environment=G2H_HOME=/var/lib/g2h
ExecStart=/opt/g2h/bin/g2hd
Restart=on-failure
NoNewPrivileges=yes
ProtectHome=read-only
[Install]
WantedBy=multi-user.target
```

```sh
sudo /usr/bin/systemctl daemon-reload && sudo /usr/bin/systemctl enable --now g2hd
```

The Hermes gateway backend reads the profile's gateway files from `hermes_home`; give the g2h user read access to
that one profile directory (for example a group ACL), not write access.

## Using it

```sh
export G2H_HOME=/var/db/g2h              # /var/lib/g2h on Linux; for you and for both bots
/opt/g2h/bin/g2h doctor                  # "isolation: separate-user: g2hd uid 450; clients [501] …"
```

Approving an op: from a clean session (condition 3 above), read the code as the g2h user, then approve as the
g2h user, with absolute paths only:

```sh
/usr/bin/sudo -u _g2h /bin/cat /var/db/g2h/approvals.txt
/usr/bin/sudo -u _g2h /usr/bin/env G2H_HOME=/var/db/g2h /opt/g2h/bin/g2h approve <request_id> <code>
```

(On Linux: `-u g2h`, `/usr/bin/cat`, `/var/lib/g2h`.)

In same-user mode `g2h doctor` says `isolation: same-user (…): NOT a security boundary`.

## What is tested, what is not
- Tested (`crates/g2h/src/isolation.rs`, `crates/g2h/tests/ops.rs`): the peer-uid and approve policy; startup refusals
  (own uid in `allowed_uids`, root, group/other-writable home, config or approver, relative approver path, a home
  owned by another uid, a group/other-writable or symlinked state/credentials/inbox credentials/log/lock/sessions/
  approvals file, a writable directory anywhere above the home or the approver, an interpreter as approver);
  g2hd's uid is the process euid; socket 0660 and home 0750 with the configured group; a simulated bots' uid can talk and
  request an op but gets `not_owner` on `approve` with the right code, and nothing runs; an unlisted uid is
  dropped; g2hd's own uid approves; doctor reports the mode. The bots' uid is simulated by a test-only override of
  the peer uid (behind the non-default `test-hooks` cargo feature, so release builds do not contain it), because
  the tests run as one account.
- Not tested: a real second OS account, a credentials or home file owned by another uid (needs root to set
  up), g2hd started as root with a lock file left by the g2h user, the dscl/useradd/launchd/systemd steps above, the kernel's peer-credential
  lookup returning a different uid, group membership on a real system, and the Hermes gateway backend reading a
  profile owned by another user.
