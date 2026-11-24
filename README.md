# Texel

A PlayStation emulator that runs in the browser, with a self-hosted game library.

The emulation core is written in Rust and compiled to WebAssembly. It runs in a Web Worker, and a small Rust server serves the web app, your BIOS images and games, keeps each user's memory cards and save states in a MariaDB database, and fetches game metadata from Redump, libretro and (optionally) IGDB.

This is a private project: BIOS images and games are copyrighted. Never commit them. Only signed-in users see the library (see Accounts).

## Status

Boots the BIOS shell and commercial games (tested: Rayman, Gran Turismo) at full speed, with FMVs, CD audio, XA audio and reverb.

| Part | State |
|---|---|
| CPU (R3000A) | Interpreter with load/branch delay slots and exceptions; instruction cache, memory access times, multiply/divide and GTE stalls |
| GTE | All commands, flags and the UNR divider, with command durations |
| GPU | Software rasteriser: polygons, rectangles, lines, textures (4/8/15-bit), semi-transparency, dithering, mask bit, VRAM transfers, 15/24-bit display; internal resolution 1× to 4×, drawn on the GPU through WebGPU where the browser has it (else in another worker, on another core), lowered while emulation lags, with precise geometry (no wobbling polygons) and perspective-correct textures; widescreen |
| CD-ROM | Commands, data reads, CD-DA playback with auto-pause and reports, XA-ADPCM streaming, lid (disc swapping), subchannel data of LibCrypt-protected discs (from SBI files) |
| MDEC | Decoding to 15/24-bit and monochrome output |
| SPU | 24 ADPCM voices, ADSR, volume sweeps, noise, pitch modulation, reverb, CD audio mixing, IRQ |
| Controllers | DualShock: digital and analog modes, configuration commands, rumble (Gamepad API); keyboard |
| Memory cards | Each user's own, named, per console, saved in the database: chosen before a game starts (two slots; the game's last ones preselected), created, renamed and deleted on the Memory cards page, which lists their saves (titles, PlayStation icons) and deletes, copies or moves them between cards, PlayStation and PS2; a console started without a disc (Start the console, on the BIOS page) gets chosen cards too, to manage their saves in its BIOS, or blank ones |
| Save states | Per user: 4 slots per disc, with screenshots and the BIOS they need, saved in the database; each game remembers the BIOS it was last started with |
| Netplay | Rooms of up to 8 people with a chat: everyone runs the game in step, exchanging controller input through the server; a room lasts the session, beside the pages: the games its host starts are the room's, and another member with an account takes over when the host leaves |
| Disc images | CUE/BIN (any number of files), ISO, single BIN, CHD, PBP (unencrypted), any of these zipped |
| Missing | Cycle-accurate timing (DMA and CD-ROM delays are approximate, GPU drawing time is modelled per pixel), SPU Gaussian interpolation, multitap, mouse, light guns, serial port 1, encrypted PBPs |

## Library layout

The server reads a library directory (read-only), with a directory per console:

```
library/
├── ps1/
│   ├── bios/                       # any layout of subdirectories, for example:
│   │   ├── original/scph1002.bin   # Sony console BIOSes, 512 KiB
│   │   ├── psxonpsp/psxonpsp660.bin
│   │   └── openbios/openbios.bin
│   └── games/
│       ├── SLES-00049/
│       │   ├── disc.cue            # CUE/BIN, any number of tracks
│       │   ├── track01.bin
│       │   └── track02.bin
│       ├── SCES-01431/
│       │   ├── disc.chd
│       │   └── disc.sbi            # LibCrypt-protected disc: Redump's SBI file
│       ├── SLES-01893/
│       │   ├── disc.chd
│       │   └── disc.yaml           # optional: where to find the game's details
│       ├── Some Game.iso           # or a single ISO / BIN
│       ├── Another Game.chd        # MAME CHD (`chdman createcd`, in the dev shell)
│       ├── A Zipped Game.zip       # an ISO, or a CUE sheet and its BINs, zipped (stored or deflated)
│       └── A PSP Conversion.pbp    # PS1 games converted for the PSP (not encrypted ones)
└── ps2/                            # PlayStation 2: listed and verified; its emulator is in progress
    ├── bios/
    └── games/
```

