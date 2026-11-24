# Design decisions

Decisions taken while building Texel, with their rationale. Each can be revisited; the "Revisit when" notes say what would change the answer.

## D1. Rust core compiled to WebAssembly, in a worker

The PlayStation (33.8688 MHz R3000A, fixed-function GPU) is well within reach of an interpreter: the core runs at about 400 fps natively and at full speed in the browser. Rust gives predictable performance without GC pauses, native unit tests and a headless runner, and small WebAssembly output (~140 KiB) through `wasm-bindgen`, with no Emscripten runtime. The core runs in a dedicated worker, so emulation never blocks the page.

*Revisit when* a JIT/recompiler becomes necessary (it should not be for this console).

## D2. Discs are streamed, not downloaded

Disc images are up to 700 MB. The core reads sectors on demand through a synchronous callback; the worker implements it with synchronous `XMLHttpRequest` and `Range` headers (allowed in workers), and the core caches 256 KiB chunks (32 MiB in total). Reads happen in the middle of an instruction, so an asynchronous API would need the core to suspend; synchronous reads keep it simple, and the CD-ROM's own seek and read delays hide the latency on a local network.

*Revisit when* playing over high-latency links: prefetching ahead of the read position, or `Atomics.wait` on a shared buffer filled by another worker.

## D3. Audio paces emulation

The worker runs frames while the audio ring buffer (a `SharedArrayBuffer` read by an `AudioWorklet`) holds less than ~70 ms. The audio clock then drives emulation speed, so there is neither drift nor crackling, whatever the display refresh rate. Without audio (no cross-origin isolation), frames are paced by the clock.

This requires cross-origin isolation (`COOP`/`COEP`), which the server sends on every response. The browser therefore cannot load third-party resources: artwork is proxied and cached by the server (which is also better for privacy).

## D4. A software rasteriser, with an optional scaled copy of VRAM

The GPU renders into an emulated 1024×512 VRAM, as the hardware does. This is exact by construction (VRAM reads by games, framebuffer effects) and simple. Edge functions with a top-left fill rule, 16.16 fixed-point attribute planes, the 4×4 dither matrix, texture windows and the four semi-transparency modes match the hardware closely.

Higher internal resolutions (2× to 4×) draw every primitive twice: into the native VRAM, which stays exact for VRAM reads, textures and save states, and into a scaled copy, which the display shows. Textures are always read from the native VRAM; dithering follows native pixels; lines keep their native thickness. 24-bit pictures (FMVs) only exist natively. The cost is the extra fill: 2× is cheap, 4× needs a fast machine.

The worker scales frames to the canvas' size on screen itself (nearest-neighbour, or bilinear with "Smooth"): CSS `image-rendering` is not reliably applied to canvases drawn from a worker.

*Revisit when* a WebGPU renderer is worth it (much higher resolutions, texture filtering).

## D5. Timing: approximate, but modelled where it matters

