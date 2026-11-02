# Running g2hd as its own OS user (opt-in)

Status: implemented, opt-in, **not yet run in production**. The code paths are covered by tests that simulate
the bots' uid (see "What is tested" below); nobody has yet run this recipe end to end on a real second account.

By default g2hd, both bots and you share one user, so a bot with a shell can edit `config.toml`, change the
approver or read the state. In separate-user mode:

- g2hd runs as a dedicated user (`_g2h` on macOS, `g2h` on Linux). Its home, `config.toml`, state, credentials and
  the approver program are owned by that user and not writable by yours. g2hd refuses to start otherwise, and
  refuses to start if its own uid is in `allowed_uids` or if it runs as root.
- The home is `0750` and the socket `0660`, both in a group (`g2h`) that holds the g2h user and you. Every
  connection's peer uid (`SO_PEERCRED` / `getpeereid`) must be g2hd's, root's, or in `allowed_uids`; other uids are
  dropped and logged (`peer_refused`).
- Your bots run as your user, so they can still `ask`, `send`, `send-op`, `recv`. They **cannot approve**:
  `approve` is accepted only from g2hd's uid or root (`sudo -u _g2h …`), even with the right code.
- The one-time code goes out through `[ops_approver]`, which runs as the g2h user. Point it at something your user
  cannot read: a file in the g2h home (below), or a notifier whose token lives only in the g2h home.

What it does **not** do: it does not sandbox the bots. A bot with a shell as you can still talk to g2hd, send ops
to the allowlist, use `deny`, and act on your files directly, without g2h. What it adds is that the allowlist, the
approver and the approval step are out of that bot's reach. If your bot can run `sudo` without a password, none of
this holds.

Run these yourself with `sudo`. Replace `501` with your uid (`id -u`) and `you` with your login.

## macOS (dscl + launchd)

```sh
# 1. a group and a hidden system user (pick ids that `dscl . -list /Users UniqueID` shows free)
D=/Users   # dscl's user records
sudo dscl . -create /Groups/g2h PrimaryGroupID 450
sudo dscl . -create "$D/_g2h" UniqueID 450
sudo dscl . -create "$D/_g2h" PrimaryGroupID 450
sudo dscl . -create "$D/_g2h" UserShell /usr/bin/false
sudo dscl . -create "$D/_g2h" NFSHomeDirectory /usr/local/var/g2h
sudo dscl . -create "$D/_g2h" IsHidden 1
sudo dseditgroup -o edit -a you -t user g2h
sudo dseditgroup -o edit -a _g2h -t user g2h

# 2. the home, config and approver, owned by _g2h
sudo install -d -o _g2h -g g2h -m 0750 /usr/local/var/g2h
sudo install -d -o root -g wheel -m 0755 /usr/local/libexec
sudo tee /usr/local/libexec/g2h-approver >/dev/null <<'EOF'
#!/bin/sh
# appends each approval request to a file only _g2h (and root) can read
umask 077; cat >> /usr/local/var/g2h/approvals.txt
EOF
sudo chown root:wheel /usr/local/libexec/g2h-approver && sudo chmod 0755 /usr/local/libexec/g2h-approver
sudo -u _g2h tee /usr/local/var/g2h/config.toml >/dev/null <<'EOF'
peers = ["grok"]
[separate_user]
allowed_uids = [501]
socket_gid = 450
[ops_approver]
command = ["/usr/local/libexec/g2h-approver"]
[ops]
export = "approve"
EOF
sudo chmod 0600 /usr/local/var/g2h/config.toml
```

3. A LaunchDaemon (system-wide, so it can set `UserName`), `/Library/LaunchDaemons/io.g2h.daemon-user.plist`,
owned by root, mode 0644:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>io.g2h.daemon-user</string>
  <key>UserName</key><string>_g2h</string>
  <key>GroupName</key><string>g2h</string>
  <key>ProgramArguments</key><array><string>/usr/local/bin/g2hd</string></array>
  <key>EnvironmentVariables</key><dict><key>G2H_HOME</key><string>/usr/local/var/g2h</string></dict>
  <key>RunAtLoad</key><true/><key>KeepAlive</key><true/>
</dict></plist>
```

```sh
sudo launchctl bootstrap system /Library/LaunchDaemons/io.g2h.daemon-user.plist
```

The `g2hd` binary must not be writable by your user (`/usr/local/bin`, owned by root). Stop any same-user g2hd
first (`g2h uninstall`), or run the two with different homes.

## Linux (useradd + systemd)

```sh
sudo groupadd --system g2h
sudo useradd --system --gid g2h --home-dir /var/lib/g2h --shell /usr/sbin/nologin g2h
sudo usermod -aG g2h you          # log out and in again for the group to apply
sudo install -d -o g2h -g g2h -m 0750 /var/lib/g2h
sudo install -o root -g root -m 0755 /dev/stdin /usr/local/libexec/g2h-approver <<'EOF'
#!/bin/sh
umask 077; cat >> /var/lib/g2h/approvals.txt
EOF
# config.toml as above, with socket_gid = $(getent group g2h | cut -d: -f3), then:
sudo chown g2h:g2h /var/lib/g2h/config.toml && sudo chmod 0600 /var/lib/g2h/config.toml
```

`/etc/systemd/system/g2hd.service`:

```ini
[Unit]
Description=g2h talk bridge (separate user)
[Service]
User=g2h
Group=g2h
Environment=G2H_HOME=/var/lib/g2h
ExecStart=/usr/local/bin/g2hd
Restart=on-failure
NoNewPrivileges=yes
ProtectHome=read-only
[Install]
WantedBy=multi-user.target
```

```sh
sudo systemctl daemon-reload && sudo systemctl enable --now g2hd
```

The Hermes gateway backend reads the profile's gateway files from `hermes_home`; give the g2h user read access to
that one profile directory (for example a group ACL), not write access.

## Using it

```sh
export G2H_HOME=/usr/local/var/g2h      # /var/lib/g2h on Linux; for you and for both bots
g2h doctor                               # "isolation: separate-user: g2hd uid 450; clients [501] …"
# approving an op: read the code as the g2h user, approve as the g2h user
sudo -u _g2h cat /usr/local/var/g2h/approvals.txt
sudo -u _g2h env G2H_HOME=/usr/local/var/g2h g2h approve <request_id> <code>
```

In same-user mode `g2h doctor` says `isolation: same-user (…): NOT a security boundary`.

## What is tested, what is not
- Tested (`crates/g2h/src/isolation.rs`, `crates/g2h/tests/ops.rs`): the peer-uid and approve policy; startup refusals
  (own uid in `allowed_uids`, root, group/other-writable home, config or approver, relative approver path, a home
  owned by another uid); socket 0660 and home 0750 with the configured group; a simulated bots' uid can talk and
  request an op but gets `not_owner` on `approve` with the right code, and nothing runs; an unlisted uid is
  dropped; g2hd's own uid approves; doctor reports the mode. The bots' uid is simulated by a test-only override of
  the peer uid, because the tests run as one account.
- Not tested: a real second OS account, the dscl/useradd/launchd/systemd steps above, the kernel's peer-credential
  lookup returning a different uid, group membership on a real system, and the Hermes gateway backend reading a
  profile owned by another user.
