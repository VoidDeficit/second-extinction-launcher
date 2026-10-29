# Second Extinction launcher

Prepares Second Extinction (Steam, Linux/Proton) for a custom backend from
[second-extinction-server](https://github.com/VoidDeficit/second-extinction-server) and starts it.

## What it does
1. Finds the game in your Steam libraries (`libraryfolders.vdf`) or uses `--game-dir`.
2. Patches `SecondExtinction_F.exe`. It backs up the original to `SecondExtinction_F.exe.orig` and only patches the known build (checked by sha256).
   The patch clears the crossplay default, so the game hosts lobbies with Steam matchmaking instead of PlayFab Party.
3. Downloads the server's CA from `http://<server>:8443/ca.pem`.
4. Imports it into the game's Proton prefix as a trusted root.
5. Sets the prefix's WinHTTP proxy to `<server>:8888`, so PlayFab traffic reaches the server.
6. Runs `steam -applaunch 1024380 --social-host <server> --social-port 8443 --social-disable-ssl`.

## Usage
```
cargo install --path .
cp se-launcher.desktop ~/.local/share/applications/   # adjust Exec= if ~/.cargo/bin is not in PATH
se-launcher                                            # opens the launcher window
```
Enter the server address and press **Play**. The launcher patches the exe, configures the prefix and starts the game.
**Revert** restores the original exe and removes the CA and proxy.
The settings are saved in `~/.config/se-launcher/settings`.

Command line (same steps, no window):
```
se-launcher --server <host> [--social-port 8443] [--proxy-port 8888] [--game-dir DIR] [--no-launch]
se-launcher --revert
```
Close the game before running it. Steam's "Verify integrity of game files" also restores the exe.
