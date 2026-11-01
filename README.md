# Second Extinction launcher

Prepares Second Extinction (Steam, Linux/Proton) for a custom backend from
[second-extinction-server](https://github.com/VoidDeficit/second-extinction-server) and starts it.

## What it does
1. Finds the game in your Steam libraries (`libraryfolders.vdf`) or uses `--game-dir`.
2. Patches `SecondExtinction_F.exe`. It backs up the original to `SecondExtinction_F.exe.orig` and only patches the known build (checked by sha256).
   The patch clears the crossplay default, so the game hosts lobbies with Steam matchmaking instead of PlayFab Party.
With **Party crossplay** or mixed mode, steps 3 and 4 are skipped: a small `winhttp.dll` beside the
game sends its web requests to the server and accepts its certificate, so no CA is installed
and Windows needs no administrator rights. A CA left by an older version is removed.

3. Downloads the server's CA from `http://<server>:8443/ca.pem`.
4. Imports it into the game's Proton prefix as a trusted root.
5. Sets the prefix's WinHTTP proxy to `<server>:8888`, so PlayFab traffic reaches the server.
6. Runs `steam -applaunch 1024380 --social-host <server> --social-port 8443 --social-disable-ssl`
   (or, in standalone mode, starts the exe through Proton directly without Steam).

## Party shim

With **Party crossplay** or mixed mode, the launcher downloads `PartyWin.dll` from the
latest server release, verifies its SHA-256 and installs it, keeping the original as
`PartyWin.dll.orig`. A normal Steam-lobby start and **Revert** restore the original.

## Windows

The launcher also runs natively on Windows (`se-launcher-windows-x86_64.exe` from
the CI artifacts). There is no Proton prefix there: the server CA and the WinHTTP
proxy go into the real registry (`HKLM`), so the first setup and **Revert** ask for
administrator rights (UAC). The game is started through `steam.exe -applaunch`, or
directly with the Goldberg DLL when playing without Steam. Settings and players
live in `%APPDATA%\se-launcher`. Mixed mode uses hard links instead of symlinks
for the shadow game directory, so it must be on the same drive as the game.

## Building

`cargo build --release` for Linux. OneDev builds both platforms on every push to
`main` and on `v*` tags (`.onedev-buildspec.yml`): Linux in `rust:1.98-bookworm`,
Windows cross-compiled there for `x86_64-pc-windows-gnu` with mingw-w64. The
binaries are published as job artifacts.

## Usage
```
cargo install --path .
cp se-launcher.desktop ~/.local/share/applications/   # adjust Exec= if ~/.cargo/bin is not in PATH
se-launcher                                            # opens the launcher window
```
Enter the server address and press **Play**. The launcher patches the exe, configures the prefix and starts the game.
**Revert** restores the original exe and `steam_api64.dll` and removes the CA and proxy.
The settings are saved in `~/.config/se-launcher/settings`.

Command line (same steps, no window):
```
se-launcher --server <host> [--social-port 8443] [--proxy-port 8888] [--game-dir DIR] [--no-launch]
se-launcher --revert
```

Global options, accepted in both window and CLI mode:
```
se-launcher --data-dir DIR        # state directory (default: $XDG_CONFIG_HOME/se-launcher)
se-launcher --instance-id NAME    # isolated profile: DIR/instances/NAME/settings
se-launcher --standalone          # alias: --offline
se-launcher --dev-mode            # imply --standalone, allow several clients
se-launcher --emulator-dir DIR    # Goldberg release for non-Steam play
```
With `--instance-id`, the window title shows the profile and its settings are kept
separate, so several launcher profiles can point at different servers.

### Launching from Steam directly
If you bypass the launcher and start the game from Steam, set the launch options
(**Steam → Second Extinction → Properties → Launch Options**):
```
%command% --social-host <server> --social-port 8443 --social-disable-ssl
```
`%command%` must come first. Putting the flags before `%command%` makes Steam try to
run `--social-host` as the program, and the game does not start at all.

### Playing without Steam
`--standalone` (or the **Play without Steam** checkbox) starts the exe through Proton
directly instead of `steam -applaunch`. It uses a [Goldberg](https://gitlab.com/Mr_Goldberg/goldberg_emulator)
steam_api64.dll: the original DLL is backed up as `steam_api64.dll.orig`, the emulator
is copied in, and `steam_settings/steam_appid.txt` is created. **Revert** restores the
original DLL.

The emulator is looked up in this order:
1. the **Emulator** field in the window / `--emulator-dir`
2. `$SE_STEAM_EMULATOR`
3. `<state dir>/steam-emulator`
4. `~/Projects/steam-emulator/release` and `~/.local/share/se-launcher/steam-emulator`

Any of those may point at a release root (`regular/x64/steam_api64.dll` is preferred)
or directly at the folder containing `steam_api64.dll`.

### Simulating your Steam account
In standalone mode the launcher reads `config/loginusers.vdf` from your Steam install and
uses the account Steam is set to sign in as (MostRecent/AutoLogin, otherwise the latest
timestamp). It writes that SteamID64 and persona name to Goldberg's
`steam_settings/configs.user.ini` and copies `config/avatarcache/<id>.png` as the avatar,
so the emulated client looks like your real Steam account.

In dev mode the first slot uses your real SteamID64 and persona name; additional slots use
the same persona with ` #N` and `SteamID64 + (slot-1)`, keeping them unique for co-op.
With `--instance-id` the slots keep their separate instance identities instead, because
multiple launcher profiles normally represent different people.

### Dev mode: several players at once
The **Dev mode: multiple players** checkbox (or `--dev-mode`) allows launching the game
repeatedly. Each press of **PLAY** starts another client with its own Goldberg identity,
save folder and **own copy of the Proton prefix**, using
`GseAppPath`/`GseSavePath`/`GseExeDir`. The separate prefix is required because the game
allows only one instance per Wine prefix (otherwise it shows "Single-instance application.").
Player data lives in:

```
<state dir>/[instances/<id>/]players/player-N/
  steam_settings/{steam_appid.txt,configs.user.ini}
  saves/
  steamid
  pid
  compatdata/1024380/pfx     # isolated prefix, one-time copy of the game's prefix
```

The first launch of each player copies the configured prefix (roughly 0.5-1 GB); later
launches reuse it. Each slot also gets a stable SteamID and the account name `Player N`, so
saves and server progress stay with that player. With `--instance-id`, the first slot is
named after the instance (e.g. `alice`), which is useful when running two launcher windows
side by side. Close the clients before pressing **Revert**. Steam's "Verify integrity of game files" also restores the exe.

### Skipping the intro
**Skip intro** (on by default, `skip_intro=0` in the settings file turns it off)
writes `"SeenIntroVideo": 1` and `"HasSeenCrossplayPopup": 1` into the game's
`settings.json` inside every Proton prefix before launch:

```
<pfx>/drive_c/users/<user>/Saved Games/Avalanche Studios/Second Extinction/<platform>/settings.json
```

The game only replays its intro video and crossplay popup while those flags are
unset, which is the case for every freshly cloned prefix. Marking them up front
makes new dev/mixed players and recreated prefixes go straight to the frontend.

### Mixed test: one Steam client + one Goldberg client
**Mixed: Steam + emulated player** (or `--mixed`) starts both at once:

- the real Steam client from the normal install, with the original `steam_api64.dll`
  and the main Proton prefix;
- one Goldberg client from a shadow game directory, with its own
  `steam_api64.dll`, player identity, save folder and cloned Proton prefix.

The shadow directory (`<state dir>/mixed/game`) symlinks every game file except the
DLL/identity files, so the two clients share the install without fighting over
`steam_api64.dll`. The emulated player uses a guest identity, not your real Steam
account, because the Steam client already uses that account.

**Party crossplay (no exe patch)** (or `--party`) restores the original exe from
`SecondExtinction_F.exe.orig` instead of patching the crossplay byte to Steam.
That makes both clients take the PlayFab Party path. Note: Steam↔Goldberg lobbies
still cannot join through Steam matchmaking; the mixed mode is a test harness for
the Party path, not a working crossplay bridge.

### Local co-op between dev players
Goldberg's LAN discovery broadcasts to UDP/TCP port 47584, and a second client on the
same machine falls back to 47585. The clients then have different ports and never see each
other's lobbies, so the in-game join fails with *"Unable to find the lobby"*. The launcher
writes `steam_settings/custom_broadcasts.txt` with `127.0.0.1:47584` for every dev player,
which makes the client on 47585 reach the one on 47584. Lobby join and the in-game friends
list then work between the local players.

### Playing with a friend who owns the game on Steam
A client running under real Steam and a client running under Goldberg cannot join each
other's lobbies: the game session layer is Steam P2P, so Valve only sees real Steam
lobbies and Goldberg only sees its own LAN/VPN lobbies. The native PlayFab Party
crossplay path is disabled by the launcher patch and would need Microsoft's relay.

The working setup is to have the Steam owner launch through this launcher too, with
**Play without Steam** checked. Their real SteamID64, persona name and avatar are
simulated, so the client still looks like their account, but both players are now on
the same Goldberg network. Point both launchers at the same server, put both PCs on
the same LAN or on one broadcast-capable VPN (ZeroTier, not Tailscale), then use the
in-game friends list and lobby join normally.

### Playing with someone on another PC
The same backend and the same Apex friend system work as long as both players reach the
server and each other:
1. Run the server on the host and start the launcher with `--server <host-ip>` (not
   `127.0.0.1`); open TCP 8443 and 8888 on the host.
2. On the same LAN, Goldberg's normal broadcast discovery finds the other player.
3. Across the internet, put both PCs on one VPN that passes broadcast/multicast (ZeroTier
   works; Tailscale does not), or add the peer's IP to `steam_settings/custom_broadcasts.txt`
   on both sides (one address per line, optionally `ip:47584`).