Each of the four directories can also be set on its own (`TEXEL_PS1_BIOS`, `TEXEL_PS1_GAMES`, `TEXEL_PS2_BIOS`, `TEXEL_PS2_GAMES`, see Configuration), for example to keep games on another volume. Each of these settings, and `TEXEL_LIBRARY`, takes several directories, separated by `:` (or the option repeated): a console's directories are merged, as if they were one. A game's path, by which its saves, settings and verification are kept, is relative to its directory, whichever it is: games can move from one directory to another. A game found at the same relative path in two directories is taken from the first (the scan says so); a BIOS found twice is listed once. PlayStation 2 games and BIOSes are listed, identified and verified like PlayStation ones (with Redump's and libretro's PS2 DATs and thumbnails); PS2 games start, but their emulator is in progress (see below).

CHD and PBP images are decompressed by the server, which presents their tracks as BIN files; they are identified and verified against Redump like the others. Zipped images are read in the zip, without unpacking it: a deflated image is indexed once (the first scan that finds it decompresses it whole, a few seconds per GB, and keeps a seek index of about 1% of its size in the state directory's `cache/zip-index`), then read from the closest indexed point (a few milliseconds per seek). Deflate64 zips (Windows' for very large files) are not read: zip them again, or convert them to CHD, which compresses discs better. Symbolic links are followed. Multi-disc games are recognised by their Redump names ("Disc 1", "Disc 2"…): they show as one game, share their settings (BIOS, memory cards), and the player can change discs.

BIOS images are identified by their content, wherever they are under `bios/`, zipped or not: by their version string and Redump's BIOS DAT. Two region-free BIOSes are known too: the PSP firmware 6.60 PS1 emulator BIOS (`psxonpsp660.bin`, which Redump does not list; the default for games of regions without a BIOS of their own), and [OpenBIOS](https://github.com/pcsx-redux/nugget/tree/main/openbios), PCSX-Redux's open-source replacement (MIT), which needs no dump from a console. OpenBIOS images carry no version; when a `SOURCE.md` next to one lists its SHA-256 (as a fetch script may write it), the server shows its build number, date and PCSX-Redux commit from there. [docs/ps1-bios.md](docs/ps1-bios.md) describes the BIOS versions, which ones to use, and their checksums.

