# proton-presence

Discord Rich Presence for Steam games running through Proton on Linux.

Discord on Linux usually cannot tell which game is running inside Proton.
proton-presence is a small background service that can: it finds the running
Steam game, looks up its name, and tells Discord over its local socket. It
starts at login, waits quietly while Discord is closed, and reconnects on its
own when Discord starts or restarts.

Written for CachyOS; it works on any distribution with systemd and a current
Rust toolchain.

## Setup (once)

### 1. Install

Install Rust if you do not have it:

```bash
sudo pacman -S --needed rust
```

Then, from this `proton-presence` directory, run the installer as your normal
user (not with `sudo`):

```bash
./install.sh
```

The installer builds the release binary, copies it to
`~/.local/bin/proton-presence`, installs the systemd user unit to
`~/.config/systemd/user/proton-presence.service`, creates
`~/.config/proton-presence/config.toml` if it does not exist, and enables and
starts the service. From then on it starts at every login.

### 2. Create a Discord application

Discord shows the *name of the application* as "Playing …", so you need at
least one application of your own.

1. Open <https://discord.com/developers/applications> and click
   **New Application**.
2. Name it what Discord should show for games without their own entry, for
   example `Steam`.
3. Copy the **Application ID** from the General Information page.

### 3. Fill in config.toml

Open `~/.config/proton-presence/config.toml` and paste the ID:

```toml
default_client_id = "123456789012345678"
```

That is all that is required. The service notices the change within five
seconds; there is nothing to restart.

To show a particular game under its own title and artwork, create another
Discord application named after the game, upload an image under
**Rich Presence → Art Assets**, and map it by Steam app ID (the number in the
game's store URL):

```toml
default_client_id = "123456789012345678"

[games.1245620]                      # ELDEN RING
client_id = "234567890123456789"     # application named "ELDEN RING"
large_image = "elden_ring"           # art asset key in that application

[games.620]                          # Portal 2
large_image = "portal2"              # art asset in the default application
```

Both keys are optional per game. Without `client_id` the game uses
`default_client_id`; without `large_image` no artwork is sent.

### 4. Check it

```bash
~/.local/bin/proton-presence status
```

This reports whether the service is running, whether Discord is reachable,
and which game is detected.

### Troubleshooting

Follow the service log:

```bash
journalctl --user -u proton-presence -f
```

Only state changes are logged: game started, game ended, connected,
disconnected. For more detail, set `Environment=RUST_LOG=debug` in
`~/.config/systemd/user/proton-presence.service`, then run:

```bash
systemctl --user daemon-reload
```

```bash
systemctl --user restart proton-presence
```

To see what would be sent, without connecting to Discord:

```bash
~/.local/bin/proton-presence --once
```

Common causes when nothing shows in Discord:

- `default_client_id` is still empty. The log says so at startup.
- **Settings → Activity Privacy → Share your detected activities with
  others** is off in Discord.
- Vesktop: Rich Presence (arRPC) has to be enabled in the Vesktop settings.
- The log says the handshake was rejected: the client ID is not a valid
  Application ID.

## What Discord shows

| Field | Value |
|---|---|
| Title | The name of your Discord application |
| Details | The game's name from Steam |
| State | `Playing via Proton` (`Playing on Steam` for a native Linux game) |
| Elapsed time | Counted from when the game was first detected |
| Large image | The `large_image` asset, if configured |

The presence is cleared when the game exits.

## How it works

- **Detection.** Every 5 seconds it reads `/proc/<pid>/environ` of each
  process and looks for `SteamAppId` or `SteamGameId`. Launcher and runtime
  processes (`reaper`, `wineserver`, `steam-runtime-*`, `pressure-vessel-*`
  and Wine's own services) are ignored, so a lingering `wineserver` does not
  count as a running game.
- **Names.** The app ID is resolved from `appmanifest_<id>.acf` in the Steam
  libraries listed in `libraryfolders.vdf`, for the native Steam client
  (`~/.steam/steam`, `~/.local/share/Steam`) and the Flatpak one
  (`~/.var/app/com.valvesoftware.Steam`).
- **Discord.** It connects to `discord-ipc-0` … `discord-ipc-9` in
  `$XDG_RUNTIME_DIR`, and in the Flatpak Discord, Flatpak Vesktop and Snap
  Discord subdirectories. The IPC protocol is implemented in
  `src/discord.rs`; there is no Discord library.
- **Reconnecting.** It only connects while a game is running. If Discord is
  closed it retries after 5, 10, 20, 40 and then every 60 seconds. If Discord
  quits mid-game the connection is dropped and re-established the same way,
  keeping the original start time.

## Uninstall

```bash
./uninstall.sh
```

This stops and disables the service and removes the unit and the binary. Your
config is kept; the script prints the command to delete it.

## Development

```bash
cargo test
```

```bash
cargo clippy --all-targets -- -D warnings
```

The tests use a temporary directory as a fake `/proc` and a fake Discord
server on a Unix socket, so they never touch the real system.

## Limitations

- Non-Steam shortcuts have no app manifest, so they
  are shown as `Steam app <id>`.
- If two games run at once, the one detected first is shown until it exits.
- Without a Discord application of your own nothing can be shown; Discord
  has no anonymous client ID.
