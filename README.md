# Texel

PlayStation and PlayStation 2 emulators that run in the browser, with a self-hosted game library.

> [!NOTE]
> This project was created with [Claude](https://www.anthropic.com/claude).

Point the server at your BIOS images and disc images: it identifies and verifies them against Redump, finds their artwork and descriptions, and serves a web app where each signed-in user plays them, keeps their own memory cards and save states, and plays together with friends in rooms. The emulators run in the browser (Rust compiled to WebAssembly); the server serves the files and keeps the saves.

- **PlayStation**: commercial games run at full speed, with FMVs, CD audio and reverb; upscaling (to 4×, on the GPU through WebGPU), precise geometry without wobbling polygons, widescreen; DualShock with rumble, keyboard or gamepads.
- **PlayStation 2**: in progress. Retail BIOSes boot, and many games reach their menus or play (Jak and Daxter, GTA San Andreas's intro, Grooverider…), some slowly; others stop early. Movies, sound, memory cards and save states work.
- **Library**: CUE/BIN, ISO, CHD, PBP, and zips of these, read without unpacking; games verified against Redump in the background; box art, screenshots and descriptions (libretro-thumbnails, Wikipedia, Wikidata, optionally IGDB).
- **Saves**: memory cards and save states per user, in a database, with screenshots; cards are managed (renamed, copied, their saves moved between cards) in the web app.
- **Netplay**: rooms of up to 8 people with a chat; two play on the console's two controllers, the others watch, everyone runs the game in step.

BIOS images and games are copyrighted: bring your own, and never commit them. Only signed-in users see the library.

## Library

The server reads four kinds of directories, read-only, each set by its own variable (see Configuration), by default under `/library`:

```
library/
├── ps1/
│   ├── bios/                       # BIOS images, in any layout, zipped or not
│   └── games/
│       ├── SLES-00049/
│       │   ├── disc.cue            # CUE/BIN, any number of tracks
│       │   ├── track01.bin
│       │   └── track02.bin
│       ├── SCES-01431/
│       │   ├── disc.chd            # MAME CHD (`chdman createcd`)
│       │   ├── disc.sbi            # a LibCrypt-protected disc's SBI file, if Redump's is not wanted
│       │   └── disc.yaml           # where to find the game's details, if the lookup fails
│       ├── Some Game.iso           # a single ISO or BIN
│       ├── A Zipped Game.zip       # an ISO, or a CUE sheet and its BINs, zipped
│       └── A PSP Conversion.pbp    # PS1 games converted for the PSP (not encrypted ones)
└── ps2/
    ├── bios/                       # PS2 BIOS images (and DVD player, arcade and MechaCon firmware, listed only)
    └── games/                      # the same formats (PBP aside)
```

- **Several directories**: each variable takes several, separated by `:`, merged as if they were one (to keep games on several volumes, say). A game's saves follow it from one directory to another: they are kept by its path inside its directory.
- **BIOS images** are identified by their content, wherever they are. A game needs a BIOS of its region; the server also downloads the open-source BIOSes (OpenBIOS for the PS1, OpenPS2BIOS for the PS2), so games start without a console's BIOS (with less compatibility). [docs/ps1-bios.md](docs/ps1-bios.md) says which PlayStation BIOS to use.
- **Games** are identified by the serial on their disc or by their tracks, and grouped by Redump's names (multi-disc games show as one, and the player changes discs). Zipped images are read in place: a deflated one is indexed once, in the background (a few seconds per GB; the index, about 1% of its size, is kept in the state directory). Deflate64 zips are not read.
- **LibCrypt**: some European PS1 discs need their subchannel data, which images do not keep: the server downloads Redump's SBI files and gives each disc its own once it is identified. An SBI file next to an image, named like it, comes first.
- **Game details**: when the lookup finds the wrong game or nothing (a title several games share), a YAML file next to the image, named like it (`disc.chd` → `disc.yaml`), or named after the game's serial anywhere in its console's game directories (`SLES-01893.yaml`), says where to look. Every key is optional:

  ```yaml
  # Redump's "Bomberman (Europe)" is Bomberman Party Edition.
  wikipedia: Bomberman Party Edition    # an English article's title, or an article's URL in any language
  wikidata: Q2909728                    # a Wikidata item, or its URL
  igdb: bomberman-party-edition         # an IGDB slug or URL (when IGDB is configured)
  thumbnails: Bomberman (Europe)        # the name libretro-thumbnails files its images under, if not found
  links:                                # other pages, shown on the game's page
    Eurogamer: https://www.eurogamer.net/...
  ```

  The game's page says when a file has a mistake. Changes take effect on the next scan (the server rescans every 30 minutes, or on an administrator's Rescan).

## Running

Texel needs a MariaDB (or MySQL) database for users, saves and verification hashes; the server creates its tables. It must be reached over HTTPS (or on localhost): the emulators need `SharedArrayBuffer`, which browsers only give to secure pages sent with the server's isolation headers (keep a reverse proxy from removing `Cross-Origin-Opener-Policy` and `Cross-Origin-Embedder-Policy`).

### Container

The image is built from the [Dockerfile](Dockerfile) (`docker build -t texel .`), and published as `ghcr.io/sebastka/texel`. With a database (here a temporary one, gone when stopped):

```sh
docker network create texel
docker run -d --rm --name texel-db --network texel \
  -e MARIADB_RANDOM_ROOT_PASSWORD=1 -e MARIADB_DATABASE=texel \
  -e MARIADB_USER=texel -e MARIADB_PASSWORD=change-me \
  mariadb:11.8@sha256:e1d5495f445654134f226f55a41ce0da227116f6f27f283101f405c794d841fc \
  --max-allowed-packet=64M
docker run -d --name texel --network texel -p 127.0.0.1:8080:8080 \
  -e TEXEL_DATABASE_URL=mysql://texel:change-me@texel-db:3306/texel \
  -v /path/to/library:/library:ro -v texel-state:/state \
  ghcr.io/sebastka/texel:edge
docker exec texel /usr/local/bin/texel --create-invite "first user"
```

The last command prints an invitation: open http://localhost:8080, follow "Register with your invitation", and enter its token (see Accounts). The state directory (`/state`) only holds caches (DATs, artwork, game details, zipped images' indexes): it may be emptied, but keeping it saves downloading and indexing again.

### Kubernetes

[`deploy/kubernetes.yaml`](deploy/kubernetes.yaml) is an example: a Deployment (one replica: netplay rooms live in the server's memory), a Service, and a volume for the state directory. It expects the library as a PersistentVolumeClaim, the database URL in a Secret, and an Ingress in front, over HTTPS, passing WebSocket upgrades (netplay). The file's comments list what to provide.

### From source

```sh
nix develop                     # Rust (with the wasm32 target), wasm-bindgen, Node.js, pnpm…
cd frontend
pnpm install
pnpm run wasm                   # the emulators, to WebAssembly
pnpm run build                  # the web app
cd ..
export TEXEL_DATABASE_URL=mysql://texel:change-me@127.0.0.1:13306/texel
export TEXEL_PS1_BIOS=/path/to/ps1/bios TEXEL_PS1_GAMES=/path/to/ps1/games
export TEXEL_PS2_BIOS=/path/to/ps2/bios TEXEL_PS2_GAMES=/path/to/ps2/games
cargo run --release -p texel-server -- --state ./state --web frontend/dist
```

Without Nix: Rust (as `rust-toolchain.toml` says, with the `wasm32-unknown-unknown` target), `wasm-bindgen-cli` of the version in `crates/web/Cargo.toml`, binaryen's `wasm-opt`, Node.js and pnpm. [ARCHITECTURE.md](ARCHITECTURE.md) describes the code and the tools to work on it.

## Configuration

Environment variables (or the server's options of the same names: `texel --help`):

| Variable | Default | |
|---|---|---|
| `TEXEL_DATABASE_URL` | | Required: `mysql://user:password@host:3306/database` (MariaDB or MySQL; add `?ssl-mode=required` for TLS) |
| `TEXEL_PS1_BIOS` | `/library/ps1/bios` | PlayStation BIOS images (directories, separated by `:`) |
| `TEXEL_PS1_GAMES` | `/library/ps1/games` | PlayStation games (directories, separated by `:`) |
| `TEXEL_PS2_BIOS` | `/library/ps2/bios` | PlayStation 2 BIOS images, and other PS2 firmware to list (directories, separated by `:`) |
| `TEXEL_PS2_GAMES` | `/library/ps2/games` | PlayStation 2 games (directories, separated by `:`) |
| `TEXEL_STATE` | `/state` | Caches: DATs, artwork, game details, zipped images' indexes (writable; may be emptied) |
| `TEXEL_WEB` | `/usr/share/texel/web` | The built web app |
| `TEXEL_LISTEN` | `0.0.0.0:8080` | Listen address |
| `TEXEL_PUBLIC_URL` | | The address users reach Texel at (`https://texel.example.com`): OpenID Connect's redirect, secure cookies, the same-origin check. Without it, the request's host, over http |
| `TEXEL_FETCH_METADATA` | `true` | Fetch DATs, artwork and game details |
| `TEXEL_FETCH_OPEN_BIOS` | `true` | Download the open-source BIOSes (OpenBIOS, OpenPS2BIOS) when a newer one is published (checked at start) |
| `TEXEL_VERIFY` | `true` | Hash games in the background to verify them against Redump |
| `TEXEL_RESCAN_MINUTES` | `30` | Rescan the library periodically (0: off) |
| `TEXEL_IGDB_CLIENT_ID`, `TEXEL_IGDB_CLIENT_SECRET` | | [Twitch application](https://api-docs.igdb.com/#account-creation) credentials, for game details from IGDB |
| `TEXEL_OIDC_ISSUER`, `TEXEL_OIDC_CLIENT_ID` | | OpenID Connect provider and client: sign-in with it is offered when both are set |
| `TEXEL_OIDC_CLIENT_SECRET` | | The client's secret (absent: a public client, PKCE only) |
| `TEXEL_OIDC_SCOPES` | `openid profile email` | Scopes asked for (add the one giving groups, if the provider needs it) |
| `TEXEL_OIDC_GROUPS_CLAIM` | `groups` | The claim listing a user's groups |
| `TEXEL_OIDC_USER_GROUPS` | | Who may sign in: any of these groups (comma-separated); empty: anyone the provider accepts |
| `TEXEL_OIDC_ADMIN_GROUPS`, `TEXEL_OIDC_ADMIN_USERS` | | Administrators: groups; subjects, e-mail addresses or user names (comma-separated) |
| `TEXEL_OIDC_NAME` | `single sign-on` | The provider's name on the sign-in button |
| `TEXEL_SESSION_DAYS` | `30` | Session lifetime |
| `TEXEL_INVITE_DAYS` | `7` | Invitation and reset token lifetime |
| `TEXEL_LOG_FORMAT` | `text` | `text` or `json` |
| `RUST_LOG` | `info` | Log filter |

## Accounts

Everyone signs in; each user has their own memory cards and save states. Two ways in:

- **OpenID Connect** (single sign-on): set `TEXEL_OIDC_ISSUER`, `TEXEL_OIDC_CLIENT_ID` (and `TEXEL_OIDC_CLIENT_SECRET` for a confidential client) and `TEXEL_PUBLIC_URL`, and register `<TEXEL_PUBLIC_URL>/auth/oidc/callback` as the client's redirect URI (and `<TEXEL_PUBLIC_URL>/` to come back after signing out). Administrators are the users of `TEXEL_OIDC_ADMIN_GROUPS` or `TEXEL_OIDC_ADMIN_USERS`; `TEXEL_OIDC_USER_GROUPS` restricts who may sign in at all.
- **Invitations**, for people without an account at the provider: an administrator creates one (Admin, in the account menu) and hands over its token or its link. They register with it, choosing an e-mail address, a name and a password. Tokens are used once and expire after `TEXEL_INVITE_DAYS`. A forgotten password gets a reset token from the Admin page. Without OpenID Connect (so without administrators), `texel --create-invite "note"` prints an invitation.

Users who are not administrators are guests: they play, keep saves, open and join rooms; administrators also invite people, manage users and rescan the library.

## Playing

A game's page has its Play button, with the memory cards to use (the game's last ones preselected) and the BIOS. The BIOS page starts a console without a disc, to manage the cards' saves in its own menus.

- **Keyboard**, split like the controller: WASD or the arrow keys for the D-pad, I △, J □, K ✕, L ○, Q/E L1/R1, 1/3 L2/R2, Enter Start, Backspace or Right Shift Select; F5 saves a state to slot 1, F9 loads it.
- **Gamepads** use the browser's standard layout, with both sticks; their Home button is the DualShock's Analog button. A pad the browser does not know shows "Set up gamepad" in the player's toolbar, to record its layout one control at a time.
- **The player's toolbar** has save states (4 slots per disc, with screenshots), disc changes, Analog, Resolution (1× to 4×), Precise geometry, Widescreen, Smooth, Fast-forward and Fullscreen.

**Playing together**: Play together, in the header, opens a room; friends join with its code (Join) or its link, signed in or not (then the room's code only lets them load its game). The game the host starts is the room's: everyone's browser runs it in step. Two people play, on the console's two controllers, handed out by the host (several people on one controller share it); the others watch. The host may hide each player's half of the screen from the other. While others are in the room, resets, save states and disc changes are off. When the host leaves, the first member with an account takes over.

## Documentation

- [ARCHITECTURE.md](ARCHITECTURE.md): how the server, the web app and the emulators are built, what the emulators do and lack, and the tools to work on them.
- [DECISIONS.md](DECISIONS.md): the design decisions, and why.
- [docs/ps1-bios.md](docs/ps1-bios.md): the PlayStation's BIOS versions, which to use, and their checksums.

## License

Texel is under the [MIT License](LICENSE). It downloads but does not include the open-source BIOSes (OpenBIOS and OpenPS2BIOS, MIT as well); BIOS images and games dumped from consoles and discs are not covered: they remain their publishers'.