Some European discs (1999 and later) are protected by LibCrypt, which checks subchannel data that disc images do not keep: without it, they stop at a loading or copyright screen, or misbehave later. Redump publishes that data for each protected disc as an SBI file (on the disc's page, "SBI subchannels"); put it next to the image, named like it (`disc.chd` → `disc.sbi`, `Game (Europe).cue` → `Game (Europe).sbi`). The game's page says when a disc has one.

When the search for a game's details finds the wrong game or nothing (a title several games share, like "Bomberman"), a YAML file next to its image, named like it (`disc.chd` → `disc.yaml`), says where to look. Every key is optional:

```yaml
# Redump's "Bomberman (Europe)" is Bomberman Party Edition.
wikipedia: Bomberman Party Edition    # an English article's title, or an article's URL in any language
wikidata: Q2909728                    # a Wikidata item, or its URL
igdb: bomberman-party-edition         # an IGDB slug or URL (when IGDB is configured)
links:                                # other pages, shown on the game's page
  Eurogamer: https://www.eurogamer.net/...
  IGN: https://www.ign.com/...
```

`wikipedia` and `wikidata` replace the search; `links` are only shown, as sites like Eurogamer and IGN have no open API to read. A file with a mistake (an unknown key, a malformed address) is ignored, and the game's page says why. Editing the file takes effect on the next library scan (Rescan).

Games are identified by the serial in their `SYSTEM.CNF` (through libretro's Redump-derived DAT), or by their track sizes, and verified track by track against Redump's SHA-1 hashes in the background.

## Metadata sources

| Source | Used for |
|---|---|
| [Redump](http://redump.org/) game DATs | Canonical names, regions, languages; SHA-1 verification |
| [Redump](http://redump.org/) BIOS DATs | BIOS models (pinned by SHA-256: they no longer change) |
| [libretro-database](https://github.com/libretro/libretro-database) | Serial → Redump name |
| libretro-thumbnails ([PS1](https://github.com/libretro-thumbnails/Sony_-_PlayStation), [PS2](https://github.com/libretro-thumbnails/Sony_-_PlayStation_2)) | Box art, title screens, in-game snapshots |
| [Wikipedia](https://en.wikipedia.org/) | Game descriptions (CC BY-SA, credited on the game page) |
| [Wikidata](https://www.wikidata.org/) | Genres, developers, publishers, game modes, release date |
| [IGDB](https://www.igdb.com/) (optional) | Fills what Wikipedia and Wikidata lack; rating |

Everything is cached in the state directory. The browser never contacts third parties: the server proxies the artwork.

## Running

The server keeps saves, settings and verification hashes in a MariaDB (or MySQL) database, and creates its tables. For a local, temporary one (gone when stopped):

```sh
docker run -d --rm --name texel-mariadb -p 127.0.0.1:13306:3306 \
  -e MARIADB_RANDOM_ROOT_PASSWORD=1 -e MARIADB_DATABASE=texel \
  -e MARIADB_USER=texel -e MARIADB_PASSWORD=change-me \
  mariadb:11.8@sha256:e1d5495f445654134f226f55a41ce0da227116f6f27f283101f405c794d841fc \
  --max-allowed-packet=64M
```

```sh
nix develop                     # Rust (with the wasm32 target), wasm-bindgen, Node, pnpm…
cd frontend
pnpm install
pnpm run wasm                   # build the core to WebAssembly
pnpm run build                  # build the web app
cd ..
export TEXEL_DATABASE_URL=mysql://texel:change-me@127.0.0.1:13306/texel
cargo run --release -p texel-server -- --library /path/to/library --state ./state --web frontend/dist
```

Open http://localhost:8080 and sign in (see Accounts: without OpenID Connect, create an invitation first). For frontend development, run the server on port 8080 and `pnpm dev` (Vite proxies `/api`).

Keyboard, split like the controller: WASD or the arrow keys for the D-pad, I △, J □, K ✕, L ○, Q/E L1/R1, 1/3 L2/R2, Enter Start, Backspace or Right Shift Select; F5 saves a state to slot 1, F9 loads it. Gamepads use the browser's standard layout, with both sticks; their Home button is the DualShock's Analog button. Pads the browser does not know show "Set up gamepad" in the player's toolbar, to record their layout one control at a time (kept per gamepad model, in the browser). The player also has Analog, Resolution (1× to 4×), Precise (precise geometry, with WebGPU), Widescreen, Smooth, Fast-forward and States controls.

### Playing together (netplay)

Play together, in the header, opens a room under your account's name; its bar then stays under the header, on every page, and its chat is beside the screen in a game. Friends join it with its code (Join, in the top bar) or its link, signed in or not (then under the name they give: the room's code lets them load its game and BIOS, nothing else). The PlayStation game the host starts is the room's: the others are taken to it, their browser loads the same BIOS and disc and gets the game's state and memory cards from the host, and everyone then runs the game in step, exchanging only controllers' input through the server. Up to 8 people can be in a room, with a chat.

- Two can play at once, on the console's two controllers; the others watch. The host hands out each controller with the 🎮 1 and 🎮 2 buttons next to the names (the first friend to join gets the second). Several people on one controller share it: their buttons are combined, so one-player games can be played together, or in turns. Everyone plays with their keyboard or first gamepad.
- Split screen (optional, set by the host): each player's browser hides the other player's half of the screen, top/bottom or left/right, against peeking. Those who watch, or hold both controllers, see everything.
- Input is delayed by a few frames, adjusted from the measured round trips, so that it reaches everyone in time. When a player's input is late, everyone waits; nobody waits for those who watch, who catch up (or reload the game's state) when they fall behind.
- Every 120 frames, members compare a checksum of the machine with the one of the member with the controls (the host, when they have one). A member whose checksum differs gets their state.
- A room is a session, not one game: when the host leaves the game (to the library, say), it stops for everyone, and the next game the host starts is everyone's. Others may step out of a game (nobody waits for them meanwhile) and back in (Back to the game, in the bar).
- When the host leaves the room, the first member with an account takes over: the game goes on, and the new host hands out the controllers and starts the next games. With nobody signed in left, the room closes.
- Once others are in the room, reset and save states are disabled, as are disc changes while a room is open; memory cards are saved by their owner only (the host who loaded them; not a member who took over, whose own cards they are not).

### Accounts

Only signed-in users see the games and BIOSes, and each keeps their own memory cards and save states. Two ways in:

- OpenID Connect (single sign-on): set `TEXEL_OIDC_ISSUER`, `TEXEL_OIDC_CLIENT_ID` (and `TEXEL_OIDC_CLIENT_SECRET` for a confidential client) and `TEXEL_PUBLIC_URL`, and register `<TEXEL_PUBLIC_URL>/auth/oidc/callback` as the client's redirect URI (and `<TEXEL_PUBLIC_URL>/` to come back after signing out). The server is the client (authorization code with PKCE); the browser only holds a session cookie. Administrators are the users of `TEXEL_OIDC_ADMIN_GROUPS` (groups, from the `TEXEL_OIDC_GROUPS_CLAIM` claim) or `TEXEL_OIDC_ADMIN_USERS` (subjects, e-mail addresses or user names); `TEXEL_OIDC_USER_GROUPS` restricts who may sign in at all.
- Invitations, for friends without an account at the provider: an administrator creates one on the Admin page and hands over its token (or its link, `/register?token=…`, which fills it in). They register with it, choosing an e-mail address (to sign in with), a name and a password (Argon2id-hashed). Tokens are used once and expire after `TEXEL_INVITE_DAYS`; only their hash is stored. For a forgotten password, the Admin page gives a reset token, used on the same registration page. Without OpenID Connect (no administrator), `texel --create-invite "note"` prints an invitation.

Everyone but administrators is a guest: guests play, keep saves, open and join rooms; administrators also invite people, manage users and rescan the library. Sessions last `TEXEL_SESSION_DAYS`. Requests that change something must come from Texel's own pages (checked with `Sec-Fetch-Site` and `Origin`).

### Configuration

| Variable | Default | |
|---|---|---|
| `TEXEL_LISTEN` | `0.0.0.0:8080` | Listen address |
| `TEXEL_LIBRARY` | `/library` | Library roots, separated by `:` (each with `ps1/bios/`, `ps1/games/`, `ps2/bios/`, `ps2/games/`) |
| `TEXEL_PS1_BIOS` | `LIBRARY/ps1/bios` | PlayStation BIOS images (directories, separated by `:`) |
| `TEXEL_PS1_GAMES` | `LIBRARY/ps1/games` | PlayStation games (directories, separated by `:`) |
| `TEXEL_PS2_BIOS` | `LIBRARY/ps2/bios` | PlayStation 2 BIOS images (directories, separated by `:`) |
| `TEXEL_PS2_GAMES` | `LIBRARY/ps2/games` | PlayStation 2 games (directories, separated by `:`; the emulator is in progress) |
| `TEXEL_DATABASE_URL` | | Required: `mysql://user:password@host:3306/database` (MariaDB or MySQL; add `?ssl-mode=required` for TLS) |
| `TEXEL_STATE` | `/state` | Caches: DATs, artwork, game details (writable; may be emptied) |
| `TEXEL_WEB` | `/usr/share/texel/web` | Built web app |
| `TEXEL_FETCH_METADATA` | `true` | Fetch DATs, artwork and game details |
| `TEXEL_VERIFY` | `true` | Hash games in the background to verify them against Redump |
| `TEXEL_DAT_MAX_AGE_DAYS` | `7` | Refresh the game DATs when older |
| `TEXEL_RESCAN_MINUTES` | `30` | Rescan the library periodically (0: off) |
| `TEXEL_IGDB_CLIENT_ID`, `TEXEL_IGDB_CLIENT_SECRET` | | [Twitch application](https://api-docs.igdb.com/#account-creation) credentials for IGDB |
| `TEXEL_PUBLIC_URL` | | The address users reach Texel at (`https://texel.example.com`): OpenID Connect's redirect, secure cookies, the same-origin check. Without it, the request's host, over http |
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

### Container and Kubernetes

```sh
docker build -t texel .
docker run -p 8080:8080 -e TEXEL_DATABASE_URL=mysql://… -v /path/to/library:/library:ro --tmpfs /state texel
```

[`deploy/kubernetes.yaml`](deploy/kubernetes.yaml) has a Deployment (with an `emptyDir` for the caches) and a Service; the database URL comes from the `texel-database` Secret, the OpenID Connect settings from the `texel` Secret. Put an Ingress in front of it (passing WebSocket upgrades, for netplay). The server must be reached over HTTPS (or on localhost) and its `Cross-Origin-Opener-Policy` and `Cross-Origin-Embedder-Policy` headers must reach the browser: audio relies on `SharedArrayBuffer`.

## Testing the core without a browser

```sh
cargo run --release -p texel-ps1 --example headless -- BIOS [DISC.cue] [FRAMES] [OUT_DIR]
PRESS="1900:cross,2300:start" cargo run --release -p texel-ps1 --example headless -- …
```

It writes a PPM screenshot every 60 frames and prints the machine state. More environment variables: `SCALE=2` (internal resolution), `SWAP=1900:1960` (open the lid at a frame, close it again at another), `STATE=3000:3300` (save a state at a frame, load it at another: checks that states are exact), `CDLOG=1` (the CD-ROM's commands and the XA sectors played, with the SPU's volumes).

The scaled drawing on the GPU (`crates/ps1-gpu`) is checked against the software, which it must match pixel for pixel: `cargo run --release -p texel-ps1-gpu --example compare -- BIOS DISC.cue [FRAMES] [SCALE] [OUT_DIR]` runs a game twice in step and compares the displayed pictures every second (`GPU_ONLY=1`: the GPU alone, to measure it; `PRESS` as above).

The PlayStation 2 emulator is in progress: its two CPUs (the Emotion Engine, and the I/O processor), the vector units, the GIF and the Graphics Synthesizer boot a retail BIOS through its intro to its browser, with two controllers. In the web app, a PS2 game's Play button starts it, straight from its disc (the BIOS starts the game's executable rather than its browser: no logo, no browser), and the PS2 tab of the BIOS page starts a console without a disc (Start the console); memory cards are chosen and kept as the PlayStation's (the server formats new ones), sound plays (with reverb), and games have save states as the PlayStation's; many games do not get far: Jak and Daxter runs its opening and title screen, with a few drawing errors left, and slowly once its jungle is drawn. The EE's code is compiled to WebAssembly as it runs, as are the VUs' microprograms (four lanes at once, in WebAssembly's SIMD), and where the browser has WebGPU, the GS draws on the GPU (the player's status says so: EE and VUs compiled, WebGPU). With both, the BIOS runs its menus at full speed (60 frames per second) and its intro at about 25 to 30; without WebGPU, the GS draws in software, at about 10. Without the browser:

```sh
cargo run --release -p texel-ps2 --example boot -- PS2-BIOS [SECONDS]
```

It prints what the BIOS kernel writes to its debug port (its start-up log), then where the CPUs are, what they wait for, the traffic between them, and what they used that is not emulated. Environment variables show more: `DUMP=1` (the busy loops), `EXC=1` (the EE's last exceptions and system calls), `PACKETS=1` and `TAGS=1` (SIF commands and DMA tags), `MEM=start,end` and `IOPMEM=start,end` (memory, hex), `IOPREGS=1` and `TLB=1` (the IOP's registers, the EE's TLB), and `SHOTS=dir` (the displayed frame every second, as PPM). The IOP's modules' output (their `printf`, which retail consoles send nowhere) is printed too. `DISC=image.iso` (or a raw `.bin`) puts a disc in the drive and boots its game directly (`OSD=1`: through the browser instead), `MC1=card.ps2` and `MC2=…` insert memory cards (created unformatted if missing, saved at the end), `WAV=out.wav` saves the sound, `NOGFX=1` draws nothing (faster), and `CDVDCMDS=1` and `CARDLOG=1` list the drive's and the cards' commands. `SAVE=state` saves a state at the end, and `LOAD=state` starts from one. `DRAWS=frames` lists what the GS was asked to draw over that many more frames (by draw context, and VU1's packets by program), `PROG=hex` traces the VU1 runs starting there, and `FINDXY=x,y` tells which GIF path sent a vertex. The compare example takes `DISC`, `MC1` and `PRESS` too, and draws on the GPU (faster).

The GS runs on the GPU (`crates/ps2-gpu`, with wgpu: Vulkan, Metal or DirectX 12 natively, WebGPU in browsers). To boot a BIOS natively with both GSs and compare them, or with the GPU alone to measure its speed:

```sh
cargo run --release -p texel-ps2-gpu --example compare -- PS2-BIOS [SECONDS] [OUT_DIR]
GPU_ONLY=1 cargo run --release -p texel-ps2-gpu --example compare -- PS2-BIOS [SECONDS] [OUT_DIR]
```

Every second, it prints how many GS memory words differ between the two (`PAGES=1`: by page), or the speed, and writes both displayed frames to `OUT_DIR` (PPM). The EE's compiler is checked the same way, in WebAssembly (Node.js): `node scripts/check-ps2-compiler.mjs PS2-BIOS [SECONDS]` (in `frontend`, after `pnpm run wasm`) boots a BIOS with the EE's code compiled and interpreted, and fails unless both consoles are in the same state every second. Its tests (`cargo test -p texel-ps2-gpu`) draw with both and compare the whole GS memory; without a GPU, they pass and say so. On Linux, the dev shell provides the Vulkan loader; the driver comes from the system.

## Layout

| Path | |
|---|---|
| `crates/disc` | CUE/BIN and ISO layouts, raw sectors, ISO 9660 and `SYSTEM.CNF` |
| `crates/ps1` | The PlayStation emulator |
| `crates/ps2` | The PlayStation 2 emulator, in progress: the Emotion Engine (interpreted, and compiled to WebAssembly in browsers), the vector units, the VIFs, the GIF, the Graphics Synthesizer, and the I/O processor (on `crates/ps1`'s CPU) |
| `crates/ps1-gpu` | The PlayStation's scaled drawing on the GPU (wgpu compute shaders), replaying what the software GPU records |
| `crates/ps2-gpu` | The PlayStation 2's Graphics Synthesizer on the GPU (wgpu compute shaders), drawing what the software GS records |
| `crates/web` | WebAssembly bindings of the PlayStation emulator |
| `crates/web-ps2` | WebAssembly bindings of the PlayStation 2 emulator, a module of its own, loaded only for the PS2 |
| `crates/web-disc` | Disc images in the browser, for both WebAssembly modules: sectors fetched on demand, through the worker |
| `crates/server` | The server |
| `frontend` | The web app (TypeScript, Vite) |

See [DECISIONS.md](DECISIONS.md) for the design decisions.