- Instructions take one cycle, plus their instruction fetch: through an emulated 4 KiB instruction cache (one cycle on a hit, a line fill on a miss) when the address is cached, else at the RAM, BIOS ROM or I/O access time.
- Loads cost the access time of their region; multiplications (6, 9 or 13 cycles by operand size), divisions (36) and GTE commands (their documented durations) stall the CPU only when their result is read too early.
- DMA transfers complete at once but stop the CPU for as long as they take.
- Peripherals catch up with the CPU on every I/O access, so that polled timers and status registers read exact values, and at least every 64 cycles otherwise.
- GPU drawing takes time: one GPU cycle per pixel, plus one for texturing and one for semi-transparency, two per pixel for VRAM copies, half for fills, plus a setup cost per primitive (counted on the native VRAM, so the internal resolution does not change it). The CPU waits for it, as it does on hardware when it sends commands faster than the GPU draws (DMA transfers halt it). Games keep their frame rates (Gran Turismo races at 25 page flips per second, its menus at 50; Rayman's FMVs at about 17), and software that draws as fast as it can no longer redraws many times per frame.
- CD-ROM response delays and DMA timing in detail are approximations.

The core runs at about 260 to 340 frames per second natively, depending on the game.

## D6. One shared disc crate

CUE parsing, the sector layout (pregaps, multi-file images, ISO presented as Mode 2 Form 1) and ISO 9660 live in `texel-disc`, used by both the core and the server: the server reads `SYSTEM.CNF` with the very code the emulator boots with.

## D7. Identification and verification: Redump, through two DATs

- libretro's Redump-derived DAT maps serials (from `SYSTEM.CNF`) to Redump names: identification is instant, without hashing.
- Redump's own DAT lists every track's size and SHA-1: games are matched by track sizes when they have no serial, and verified in the background by hashing (cached by path, size and modification time).
- Redump's BIOS DAT identifies BIOS dumps; it has not changed since 2016, so it is pinned by SHA-256.

Names follow Redump, which is also how libretro-thumbnails names its artwork, so artwork lookups need no fuzzy matching.

## D8. Game details from Wikipedia and Wikidata; IGDB optional

Descriptions come from the English Wikipedia article (searched by title, accepted only when its short description says it is a video game and not a series, preferring articles that mention the PlayStation), and genres, companies, modes and the release date from its Wikidata item, which must list the PlayStation (Q10677) among its platforms when it lists any. Neither needs an account. IGDB needs Twitch client credentials, so it is optional: it fills what Wikipedia and Wikidata lack.

Redump's titles are not quite the articles': "Crash Bandicoot 3 - Warped" (Redump writes colons as " - ") is "Crash Bandicoot: Warped", "Dragon Ball - Final Bout" is "Dragon Ball GT: Final Bout", "Disney's Tarzan" is "Tarzan (1999 video game)". An article is accepted when the words its title (without its "(video game)" qualifier, accents folded) and the game's share make at least three quarters of all their words, the game's title being also tried without a leading possessive brand ("Disney's"); an article with a number the game's title lacks is a sequel, and refused. Enough for these, not for "Crash Bandicoot" against "Crash Bandicoot 3 - Warped" or "Destruction Derby" against "Destruction Derby 2". The title itself, with its colon back and in title case ("Disney's Action Game Featuring Hercules"), is also tried as a page name, which follows Wikipedia's redirects; an article reached that way is accepted whatever its title, as the redirect says it is the same game ("Spyro 2: Gateway to Glimmer", the European title, leads to "Spyro 2: Ripto's Rage!"). Releases of a single non-English country (France, Germany...) without English have their local title in Redump ("Nickelodeon Les Razmoket a Paris - Le Film"): when English Wikipedia has nothing, that country's Wikipedia is searched, and its article's Wikidata item leads to the English article (or the local one is used).

A game without an article may still have a Wikidata item ("Disney's The Little Mermaid II" only has a Russian article): it is then searched there by label and alias, and gives the details, without a summary. Wikidata items are sometimes incomplete (no developer or publisher for Men in Black): what they lack is read from the English article's infobox, where links and list templates are unwrapped, regional release templates read for their values, and only the PlayStation's entries kept when entries are marked by platform ("The Collective (PS1)").

When all that fails (a title several games share: Redump's "Bomberman (Europe)" is Bomberman Party Edition, which nothing in Wikipedia or Wikidata links to that name), a YAML file next to the game's image says where to look: a Wikipedia article (title or URL, any language), a Wikidata item, an IGDB slug, and other links. The Wikipedia and Wikidata pages replace the search, and their item's platforms are not checked (they were chosen); other sites (Eurogamer, IGN) have no open API and are only linked to, not read. The file is parsed strictly (unknown keys are refused, with serde-saphyr, as serde_yaml is unmaintained), and its content is part of the cache key of the game's details, so an edit takes effect at the next scan.

Artwork comes from libretro-thumbnails, which stores some images as git symbolic links (the discs of a multi-disc game point to one cover): GitHub serves those as the target's name, which the server follows.

Details are cached for 30 days (a lookup that found nothing, for a day) and prefetched in the background, a game every 3 seconds, so the game list shows them without waiting for third parties. Wikimedia rate-limits bursts: an error (a 429, say) fails the lookup, which is then neither cached nor shown and is tried again, rather than recorded as "nothing found". A short description is accepted as a game's when it names a video game, or any "… game" ("1999 action role-playing game") but a board, card or tabletop game, a game show or a series. Wikidata rate-limits: when its part fails, the rest is shown but not cached, so that the next request tries again.

## D9. Saves live in a database; caches on disk

Memory cards, save states, the BIOS of each game and the verification hashes are kept in MariaDB (or MySQL), through sqlx: atomic writes, backups with the cluster's other databases, and no writable volume to keep. The server applies its migrations (`crates/server/migrations/`, embedded in the binary) at startup, after waiting up to a minute for the database, and reports unready while it is unreachable. Caches (DATs, artwork, game details) can all be fetched again: they stay in the state directory, an `emptyDir` in Kubernetes. The migrations were reset to a single one before the first deployment (D44): there is no older layout to import.

Each game gets its own two cards (raw 128 KiB images, compatible with other emulators), so saves follow the user across browsers and devices, and one game cannot fill another's card. Saves are keyed by the disc's serial (from `SYSTEM.CNF`), not by its file: converting a game to another image format, or replacing a dump with another (an EDC variant, a revision), keeps them. The discs of a multi-disc game keep their cards (and BIOS choice) with the first disc's serial, so they share them; each disc keeps its own save states. Discs without a serial fall back to their file (`file-<id>`). Two library entries with the same serial (a translation patch, say) share their saves. The worker uploads a card when the game wrote to it (checked every ~2 s, and on pause and exit). There are no user accounts: one deployment is one player's library.

Save states serialise the whole machine with serde and postcard (CPU, RAM, VRAM, sound RAM, every peripheral), behind a format version and a fingerprint of the BIOS: a state only loads with the same BIOS and format. So the BIOS a state was saved with is recorded with it, and the player disables loading it under another. Each game also remembers the BIOS it was last started with (else its latest state's), which its page preselects. The disc and the memory cards are not in states: loading a state keeps the cards as they are, as on a real console. States (~3.4 MB, ~0.6 MB once gzipped by the browser) and their screenshots are stored per disc, in four slots.

*Revisit when* several people share a deployment: per-user saves behind the ingress' identity header.

## D10. No authentication in the server (superseded by D44)

The deployment is private and sits behind an authenticating ingress, which is better at it (OIDC, SSO). The server serves copyrighted material to whoever reaches it, which the README and the manifest state.

## D11. Compressed images are decompressed by the server

CHD (through the `chd` crate) and PBP (read directly: blocks of 16 raw sectors, deflated) are turned into virtual BIN files and a generated CUE sheet. The browser sees every format the same way, and stays small: no decompressors in the WebAssembly module. The server keeps a few containers open, with their last decompressed hunk or block. Encrypted PBPs (from the PlayStation Store) are refused.

## D12. Discs are changed through the lid

Changing discs opens the emulated lid (the drive reports it, reads fail), waits 1.5 s, then closes it on the new disc, as a person would. Games only expect this when they ask for another disc: opening the lid while a game streams from the disc may crash it, as it could on a console.

## D13. Region-free BIOS and the drive's region

On a console, the CD drive reports the disc's region and the BIOS refuses discs of other regions. Here, the drive reports the BIOS's region, so that more combinations boot: North American BIOSes boot European discs. Japanese BIOSes still refuse European discs: they also check the licence text on the disc. With a region-free BIOS (the PSP's PS1 emulator, recognised by its fingerprint, or OpenBIOS, recognised by its name in the ROM), the drive reports the disc's real region, read from its licence sector, as it would on hardware, for games that check it.

## D14. OpenBIOS and zero-block DMA

OpenBIOS starts block-mode DMA transfers with a block count of 0, meaning none, as PCSX-Redux (which it is developed with) treats them. Treating 0 as 65536 blocks, as for the block size, sent unrelated memory to the GPU. DMA block counts of 0 now transfer nothing. Before GPU drawing time was emulated (see D5), OpenBIOS's boot animation redrew about seven times per displayed frame, and ran at a quarter of the usual speed.

## D15. Gamepad layouts

Browsers report a gamepad's buttons in the W3C "standard" layout (Cross, Circle, Square, Triangle, then shoulders...) only for the models they know: they say so with `mapping === "standard"`. Other pads report buttons and axes in their own order, often with the D-pad as a single "hat" axis taking one value per direction. For those, the player toolbar shows "Set up gamepad", which records each control as the user presses it: a button, an axis pushed to ±1 (triggers), or an exact axis value (hat D-pads), then each stick axis and its direction. Layouts are kept in `localStorage`, per gamepad id (the model, as the browser names it), so they apply to every game but only in that browser. Game input is ignored while the dialog is open. A skipped control is left unbound.

## D16. Netplay: lockstep rooms, relayed by the server

Two ways to share a game exist: one browser runs it and streams video to the others, or everyone runs it and exchanges input (lockstep). Lockstep was chosen: everyone gets full-quality video and audio, and the network carries a few bytes per frame and member. It relies on the emulator being deterministic, which save states already showed: from the same state with the same input, two runs produce identical frames. Nothing in the core depends on wall-clock time, and the internal resolution only changes what is drawn, not the emulated timing, so each member may choose their own.

- Transport: a WebSocket from each emulator worker to the server, which gathers the members of a room (up to 8) under a 6-character code and routes their messages, to everyone or to one member, without reading the game's. Friends reach the same protected server anyway, so this needs no NAT traversal or relay server of its own. The server also keeps the room's names and chat (the last 100 lines, for newcomers) and answers pings, so that each member knows its round trip to it.
- Each member sends its input for frame f + its delay before running frame f. A frame runs once the host's input for it is known, as it carries who owns each controller, and the input of every owner: those who only watch never hold anyone up. Each member's delay covers the trip of its input through the server to the slowest other member (half of both round trips, in frames, plus one), and is adjusted every 2 seconds. Rollback (running ahead, then replaying on late input) was not done: it needs the emulator to run several times faster than real time.
- A controller has a list of owners, whose inputs are combined: several members can share one, for one-player games. Member ids are never reused in a room: when a member leaves, every member has received the same inputs from them, and their controllers go idle.
- A member joining asks the host for its state: it gets the machine's state, the memory cards and the inputs known for the frames ahead. A member far behind (its tab was hidden) asks again; one a little behind runs frames faster to catch up.
- Desyncs are detected every 120 frames by comparing a checksum of the serialised state with the one of the member with the controls (the host if it has one, else the owner of the first controller, then of the second). A member whose checksum differs asks it for its state, and replays from its input history the frames it already ran past it. There is no protection against a member sending false input or state: rooms are for friends.
- The split screen is applied when drawing, by each member's browser: it is not part of the game, so it cannot desynchronise it, and like the rest it trusts the members.
- Rooms live in the server's memory, and close when their host leaves. A browser sends something every second; a socket silent for 30 seconds is closed, as pages frozen in the back/forward cache can keep theirs open.

## D17. Audio waits for a click, the game does not

Browsers start audio only once the page has been clicked or a key pressed. The player used to wait for it before starting the game, which never happened on a page opened from a link (a netplay invitation, with a saved name, joins without a click). Now the game starts at once, paced by a timer, and switches to the audio clock (see D3) once the first click or key press lets the audio play.

## D18. DMA interrupts: the master flag, also on DICR writes

The DMA interrupt (IRQ3) is raised when DICR's master flag (bit 31) goes from 0 to 1, and that flag only counts the flags of enabled channels. It rises when a transfer ends, but also when a write to DICR enables a channel whose flag is still set: both raise the interrupt. The second case matters at boot. The BIOS's CD-DMA handler takes every DMA interrupt as the CD's, so when one comes from a GPU transfer during the boot animation, the GPU's flag is left set. Games disable every channel at startup (DICR = 0, which does not acknowledge), then enable the ones they use: the master flag rises, their handler acknowledges the old flag, and later transfers raise the interrupt again. Raising it only when transfers ended left that flag up for good, so that no DMA interrupt was raised again; games whose movie player waits for its DMA callbacks (Disney's Tarzan, with any BIOS) stopped at their first movie.

## D19. LibCrypt: SBI files, and the CPU's debug registers

LibCrypt (European discs, from 1999) reads, with GetlocP, the subchannel Q data of 32 sectors that were corrupted on purpose when the disc was pressed; a copy reports their positions correctly, and the game refuses to go on (Disney's Tarzan stops at its copyright screen) or misbehaves later. CUE/BIN and CHD images do not keep subchannel data, so Redump publishes it for each protected disc as an SBI file: the disc crate reads it, and the CD-ROM controller answers GetlocP from it for the sectors it lists. SBI files sit next to the image they belong to, named like it, or in a shared directory (D53); the server serves them with the disc, and verify.py checks their format (Redump's DATs do not list them).

LibCrypt also keeps its state in the CPU's debug registers (COP0 BPC and BDAM: a pointer to its table of sectors, and the key it builds): they now keep what is written, though their breakpoints are not emulated. They are saved after the rest of save states (format 3; format 2 states load with them at zero).

## D20. One library directory per console

The library has a directory per console (`ps1/`, and `ps2/` for later), each with `bios/` and `games/`; each of the four can also be set apart (`TEXEL_PS1_BIOS`...), to keep games on another volume. Inside the server, files keep their paths relative to their kind of directory (`games/SLES-00049/disc.chd`, `bios/original/scph1002.bin`), whatever directory that is: game ids and verification hashes are keyed by them, so moving the library changes neither. The PlayStation 2 directories are only reported at startup until it is emulated; each console's helper scripts (verify.py, fetch-openbios.sh) live next to its directories and find them relative to themselves.

## D21. Not emulated yet

Cycle-accurate DMA, GPU and CD-ROM timing (GPU drawing time is modelled per pixel, not per cycle), SPU Gaussian interpolation, multitap, mouse, light guns, serial port 1 and encrypted PBPs.

## D22. One emulator crate per console

The PlayStation emulator lives in `crates/ps1` (`texel-ps1`), formerly `crates/core`: a PlayStation 2 emulator will be a separate crate, `crates/ps2`, beside it, rather than a mode of the same core. The two machines share almost nothing at the hardware level (the PS2's IOP is a PS1, but runs alongside an Emotion Engine, a VU pair and the GS), and keeping them apart keeps each WebAssembly build small. What they do share stays in common crates: `texel-disc` reads both consoles' CD images (and will read DVD images), and the server's library, metadata and database are console-agnostic apart from the paths they are given.

The library already scans both consoles: PS2 games and BIOSes are listed under paths starting with `ps2/` (PS1 paths keep their bare `games/` and `bios/`, so ids and hashes do not change), a disc is told apart by its `SYSTEM.CNF` (`BOOT2` on the PS2), DVD images are read as plain or CHD ISOs, and PS2 BIOSes are identified by the `ROMVER` file of their ROM directory. Each console has its own DATs (Redump's games and BIOSes, libretro's serials) and thumbnails, so PS2 games are named, verified and illustrated like PS1 ones; they show as not playable until their emulator exists.

## D23. The PlayStation 2's main CPU, first

The PS2 emulator starts with the Emotion Engine's CPU (the R5900: MIPS III and parts of MIPS IV, 128-bit registers, multimedia instructions), its coprocessors (COP0, the FPU; VU0 register transfers only), and the hardware the BIOS kernel sets up before anything else: the interrupt controller, the timers, the RDRAM controller, and the GS's privileged registers. It is an interpreter, like the PS1's. That is enough for the BIOS kernel to start, print its start-up log ("# Initialize GS ... # Initialize Done.") on its debug port, and wait for the IOP over the SIF: the next steps are the GS with its DMA paths, then the IOP (a PS1 CPU, which may reuse `texel-ps1`).

Addresses go through the TLB, as on the console: kseg0 and kseg1 are fixed windows on physical memory, and everything else is mapped by the 48 entries the kernel writes (the scratchpad included: an entry with its S bit maps all 16 KiB of it, whatever its page size). Misses, invalid entries and writes to read-only pages raise the exceptions the kernel handles. So that this costs nothing per access, the entries are expanded into a table with one word per 4 KiB page, rebuilt when an entry is written or the address space ID changes; translating is one table read.

One shortcut remains, until something needs more: each instruction takes one cycle, and there are no caches. Rough costs for slow instructions, and a scheduler that keeps the EE, the VUs, the GS and the IOP in step, come with the IOP; caches only for the few games known to need them.

Speed is the open question. The interpreter runs at about 180 to 200 million instructions per second natively here, below the EE's 294.9 MHz, before the VUs, the GS and the IOP add their share; WebAssembly is slower still. Full speed in the browser will take compiling EE code to WebAssembly at run time (a recompiler), and may remain out of reach for demanding games.

## D24. The IOP: the PlayStation's CPU, shared

The IOP is the PlayStation's R3000A, so it runs on `texel-ps1`'s CPU: that CPU is generic over a small bus trait (`CpuBus`: fetches, loads and stores, the cache switch, the interrupt line), and has a configurable processor ID, which is how the BIOS tells the IOP (0x1F, as PCSX2 has it) from the EE. The PlayStation keeps its own bus unchanged. The IOP's memory map, interrupt controller, six timers and DMA registers are `texel-ps2`'s, in `iop.rs`; it runs at an eighth of the EE's clock, interleaved with it every 32 EE instructions.

The SIF connects the two: shared mailbox and flag registers, and two FIFOs fed and drained by DMA (EE channels 5 and 6, IOP channels 9 and 10), moved whenever either side has something to give and the other room to take. Of the IOP's devices, the BIOS needs answers from the CD/DVD controller (status, the MechaCon's version and clock: no disc yet) and from SIO2 (controllers and memory cards: none yet); the others keep what is written.

The CD/DVD controller's EEPROM (NVM) is emulated as a fresh one would be, as PCSX2 creates it: the region parameters of the BIOS's region, a dummy iLink ID, and the default system configuration (English, except in Japan and China); S commands answer as many bytes as the BIOS expects, from PCSX2's command table. The SPU2 makes no sound yet, but its status follows its DMA transfers, which take time (the sound libraries wait for a transfer to be under way, then finished).

With these, a retail BIOS boots its two CPUs, resets the EE twice (for EELOAD, then OSDSYS), loads OSDSYS from the IOP, reboots the IOP with the browser's modules, uploads its sounds, sets up the display and runs its drawing loop: VU0 macro instructions, and DMA to the GIF and VIF1, none of which is emulated yet. That is where step 3 ends and step 2 (the GS, the GIF and VIF paths, VU0 and VU1) begins.

Hardware facts (S command result sizes, the EEPROM layout and defaults, the SPU2 status bits) come from PCSX2's source; no code is taken from it (GPL).

## D25. The PS2's graphics: in software first

The Graphics Synthesizer is emulated in software, in Rust, as the reference for anything faster: its 4 MiB of local memory holds every pixel format in the GS's own layout (pages, blocks and columns per format), because games upload in one format and read in another, render to textures and reuse frame buffers as palettes. The rasterizer samples pixels at their top-left corner with a top-left fill rule, and runs the whole pixel pipeline per fragment: the texture function (with palettes and the four wrap modes; nearest sampling for now), fog, the alpha, depth and destination alpha tests, blending, and the write masks. The display circuits read the frame buffer out as RGBA.

Its inputs are emulated in order, without a schedule: DMA transfers to the GIF and the VIFs complete when started, VIF codes run as they arrive, and microprograms run to their end when started (MSCAL, VCALLMS, CMSAR1), so XGKICK reaches the GIF in the order the program meant. The vector units execute each instruction pair with both halves reading the registers as they were (the upper result winning on conflicts), Q arriving after DIV's latency, and the PS2's float rules; the other pipeline delays (flags especially) are not modelled yet.

With these, a retail BIOS shows its intro (the Sony Computer Entertainment swirl, then the PlayStation 2 logo) and, with a fresh EEPROM, its first-boot language screen, which waits for a controller. A GPU renderer (WebGPU, with the software one as its reference) and the speed of the whole (the EE at about 50 MHz of its 295 while the GS draws) are next, with controllers.

## D26. The PS2 in the web app: a module of its own

The PS2 emulator is a WebAssembly module of its own (`crates/web-ps2`), next to the PlayStation's, so that PlayStation games do not download it; the worker imports it only when a PS2 session starts. Its interface follows the PlayStation's where they overlap (frames, controllers, rumble), so the worker drives both the same way, and the features it does not have yet (discs, memory cards, save states, play together) are simply not offered. Until discs are read, a PS2 session is a console booted to its browser, started from the BIOS page.

Controllers are the PlayStation's DualShock (`texel-ps1`), which the PS2 accepts, behind SIO2: the IOP's driver starts a transfer before its DMA brings the bytes to send, so a transfer runs once they have all arrived (or when its results are read), and raises the SIO2 interrupt then.

## D27. PS2 speed: where the time goes

Profiled natively while the browser runs its intro and menus, the software GS takes about two thirds of the time: the browser shades about 2.3 million pixels per frame (full-screen passes with textured sprites, mostly), at about 17 ns each, bound by the GS memory's accesses rather than by computation. The EE interpreter takes most of the rest.

What was done: triangles and sprites step their attributes per pixel instead of interpolating each, the frame buffer is read only when blending, a mask or the destination alpha test needs it, the timers, interrupts and SIF are updated every 128 EE instructions instead of 32, and the EE's idle loop (NOPs and a branch back to them) is detected and skipped: while the EE spins there, time passes without instructions running. Together: from about 50 to 82 million EE cycles per second natively, and from 7 to 10.5 frames per second in WebAssembly.

What full speed needs: the GS on the GPU (its local memory in a GPU buffer, in its own layout, drawn by compute shaders), which the software GS remains the reference for; then the EE's code compiled to WebAssembly at run time.

## D28. The GS on the GPU: compute shaders over the GS's own memory

The GPU GS (`crates/ps2-gpu`, wgpu) keeps the GS's 4 MiB in a storage buffer, in the GS's own layout, and draws with compute shaders rather than the graphics pipeline: games read their frame buffers back as textures and palettes in other formats, and write one format over another, which a GPU's own texture formats cannot express, while compute shaders address the memory as the GS does (each format's layout as bit operations). The software GS stays the reference: it records what it is asked to draw (primitives with their registers, uploads, copies, palette loads), and the GPU replays the record, or both run and are compared.

Primitives are set up on the CPU (edges, attribute planes, page ranges) and drawn in batches: one dispatch over a batch's area, a thread per pixel running the batch's primitives in order, so a pixel's blending and depth test see the primitives before it. Threads run in no order between pixels, so a batch ends before a primitive that reads (as a texture) memory the batch writes, writes memory the batch reads, or writes the same memory through another layout. Palette loads go to a ring of palette slots, so a batch's primitives keep the palette they were drawn with. The same wgpu code runs natively (Vulkan, Metal, DirectX 12) and in browsers (WebGPU), within WebGPU's default limits.

Measured on two machines while a BIOS boots (12 s, from its intro to its menu), the GPU takes the GS's time away almost entirely: on an Apple M1 Pro (Asahi Linux's Vulkan driver), from 82 to about 300 million EE cycles per second (the EE runs at 295: faster than the real console, on average); on an AMD Ryzen 9 5950X with a Radeon RX 6900 XT (Mesa's RADV), from 66 to about 260. Profiled on the latter, the GPU driver takes 0.1% of the time and the EE interpreter nearly all of the rest: compiling the EE's code is the next speed-up, and the slower machine's single-thread speed is what separates the two.

Both drivers draw the same memory. The comparison with the software GS differs in two known ways: attributes are interpolated in 32-bit floats on the GPU and stepped in 64-bit ones in software, so depth (32 bits) and colours land one apart now and then (about 5% of the memory, mostly the depth buffer, during the BIOS intro's 3D); interpolating in fixed point in both, as the GS itself does, would make them agree exactly. And lines whose pixels share a column (a step shorter than a pixel along the major axis) are drawn once on the GPU, twice in software. Line pixels themselves are computed exactly, in integers, in both.

A shader compiler (Asahi's Honeykrisp) miscompiled a memory read with a load in each case of a `switch`: the reads are written with a single load after it.

## D29. The GPU GS in the browser: WebGPU on the worker's canvas, software otherwise

In the web app, the PS2 module draws with the GPU GS when the browser has WebGPU: the worker gives it the canvas (an OffscreenCanvas), wgpu makes a WebGPU surface of it, and each frame goes from the GS memory to the canvas without coming back to the CPU (the display circuits' compute shader, then a render pass scaling the picture to the canvas, nearest or bilinear as the player's setting says). The software GS then only records. A canvas has one kind of context for good, so the worker asks for a WebGPU adapter first, and gives the canvas to WebGPU only when there is one; otherwise, or if setting up fails before taking the canvas, the console draws in software on a 2D context, as before.

Measured in Chromium (headless), booting a BIOS: on an AMD Radeon RX 6900 XT, about 23 frames per second through the intro and 50 to 63 in the first menus, against 8 to 10 in software; even on a software Vulkan device, about 20 and 40. The EE's interpreter, in WebAssembly, is now what limits it.

Chrome's WGSL compiler reserves words wgpu's accepts (`smooth`): shaders are checked in a browser too, not only natively.

## D30. The EE's code compiled to WebAssembly, block by block, exactly as interpreted

In browsers, the EE's code is compiled to WebAssembly as it runs (`crates/ps2/src/ee/jit`). A block starts where the EE is (not in a delay slot) and runs to a jump and its delay slot, up to 64 instructions in its 4 KiB page; conditional branches leave it when taken and it goes on when not (a superblock). The integer instructions, the branches, and loads and stores to RAM are compiled (addresses translated by reading the TLB's page table directly); everything else, and every unusual case (a TLB fault, a table to rebuild, memory other than RAM, writes to compiled code), calls the interpreter for that one instruction, which stops the block if it took an exception or changed where the EE goes. A block is compiled the second time it runs, into a module of its own, which the browser compiles and instantiates with the emulator's own memory and function table; its function goes into that table, and the EE calls it from there, through a direct-mapped cache of the blocks run lately (valid while the address mapping has not changed). Writes to RAM lines (64 bytes) holding compiled code drop their blocks.

The interpreter stays the reference, exactly: blocks count their instructions as the interpreter does (one cycle each, brought up to date before any instruction they leave to it, so COP0's Count reads the same), and a block only starts when its first segment fits in what is left of the slice, each later segment checking it too, so slices end on the same instruction. Booting a BIOS compiled and interpreted gives the same registers, memory and GS memory every second (`frontend/scripts/check-ps2-compiler.mjs`), which is how the compiler is tested; natively, its modules are validated (`wasmparser`), as nothing runs them there.

Compiled blocks reach the EE and the bus by address, and Rust calls them by function-table index: that takes unsafe code. The workspace denies unsafe code (it forbade it until then): a module that needs it allows it, says why, and keeps it to the least, with a `SAFETY` comment on each block. The compiler's lives in one module of the PS2's WebAssembly bindings (`crates/web-ps2/src/jit.rs`, two unsafe blocks); `texel-ps2` has none.

Profiling then showed time spent idling. The EE's idle loop was skipped only when found at the start of a slice: it is now skipped wherever the EE enters it (the rest of the slice passes). The IOP spent nine instructions in ten in its kernel's idle thread (a jump to itself): after one instruction per slice (taking any interrupt raised), the rest of its slice passes when it is there. With both, natively, a BIOS boots at about 380 million EE cycles per second with the GPU GS (from 300); in WebAssembly, without graphics, from 24 to about 58 frames per second compiled (52 interpreted). In Chromium, with the GS on the GPU, the BIOS's menus run at full speed and its intro at 25 to 30 frames per second.

## D31. PS2 discs: the drive as documented, and a fast boot

The CD/DVD drive reads discs as ps2tek documents it: N commands (seek, read in CD blocks of 2048, 2328 or 2340 bytes, or DVD blocks of 2064 bytes with their header) take the time a drive takes, the blocks go to IOP DMA channel 3 as they are read, and the CDVD interrupt comes when the command completes; the drive and sticky status, the disc type (a PS2 DVD for an image of 2048-byte sectors, a PS2 CD otherwise: Redump keeps PS2 CDs as raw BIN/CUE) and the error register are what the drivers poll. The EE reads the drive's registers too (EELOAD checks the disc type), so they are on its bus as well. Discs are read through `texel-disc`, natively from files and, in the browser, through the worker's range requests (`texel-web-disc`, shared with the PlayStation's module).

Seeks take half the times ps2tek gives (about 15 ms for a short one, 50 for a long one; spinning up still takes a third of a second). Those figures are an emulator's estimates; with them, Jak and Daxter's sound driver (OVERLORD) loses a race against the EE: its loader thread sends its main thread a message from its own stack, and if the EE's first request arrives before the main thread polls for it (every 0.1 ms), the loader overwrites the message while handling the request, and the IOP crashes. Halved, the race goes the console's way, and further changes nothing.

Games boot directly (a fast boot): the BIOS starts as always, its kernel starts EELOAD, and EELOAD, which would start the browser (`rom0:OSDSYS`), starts the disc's executable instead (from SYSTEM.CNF). Its target is a string in its own memory, followed by the test mode's (`rom0:TESTMODE`), which a fast boot never uses: while the EE runs EELOAD, the target is replaced by the disc's, every time EELOAD is loaded again (when the IOP restarts), as long as the room is there (32 bytes; otherwise the browser starts). Booting through the browser instead would need the drive's MagicGate checks (the PS2 logo is decrypted with keys the drive derives from the disc), which are not emulated.

The IOP's modules print a lot (what they load, what fails), to nowhere on retail consoles. Every module calls stdio's `printf` through an import stub, which loadcore links to `printf` itself and which keeps its function number: `printf` is found by scanning IOP memory for stdio's linked import tables, and its calls are formatted from the IOP's registers and stack (`iop_printf`, a diagnostic the boot example turns on).

## D32. PS2 memory cards, and the transfers they need

Memory cards answer on SIO2's ports 2 and 3 as ps2tek's memory card page describes their commands; where each reply byte goes is what ps2sdk's open-source MCMAN and SECRMAN check (the acknowledgement 2Bh before the terminator at the end of short replies; data, their XOR and the terminator after the acknowledgement and terminator of reads; the new terminator after setting it). A card is 16384 pages of 512 bytes and 16 of ECC, kept as the IOP writes them, erased by blocks of 16 pages (to FFh); a new card is unformatted. MagicGate, the authentication SECRMAN runs between the card and the drive's MechaCon, is not computed: the card answers its vectors with zeros (and their XOR), and the MechaCon's commands 80h to 8Fh answer success, which is all SECRMAN checks of them.

Two SIO2 details matter for cards, whose drivers send several commands per transfer: by DMA, each command's bytes start a block of channel 11 (the rest of the block is padding), and each reply goes at the start of its own block of channel 12; and channel 12 waits for its transfer, which the drivers start after it.

Jak and Daxter also showed scratchpad DMA modes that were missing: interleave (after every TQWC quadwords, memory skips SQWC, from D_SQWC) and source chain with TTE to the scratchpad (each tag goes there before its data). Without them, the game's data in the scratchpad was wrong, and the EE soon read past its end. With them, the game draws its own screens and reaches its first 3D scene, where it stops for now.

## D33. PS2 sound: the SPU2's voices are the PlayStation's

The SPU2 is emulated with the PlayStation's voice: its ADPCM decoding, ADSR envelopes and volume sweeps are shared with `texel-ps1` (made public there, its own state unchanged so its save states still load). Around them, the SPU2's own: two cores of 24 voices, 2 MiB of sound memory addressed in halfwords, 48 kHz output; each core mixing its voices, its sound data input (fed by auto-DMA: streamed PCM, in blocks of 256 left then 256 right samples) and its external input (core 0's output, for core 1), as MMIX and the VMIX masks say, at their volumes (master, input, external). Transfers: the data port, manual DMA both ways (channels 4 and 7, taking time as before), auto-DMA (complete when the core has played all but a block). Interrupts on the IRQA address, for voices and transfers, through IRQINFO and the IOP's interrupt 9. The register layout follows ps2sdk's libsd headers and PCSX2's register names.

Reverb is emulated too, as the PlayStation's (the same algorithm, its registers and work area per core, at 24 kHz), fed by the voices and inputs the VMIXE masks and MMIX send to effects. The browser plays the PS2 at 48 kHz (the PlayStation at 44.1).

Sound also unblocked Jak and Daxter, which waited for its sound driver: it now runs its opening flight through the jungle.

## D34. PS2 save states: the machine serialised, the GS memory beside it

A PS2 state is the PlayStation's design (postcard, behind a tag and a format version, checked against the BIOS), over every part of the machine: both CPUs, their memories, the VUs and VIFs, the GIF and GS, the SPU2, the drive, the SIO2, timers, DMA and interrupts. Left out, as for the PlayStation: the BIOS (checked by hash), the disc and the memory cards (kept where they were; the cards are saved apart, by the server); and what is rebuilt (compiled code, dropped on loading; the TLB's lookup table) or only for diagnostics.

The GS memory (4 MiB) and the CLUT are stored beside the rest rather than in it: when the GPU draws, they are on the GPU, not in the software GS, so the state is taken in two steps, the machine at once and the GPU's memory read back (asynchronously in browsers, from a copy made at the same moment), and put back on the GPU on loading. A state is about 39 MB, 11 MB once the browser has compressed it, which MariaDB's default packet size (16 MiB) could soon refuse: the database is run with 64 MiB.

Checked with Jak and Daxter: a state saved at 20 seconds and run for 5 is identical, byte for byte, to the state of a console run for 25.

## D35. Jak and Daxter's drawing errors: VU1's TOP wraps at ten bits

Jak and Daxter drew huge triangles, stretched towards the top left of the screen. Diagnostics in the boot example (`DRAWS`: the GS commands of a frame, by draw context; VU1's packets by program, with `PROG` tracing a program's run and its microcode then, and `FINDXY` the GIF path a vertex came through) showed where: some VU1 packets held stale memory after their first vertices. The program that built them reads TOP (XTOP) to know which of its double buffers it works in, and compares it with zero; with BASE and OFFSET at 0x200 each, TOPS was 0x400 here, past the end of VU1's 1024 quadwords. Memory accesses wrapped, but the comparison did not, and the program set up the wrong buffer. TOPS is a ten-bit register: it now wraps, as do UNPACK addresses relative to it. The VUs' PC wraps at the end of micro memory too (Jak branches backwards across address 0 into code at the top), so that links saved by BAL and JALR are what the hardware saves.

With this, the logo, the title sequence and the jungle draw; some large triangles remain, sent by the EE through VIF1's DIRECT (path 2), which the VU1 fix does not reach. Drawing correctly costs: the jungle keeps VU1 busy at about its hardware limit (some 150 million instructions per second), several times what the VU interpreter does, which makes VU1's speed the next limit (compiling microprograms, as the EE's code is).

## D36. Input volumes are plain signed values

Tekken 3's music played only some of the time: its XA streams were read and decoded, then mixed at the CD input volume (1F801DB0h), which the SPU doubled as if it were a 15-bit fixed volume. Games setting 3FFFh were heard; Tekken 3 sets 7FFEh, which doubled wrapped to -4/32768, silence. The CD input volume is a signed 16-bit value, used as is; so are the SPU2's EVOL, AVOL and BVOL, which had the same mistake. Only the voices' and the main volumes, in their fixed mode, are 15-bit values to double.

## D37. PlayStation speed: a leaner rasteriser, and scaled drawing on the GPU

Profiled, the PlayStation spends little on its CPU and much on drawing at higher internal resolutions: every primitive is drawn twice, at native resolution into the real VRAM (games read it back, and textures come from it), then scaled. At 4×, Crash Bandicoot ran at 63 frames per second natively and 46 in WebAssembly: below the 50 a PAL game needs.

The software rasteriser now draws the same pixels with less work: each triangle row's span comes from its edge functions (the pixels where they are non-negative, solved exactly), rather than from testing its bounding box; what a primitive's pixels need (texture window, page, CLUT, blending) is worked out once per primitive; loops are specialised for textured and shaded primitives; rectangles step their texture coordinates instead of dividing; the displayed picture is converted through a table, a row's contiguous runs at a time. The CPU no longer copies its 32 registers after every instruction to model the load delay: the load landing is applied after the instruction, unless the instruction wrote the register. Frames and sound are identical, bit for bit, on three games at 1× and 4× (3000 frames each); at 4×, natively, from 63 to 117 frames per second (Crash Bandicoot), 98 to 160 (Tekken 3), 105 to 181 (Crash Team Racing); in WebAssembly, from 46 to 99. SIMD instructions in WebAssembly gained nothing: the compiler does not vectorise the rasteriser.

Where the browser has WebGPU, scaled drawing now happens on the GPU (`crates/ps1-gpu`), as the PS2's GS does. The software GPU still draws native resolution, exactly; it records, instead of drawing scaled, what changes the scaled VRAM: primitives set up as it would draw them (edge functions, attribute planes, spans), fills, copies and transfers (with the mask decisions taken on the native VRAM), and the native VRAM textures are read from, when it was drawn since last sent. Compute kernels replay them with the same integer arithmetic (the attributes' 64-bit sums emulated with 32-bit words), so that the GPU draws what the software would: the compare example checks it, and three games matched the software at 2×, 3× and 4× on every compared frame. Triangles and rectangles are batched (a thread per pixel, the batch's primitives in order); lines, whose scaled blocks overlap, and overlapping copies run in one thread, in order. At 4×, natively, Crash Bandicoot runs at 240 frames per second this way.

Compiling the CPU's code to WebAssembly, as the EE's, was tried (`crates/ps1/src/jit`, with the EE's encoder now shared from there): blocks that count cycles as the interpreter does, instruction cache included, and stop where it would look outside the CPU. It gains nothing as the machine is built: the peripherals catch up every 64 cycles, which bounds blocks to about five instructions, and entering a block costs what compiling saves (Crash Bandicoot: 279 frames per second compiled, 281 interpreted). It would take peripherals that say when they next need attention (an event schedule), so that the CPU runs long stretches; until then it is not turned on (nothing calls `setCompiler` for the PlayStation), and it still differs from the interpreter somewhere.

## D38. PlayStation enhancements: precise geometry, perspective, widescreen

The PlayStation rounds every projected vertex to a whole pixel and draws textures without perspective: polygons wobble as they move, seams open, textures bend on large polygons. Rendering better than the console, while emulating it as it is, takes knowing where vertices really were: the GTE now keeps its projections' exact positions and depths (as RTPS and RTPT compute them, before rounding and saturation; only when nothing saturated or overflowed) in a table on the side, keyed by the screen position they were rounded to (the word a game hands the GPU, which it copies from the GTE through memory), valid for two frames (games draw a frame during the next). When the GPU records a triangle (with the GPU drawing the scaled picture), each vertex takes its exact position when the table has it, else its rounded one: neighbours sharing a vertex agree on it either way, which a triangle all exact or all rounded did not (cracks). Such triangles are drawn with float edge functions (with the same top-left rule: on whole positions they cover the same pixels), colours interpolated linearly, and textures in perspective when every vertex has a depth. The native VRAM, what the console computes and what games read back, does not change: the table is not in save states, and netplay is unaffected. In Crash Bandicoot, 96% of triangles are drawn this way.

Widescreen is the usual one: the GTE squeezes projected X by 3/4 around the screen's centre, and the picture is shown 16:9. It changes what the console computes (3D only: 2D is stretched, and games cull what they think is off-screen, so things may appear late at the sides), so it is off in netplay. Both are player toggles (precise on by default, with WebGPU; widescreen off).

Frame rates above the game's own take changes to each game: NTSC games run at 60 Hz, but most 3D games draw a new picture every other frame (Crash Bandicoot, NTSC: 30 pictures a second, measured), with their logic tied to it; there is no general switch for that.

## D39. The GPU draws while the CPU runs

D5 made the CPU wait for every bit of drawing: GP0 writes and GPU DMA added the drawing's time to the CPU's. On the hardware the GPU draws in parallel: its command lists go by DMA as the GPU asks for them, the CPU running in between, and games draw one frame while computing the next. Serialised, games heavy on both sides ran at half or a third of their frame rate: in their fights, Tekken 2 (US) drew 30 pictures a second, Tekken 3 (US) and Tobal No. 1 20, where they draw 60.

Drawing time now runs alongside: the GPU counts it down as the console runs, and says it is not ready for commands until it is done (GPUSTAT bit 26); a command list's DMA transfer ends, with its interrupt, when its drawing does (what games wait on), the CPU running meanwhile; a GP0 write waits for the drawing under way, roughly the command FIFO; other transfers to the GPU (VRAM uploads) wait for the drawing, as before. The native VRAM is still drawn at once. Drawing under way is not in save states: a transfer waiting for it ends when a state is restored. Tekken 2, Tekken 3 and Tobal No. 1 now draw 60 pictures a second in their fights (Tekken 3 PAL 50), Castlevania: Symphony of the Night and the Street Fighter Alphas still do; Gran Turismo (races at 25 page flips a second, menus at 50), Rayman and Spyro run as before.

The headless example now also counts changed pictures (per 50 frames), which interlaced games, drawing into one buffer, need: they do not flip pages.

## D40. Without WebGPU: scaled drawing on another core, and a resolution that gives way

Drawing on a real PlayStation's pace (D39) made software drawing at higher resolutions the limit: every primitive is drawn at native resolution (exact, what games read back) and again scaled, on the emulator's one core. Tekken 3 drew its fights at about 69 frames per second at 2× in WebAssembly, against the 60 needed, and fell below in heavier scenes; without WebGPU (Chromium on an Apple M1 under Linux), its music stuttered after fights.

The scaled picture is now drawn in another worker, on another core, as the GPU does it: the emulation worker records its drawing (the recording made for the GPU, D37) and sends it, encoded, to a drawing worker, where `gpu_record::Replay` draws it with the same rasteriser and paints the canvas. Recordings are never dropped (they change what is drawn next), only pictures: what was recorded while the drawing worker was busy goes with the next round. The replay matches the software's own scaled drawing on every compared frame (Tekken 3, Tobal No. 1, Crash Bandicoot, Castlevania: Symphony of the Night; 1×, 2× and 3×; the compare example's SOFT=1). It is used for the PlayStation without WebGPU, where there are at least three cores; Rust's WebAssembly threads, which would split drawing further, need a nightly toolchain.

And when emulation, or the drawing worker, falls behind the console's pace for two seconds, the resolution drops one step (the status says so); it rises back, up to the one chosen, after ten seconds with clear room for it (a guess: drawing grows with its pixels). In Chromium without WebGPU on the development machine, Tekken 3 then fights at 60 frames per second, at 2× when 3× is asked.

## D41. The SPU writes its capture buffers

Crash Team Racing stopped on its "Sony Computer Entertainment Europe Presents" screen. Its music fades out on an SPU interrupt: the game sets the SPU's interrupt address to 0x200, inside the buffer where the SPU writes the CD audio it plays, and waits for the fade to finish. The SPU did not write those buffers, so the interrupt never came.

The SPU now writes its four capture buffers in its RAM as it plays: the CD audio's left and right samples at 0x000 and 0x400, and voices 1 and 3 at 0x800 and 0xC00. That is 512 samples each, wrapping. Each write is checked against the interrupt address, and SPUSTAT bit 11 says which half is being written. The position is not in save states: a loaded state starts it again at the first sample, as a reset would. Crash Team Racing now plays its introduction. Eleven other games' pictures, flips and sound are unchanged over 2,400 frames: Tekken 3, Crash Bandicoot (PAL and US) and eight others.

## D42. A GPU command list's transfer ends with its last command in the FIFO

D39 ended a command list's DMA transfer when its drawing did. On the hardware it ends once its last words are in the GPU's FIFO, the GPU having drawn what came before, while the last commands still draw. libgpu tells the two apart: it sends a request at once when the transfer is over (then waits for the GPU to be ready), and queues it while the transfer is busy. A request with no parameters to copy is queued as a pointer to its caller's own packet, often on the stack. Crash Team Racing clears VRAM with a one-command list, and the next request, queued, then went from a stack overwritten since: garbage commands, among them a VRAM upload of size 0 (all of VRAM) that swallowed the game's next 262,144 words, its textures and commands. Its copyright screen and introduction were garbled.

The transfer now ends when the drawing left is no more than its last non-empty node's. That node is the last command for a one-command list, and usually the small first entry (OT[0]) for an ordering table. And once a list is sent, its channel's address holds the end marker (0xFFFFFF), as on the hardware: started again without a new address, the channel sends nothing. Crash Team Racing's introduction is now drawn right. Over 2,400 frames, the other test games' page flips stay as they were, Tekken 3's included (PAL and US). One US game flips a little more often: 50 rather than 47 per 50 frames.

## D43. Starting a console without a disc, with a game's memory cards

The BIOS page started a PS2 without a disc (to its browser), but not a PlayStation. Started so, the PlayStation runs its menu: its memory card manager and its CD player. Its retail BIOSes now have the same button; the PSP's and OpenBIOS have no menu of their own and do not.

Memory cards stay per game (D9: by serial, shared by a game's discs). A console started without a disc would have had blank cards, its manager showing nothing and keeping nothing. The console page now picks whose cards go in the slots: blank ones, not kept (the default), or a game's, written back to that game as it would be by the game itself. Changing them restarts the console. The games that have saved cards come first in the list, from a new endpoint, `GET /api/memcards` (their first disc, for a game on several). Shared, console-wide cards would have matched the hardware better, but would have mixed every game's saves on two 15-block cards, and changed how saves are kept: the per-game model stays, and the BIOS's manager works on one game's cards at a time. The PS2's console page has the same picker.

## D44. Accounts: OpenID Connect, invitations, and saves per user

D10 left authentication to an ingress. With friends who have no account at the sign-in provider, and saves that should be each player's own, the server now knows its users.

- **Who.** Users sign in with OpenID Connect, or, invited, with an e-mail address and a password. Administrators are OpenID Connect users the configuration names (groups or users); everyone else, invited users included, is a guest. Guests play, keep saves, open and join rooms; administrators also invite, manage users and rescan the library. Only signed-in users see the library; the web app itself (static files) is served to anyone, and shows the sign-in page.
- **OpenID Connect.** The server is the client (backend for frontend): authorization code with PKCE (S256), `state` and `nonce`. The sign-in under way is stored in the database, bound to the browser by a short-lived cookie (login CSRF); the callback uses it once. The ID token is checked with `ring` (the TLS provider already) against the provider's keys (JWKS, refreshed hourly and on an unknown key id): asymmetric algorithms only, issuer, audience and authorized party, expiry, issue time, nonce. Userinfo completes the claims (groups often come only there). The role is recomputed at each sign-in.
- **Pictures.** An OpenID Connect user's `picture` claim is fetched at sign-in, when its URL changed, and kept in the database; the page shows it from Texel's origin (`/api/users/{id}/avatar?v=<hash>`, kept by browsers until it changes), as its Content Security Policy allows no other, and the provider is not asked on every view. Users may be able to set that URL at the provider: the fetch takes https only (redirects too, three at most), 5 seconds and 2 MiB at most, and only a PNG, JPEG, WebP or GIF image (by its signature, so never SVG) is kept, served with its type, `nosniff` and a sandboxing policy of its own. Images are not decoded and re-encoded, sparing an image library; a failure never blocks a sign-in. Without a picture, an avatar shows the initials on a colour of the user's own.
- **Invitations.** An administrator creates a token (20 characters of an unambiguous alphabet, about 99 bits, in groups of five) and hands it over; the registration page takes it, typed or from its link (`/register?token=`). Tokens are used once, expire, and only their SHA-256 is stored. A reset token (for a forgotten password) works the same way. No e-mail is sent. Without OpenID Connect there is no administrator: `--create-invite` prints an invitation.
- **Passwords.** Argon2id (its default parameters), a random salt each, as PHC strings, hashed on a blocking thread. A sign-in for an unknown address checks a dummy hash, taking as long. Ten failures for an address hold it back for a quarter of an hour. At least 10 characters.
- **Sessions.** A random token (32 bytes) in an `HttpOnly`, `SameSite=Lax` cookie, `Secure` with the `__Host-` prefix over https; its SHA-256 in the database, with the ID token (to sign out at the provider too). Lookups are cached a minute (discs are read by hundreds of requests). Changing a password ends the user's other sessions; a reset ends them all.
- **Cross-site requests.** Unsafe requests and WebSocket upgrades must come from Texel's pages: `Sec-Fetch-Site: same-origin`, or an `Origin` equal to the public URL (or the request's host). Clients that are not browsers send neither, and are let through: they hold no victim's cookie.
- **Netplay without an account.** Opening a room takes an account (its members see the host under their account's name); joining does not. Looking a room up sets a cookie with its code, which lets its holder read that room's disc and BIOS, and only while the room is open. Signed-in members join under their account's name.
- **Saves per user.** Memory cards, save states and the BIOS chosen per game belong to a user. The saves made before accounts belonged to nobody and were dropped; as nothing had been deployed, the migrations were then reset to a single initial one, and the import of older versions' files (saves and hashes kept outside the database) removed.
- **Managing saves.** The Memory cards page reads the cards on the server: the PlayStation's directory (titles in Shift-JIS, animated icons through their palettes), and the PS2's file system (FAT, directories; titles from `icon.sys`). Saves are deleted, copied or moved between games' cards; a destination without a card gets a formatted one (the PS2's laid out as the console formats cards, its pages with their ECC: MCMAN reads such a card's root directory). A move writes both cards in one transaction.

## D45. Save states keep the GPU's and SPU's timing

D39 (the GPU drawing while the CPU runs), D42 (the command list waiting for that drawing) and D41 (the SPU's capture position) each left their new state out of save states: a frame's drawing time at most, a capture position, seemed not worth a format change. But a restored machine then runs differently from the one saved, and netplay relies on the opposite: a member whose checksum differs gets the host's state, and must then run in step. With Tekken 3, members fell out of step again within a second of every state, received the host's state again and again (126 MB in under three minutes for one of them), lagged, and dropped out.

Format 4 keeps them, after the rest (`Timing`: the drawing under way, the command list's transfer waiting for it, the capture position); formats 2 and 3 still load, as before (the transfer ends at once). With the headless example's `CHECKSUM` and `STATE` (save then load at the same frame), a reload during Tekken 3's menus or fights now leaves every later checksum as in a run without it; before, they differed within 60 frames. The CPU's pending load is still left out: it is empty between instructions, where states are taken.

## D46. Rooms are sessions, beside the pages: the host's games are the room's

A room followed one game, opened from the player and closed when its host left. It now lasts the session, and is no longer part of the player: its bar sits under the header on every page, and follows its members as they browse.

- **Opening.** Play together, in the header, opens a room without a game. The game the host starts is the room's (opening a room while playing makes that game the room's, as it is): the others are taken to it (`#/room`) and get the host's state, as members joining did. When the host leaves the game (back to the library, say), it stops for everyone, and the room waits for the next: every frame needs the host's input, so the others could not play on. A member who steps out stops holding the game up (`idle`: nobody waits for their input, the host takes back their controller), and steps back in with the host's state. PlayStation games only: PS2 netplay is not done (no checksum, states too large to send).
- **The connection in the page.** It was in the emulator's worker, and went with the player. The page now holds the room (members, chat, game, pings), and relays the game's messages to the worker, which runs the lockstep as before; a worker that is still starting queues them. Each game is a player of its own, as outside rooms.
- **Epochs.** Each change of game or host starts an epoch, counted by the server. Inputs, checksums and states carry its low byte, and requests (`hello`, `resync`, `playing`, `idle`) the epoch: those of an earlier one are about another game, or another host's frames, and are dropped. A guest asks for the host's state again until it gets it (the host may still be starting the game); the host's state lists who plays, whose input to wait for.
- **Handing over.** When the host leaves, the server makes the first other member with an account the host (one without an account was let in by a code only), or closes the room when there is none. In a game, the new host goes on from its frame, keeping the controllers with whoever has them, and the others get its state; out of one, the game stops for everyone.
- **Memory cards.** Only the member whose cards they are saves them: the host who loaded them. A member who took over keeps the session's cards, which came from the previous host, without saving them over their own; the next game they start is played on theirs.

## D47. A GPU command list that loops ends where it loops

Tekken 3 sends, at the end of some fights, a command list (DMA channel 2, linked list) whose nodes loop back: from its 106th node, 64 nodes round and round. The transfer was sent whole, up to a bound of a million nodes: half a second of drawing for the GPU's timing (D39), which the game waited for, its picture held half drawn (the winner's animation missing) and its sound neglected, four times a match.

- A list now ends where it comes back to a node it sent, found with a second walker going at half speed (Floyd's cycle detection: no memory of the nodes, one more read every other node). The loop's nodes are sent two or three times rather than a million; the game draws its winner's animation, and nothing stalls.
- Sending the list at the GPU's pace instead (the CPU running meanwhile, as on the console), in case the game mended it before the transfer got there, was tried: the game does not, it waits for the GPU with its interrupts off, and the transfer never ended. The list must not loop on the console: something earlier differs (not found); other emulators bound or cut such lists too.
- Lists that do not loop are sent as before (D42's timing).

## D48. Memory cards are the user's own, chosen before playing

D9 kept two memory cards per game (by serial), D43 let a console without a disc borrow a game's. A card belonged to a game: a save could not follow its owner to another edition but by copying, and a console's own habit, one card for many games, was out of reach.

- **Cards.** A card is the user's: named, of one console, with its image (`cards`). The Memory cards page lists a console's cards (sub-tabs, as the Games and BIOS pages), creates them (formatted by the server, for both consoles: the PS2's as the console's browser would), renames and deletes them, and works on their saves as before (delete one, copy or move it to another card), all on the server: the PlayStation's directory and the PS2's file system are read and edited there, without starting a console.
- **Before playing.** A game's page asks for its two slots' cards: the game's last ones, else the card used last, else (no card yet) a new one named after the game, made when it starts; or a blank card, not kept. The choice goes in the player's address (`#/play/<game>/<BIOS>/<card 1>-<card 2>`), and is remembered per game (`game_settings`, with its BIOS). A console without a disc chooses the same way; the PS2's game launch too.
- **Migration.** Each game's cards became cards named after its key and slot ("SLES-01893 card 1"), and stay that game's last cards: nothing changes for a game already played. The old table is dropped.
- **Netplay.** Unchanged: the host's cards go with its state, guests save nothing (D46).

## D49. The VUs' microprograms compiled to WebAssembly

Jak and Daxter's jungle needs about 160 million VU1 instructions per second of console time; the interpreter ran 15 million a second in the browser (5 frames a second). Each microprogram is now compiled, as the EE's blocks are (D30): a WebAssembly module per program, in the emulator's function table.

- **A program at a time.** Compiled from its start address when it runs the second time: the blocks reachable from there (to a branch and its delay slot, or to an E bit and the instruction after), a loop dispatching between them on the PC. A jump to a PC no block starts at, and the rare cases (a branch in a delay slot, an E bit on a branch), stop the program for the interpreter, which goes on with the same run.
- **The interpreter's results, exactly.** Each pair does what the interpreter does, in its order: Q's latency, the upper instruction's result (four lanes at once, in WebAssembly's SIMD: operands and results made PS2 floats, the MAC flags from the lanes' bits), the lower instruction, the commit, the flags recorded for the flag instructions four later. The rare lower instructions (the EFU, XGKICK, the random numbers) call the interpreter's. MIN and MAX now give the second operand on ties (+0 and -0), in both. Checked on Jak's jungle: the state, with the VUs' registers and memory, the same every second as with the interpreter (frontend/scripts/check-ps2-vu.mjs).
- **Microcode uploaded again.** Games upload one renderer's microcode after another to the same addresses, every frame (Jak: more than a dozen versions). A start address keeps up to 64 compiled versions, each with the instructions it was compiled from, checked against micro memory only after it was written to.
- **Result.** VU1 runs about 13 times faster (about 190 million instructions a second compiled), VU0 too; the jungle went from 5.4 to 7.9 frames a second (Node, the GS not drawing). The EE is now the limit (its interpreted instructions, COP2 among them), then VIF's unpacking.


## D50. The PS2 runs in slices of 512 EE cycles

The PS2 ran in slices of 128 EE cycles: the EE ran 128 instructions (compiled or interpreted, D30), then the IOP its share, then the timers, DMA, the interrupt lines and the rest caught up. With the EE and VUs compiled (D49), the slices themselves cost about a tenth of the time in Jak and Daxter's jungle.

- **512.** In WebAssembly, the jungle runs at 11.5 frames a second with slices of 512 (10.9 with 256, 10.2 with 128). Interrupts are seen up to 512 EE cycles late (1.7 µs; a scanline is about 18,700), and the IOP runs 64 of its cycles at a time instead of 16.
- **Tested on the library.** Eighteen PS2 games booted for a minute of console time at 128, 256, 512 and 1024 (natively, start and cross pressed in turn, a frame a second compared): the same games get as far, with the same SIF traffic and primitives drawn within a fraction of a percent. Half of them do not get past a black screen in that minute, so for those only their boot is tested.
- **What it found first.** At 512, Jak 3 waited forever for the IOP: its DMA interrupt was raised only when a channel finished with the master enable on, not when the kernel turned the enable back on with a flag up, as DICR bit 31 rising does on the console. Fixed; two games that hung at boot at some slice lengths boot at all of them since. A game hanging with longer slices points to such a race in the emulation rather than to the slice: the console has none.
- **Not longer, for now.** 1024 behaved the same in that test but gains little more; slices that end early at the next event due (timers, VBlank, DMA) would allow longer ones where nothing is due.

## D51. Zipped disc images, read in place through a seek index

Disc images are often kept zipped. The server reads an image inside a zip without unpacking it, for both consoles, like CHD and PBP (a virtual file, decompressed on the fly for the ranges the browser asks for).

- **Stored entries** are read where they lie.
- **Deflated entries** are one stream, decompressed only from its start. It is decompressed once (checking its size and CRC), recording a resume point at the end of a deflate block every 4 MiB of output: where the compressed data is, to the bit, and the 32 KiB of output before it, which later blocks may refer back to (zlib's zran method). A read starts from the closest point before it: a fresh decompressor, primed with the bits left in that byte and given the 32 KiB as dictionary; one that already stands between that point and the read goes on instead, so sequential reads cost nothing more. The index is kept in the state directory (`cache/zip-index`), named after the zip (path, size, modification time) and the entry (index, name, CRC): a changed zip gets a new one.
- **Not at startup.** The scan only names the indexes: building them in it held the server's start (it opened its port after the first scan) for seconds per GB of new zips. They are built after it, one at a time, by a background task; an image played before its turn is indexed then, its first read waiting (before the lock the open images share, so other players' reads go on). The server also serves at once, the first scan in the background (`/readyz` answers 503 until it is done).
- **Measured** (on a desktop): Jak and Daxter (1.4 GB ISO, 938 MB zipped) indexes in 3 s, its index 9 MB; a 4.4 GB DVD (ZIP64) in 10 s, 31 MB. Random 64 KiB reads take about 4 to 5 ms, 10 at most; sequential ones well under a millisecond. The SHA-1 the server computes through the zip is the ISO's.
- **zlib's interface.** Priming and stopping at block boundaries are in zlib's C interface only: `libz-rs-sys`, the same Rust implementation as `flate2`'s (`zlib-rs`). Its calls are unsafe, so `zipimage` allows unsafe code (D30), with its reasons.
- **Not read:** Deflate64 (Windows' compression for very large files), and other methods; the scan says so and skips them. CHD stays the better format for discs: smaller, and random access by design.
- **BIOSes in zips**, for both consoles: a BIOS is small (512 KiB, 4 MiB), so the scan decompresses the zips' entries of a BIOS's size whole and identifies them by content, as files; the server decompresses one again when a player asks for it. A zip may hold any number of them (a whole BIOS directory zipped scans as it does unzipped). The `SOURCE.md` that describes an OpenBIOS or OpenPS2BIOS image applies to the file next to it, not to one in a zip.

## D52. Several library directories, merged

A library may be spread over several directories (another volume, a second collection). Each console's BIOS and game settings (`TEXEL_PS1_BIOS`, `TEXEL_PS1_GAMES`, `TEXEL_PS2_BIOS`, `TEXEL_PS2_GAMES`) take several directories, separated by `:` as in `PATH` (or the option repeated). They are the whole library's configuration: the single library root they used to default under (`TEXEL_LIBRARY`, with its fixed `ps1/bios`… layout) is gone; their defaults are those directories under `/library`, where the container and the Kubernetes manifest mount it.

- **Merged as one.** A game's path is relative to its directory (`games/…`, `ps2/games/…`), whichever directory it is in: its id, and so its saves, settings and verification hash, do not depend on where it lies. Games can move from one directory to another, and a directory can be added, without anything to migrate; with one directory, nothing changes.
- **Duplicates.** A game found at the same relative path in two directories is taken from the first listed, and the scan warns. A BIOS is identified by its content: found twice, it is listed once.

## D53. Redump's SBI files, by Redump name

Redump publishes the SBI files of every LibCrypt-protected disc (D19) as one set. A copy next to each protected image works, but the set covers every protected disc at once. It was first kept in a directory of the library (`TEXEL_PS1_SBI`); the server now downloads it with Redump's DATs (D54) and keeps it in memory (235 files, about 100 KiB). The files keep Redump's names (`Disney's Tarzan (Europe).sbi`), so a disc finds its file by its Redump name.

- **Not by serial.** A serial is shared by a disc's revisions and variants, and their SBI files may differ: Spyro - Year of the Dragon (Europe) Rev 0 and Rev 1 are both SCES-02835, with different files, so one of them would get the other's and stop at its protection check. Some protected discs have no serial in libretro's table (betas, a demo, some French, Spanish and Scandinavian releases), and others several, or odd ones (`SCES-02105/P`).
- **Lookup.** A disc's own file, next to its image, comes first (it may be the only copy, or one the set lacks). Otherwise, a disc whose hashes identify it gets the file of its Redump name, if any. A disc not identified yet (not hashed, or a dump Redump does not list) has only its serial: the file of that serial's discs is used only when they all have the same one (the EDC and non-EDC variants of CTR do), and none otherwise; once hashed, it gets its own.
- **Zipped discs.** A zipped disc may carry its own, next to its CUE sheet and BINs: the one named like its CUE sheet or image, or the zip's only one when it holds one disc (decompressed when asked for); a file next to the zip still comes first.
- **When.** The lookup is done when the game's page or the player asks, since a disc's identity changes as it gets hashed.

## D54. Redump's files from redump.info, downloaded when they change

The server keeps Redump's files for each console in its state directory (`cache/dats`): the game DAT, the BIOS DAT, the CUE sheets, and for the PS1 the SBI files (D53), from redump.info's downloads, with libretro's DAT for serial numbers. They were downloaded from redump.org when older than a week (`TEXEL_DAT_MAX_AGE_DAYS`), the BIOS DATs once, pinned by their SHA-256 as they had not changed since 2016; redump.info has new ones (2026), in XML.

- **When they change.** At each start (in the background) and with the periodic rescans, a HEAD request per file asks for its version: Redump serves each download under a name carrying its version (`Sony - PlayStation - Datfile (10986) (2026-11-24 13-19-14).zip`), libretro's files have an ETag. A file is downloaded only when it is missing or its version is not the one in `versions.json`, next to them; unreachable, the copy at hand is kept. `TEXEL_DAT_MAX_AGE_DAYS` is gone.
- **Formats.** The game DAT is unzipped (as before); the BIOS DAT is XML now, `machine` entries read as `game` ones (a cached clrmamepro one still reads until replaced); the CUE sheets and SBI files are kept as Redump zips them. The SBI files are read into memory with the DATs.
- **CUE sheets** go to BIN files found without theirs (in a directory, or a zip): a multi-track game's tracks were each taken for a disc of its own, its music lost. The scan matches them to a Redump game by their names, when they are Redump's (every track of a game there), else by their sizes, when they are every track of one game only (two tracks or more; equal sizes in the files' order), and uses that game's sheet with the files' own names; the background hashing then verifies the match as for any game. The game is keyed by its first track's path, as that file alone was before. The scan gets the DATs, and runs again when a refresh brings new ones.

## D55. The open-source BIOSes, downloaded by the server

OpenBIOS (PCSX-Redux's, MIT) lets PS1 games start without a BIOS dumped from a console, and OpenPS2BIOS (MIT, experimental) may do the same for the PS2. They were fetched by scripts kept with the library, which also wrote the `SOURCE.md` the scan reads for their versions. The server now fetches them itself, at start (in the background, after the first scan), into its state directory (`cache/openbios/ps1`, `ps2`), which the scan takes as BIOS directories besides the library's (a copy in the library is listed once: a BIOS is identified by its content).

- **Newest, once.** OpenBIOS has no release of its own: every PCSX-Redux build ships it, and builds are published on distrib.app (nightly). The server takes the newest Windows build (a plain zip, where the Linux one is an AppImage, its files in a squashfs), checks it against its manifest's SHA-1, and reads `openbios.bin` out of it (512 KiB, OpenBIOS's mark at 0x78). OpenPS2BIOS comes from its newest GitHub release, checked against GitHub's SHA-256 of the asset and the release's `.sha256` file, its size, and identified as a PS2 BIOS. A `VERSION` file keeps the build or release: one already there is not downloaded again (OpenBIOS itself changes far less often than the builds: build 938's is byte for byte build 585's).
- **Where it comes from.** Each gets a `SOURCE.md` as the scripts wrote it (build or release, date, commit, SHA-256), so the BIOS's page says so.
- **Optional.** `TEXEL_FETCH_OPEN_BIOS=false` turns it off; the scripts still work for a library kept by hand.
