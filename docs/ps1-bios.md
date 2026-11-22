# PlayStation BIOS versions

What the PlayStation BIOS versions differ in, which ones to use, and how to tell them apart. Texel identifies a BIOS by its content wherever it is under `bios/` (see the README), so file names only matter to you and to other emulators; the names below are the ones most emulators expect.

All Sony BIOSes are 512 KiB (524,288 bytes). Version, date and region come from the `System ROM Version` string at offset `0x7FF32` of each ROM, except for the SCPH-1000's, which has none and is identified by its checksum. Region codes: **J** = NTSC-J (Japan), **A** = NTSC-U/C (North America), **E** = PAL (Europe).

## Which BIOS to use

- **For playing**: a BIOS of the game's region, from the 1995 kernel onwards: `scph5500.bin` (J), `scph5501.bin` (A), `scph5502.bin` (E), or any later one of the same region. Japanese BIOSes refuse European discs (see DECISIONS.md, D13).
- **Region-free**: `psxonpsp660.bin` or OpenBIOS, for games of a region you have no BIOS for.
- **For testing the emulator**: one BIOS of each kernel (below), since they differ in code that games call.

## Kernels

The kernel is the part of the BIOS that games call into; its version string is separate from the `System ROM Version`. Three kernels shipped:

| Kernel string | BIOS versions | Notes |
|---|---|---|
| `CEX-1000 KT-3  by S.O.` | 1.0 (SCPH-1000), 2.0 (SCPH-1002) | The 1994 kernel. Its exception handler has a bug (it reads a garbage register value instead of the cause register), and it lacks some functions later games may call (`GetSystemInfo`, `card_write_test`, `_ioabort_raw`). |
| `CEX-3000/1001/1002 by K.S.` | 2.2, 3.0, 4.1, 4.5 | The 1995 kernel, which every later console kept except the SCPH-7000. The most compatible. |
| `CEX-7000/-7001 by K.S.` | 4.0 (SCPH-7000) | Identical to the 1995 kernel, except that it configures the ROM area as 1 MB instead of 512 KB at boot. |

## Known BIOSes

| Usual file name | Models | Version | Date | Region | Kernel | Description |
|---|---|---|---|---|---|---|
| `scph1000.bin` | SCPH-1000, DTL-H1000 | 1.0 | 1994-09-22 | J | 1994 | Launch BIOS. Different menu cursor, and the CD player only opens with an audio CD in the drive. Least compatible: use it only to test launch-era behaviour. |
| `scph1002.bin` | SCPH-1002, DTL-H1002 | 2.0 | 1995-05-10 | E | 1994 | Earliest European BIOS in Redump's list, still with the 1994 kernel and its bugs. |
| `scph1001.bin` | SCPH-1001, SCPH-5003, DTL-H1201, DTL-H3001 | 2.2 | 1995-12-04 | A | 1995 | Very compatible. The US BIOS long recommended by older emulators such as ePSXe. Menu unchanged from v2.1. |
| `scph5000.bin` | SCPH-5000, DTL-H1200, DTL-H3000 | 2.2 | 1995-12-04 | J | 1995 | Japanese counterpart of `scph1001.bin`: same kernel and v2.1-era menu. libretro lists another image under this name (see below). |
| `scph5500.bin` | SCPH-5500 | 3.0 | 1996-09-09 | J | 1995 | Japanese BIOS expected by Beetle PSX and SwanStation (libretro). |
| `scph5501.bin` | SCPH-5501, SCPH-5503, SCPH-7003 | 3.0 | 1996-11-18 | A | 1995 | US BIOS expected by Beetle PSX and SwanStation. Recommended for North American games. The SCPH-7003, despite its 7000-series model number, shipped with this BIOS, so it has no Sound Scope. |
| `scph5502.bin` | SCPH-5502, SCPH-5552 | 3.0 | 1997-01-06 | E | 1995 | European BIOS expected by Beetle PSX and SwanStation. Recommended for PAL games. Has the older European menu, redesigned with the SCPH-7002. |
| `scph7000.bin` | SCPH-7000, SCPH-7500, SCPH-9000 | 4.0 | 1997-08-18 | J | 7000 | The only BIOS with the CEX-7000 kernel. Adds Sound Scope, the CD player's music visualiser (SELECT during audio playback). Lacks the accented-character font of non-Japanese BIOSes. |
| `scph7001.bin` | SCPH-7001, SCPH-7501, SCPH-7503, SCPH-9001, SCPH-9003, SCPH-9903 | 4.1 | 1997-12-16 | A | 1995 | Back on the 1995 kernel, with Sound Scope. |
| `scph7502.bin` | SCPH-7002, SCPH-7502, SCPH-9002 | 4.1 | 1997-12-16 | E | 1995 | The same code as `scph7001.bin`: only the region letter of the version string differs (offset `0x7FF52`, `A` → `E`). Sound Scope, and the redesigned menu shared with the US and Japanese BIOSes. |
| `scph101.bin` | SCPH-101 (PS one) | 4.5 | 2000-05-25 | A | 1995 | The latest North American BIOS in Redump's list. |
| `psxonpsp660.bin` | PSP firmware 6.60 | 4.5 | 2000-05-25 | J (region-free) | 1995 | Not from a console: the BIOS of the PSP's PS1 emulator, based on the Japanese v4.5 and modified by Sony for it. Redump does not list it. Region-free in Texel (DECISIONS.md, D13). |
| `openbios.bin` | | | | Region-free | | [OpenBIOS](https://github.com/pcsx-redux/nugget/tree/main/openbios), PCSX-Redux's open-source replacement (MIT): needs no console dump. No version string; built from source, so no DAT lists it. |

## Checksums

| File | Redump name | MD5 | SHA-1 |
|---|---|---|---|
| `scph1000.bin` | `ps-10j` | `239665b1a3dade1b5a52c06338011044` | `343883a7b555646da8cee54aadd2795b6e7dd070` |
| `scph1002.bin` | `ps-20e` | `54847e693405ffeb0359c6287434cbef` | `20b98f3d80f11cbf5a7bfd0779b0e63760ecc62c` |
| `scph1001.bin` | `ps-22a` | `924e392ed05558ffdb115408c263dccf` | `10155d8d6e6e832d6ea66db9bc098321fb5e8ebf` |
| `scph5000.bin` | `ps-22j` | `57a06303dfa9cf9351222dfcbb4a29d9` | `ffa7f9a7fb19d773a0c3985a541c8e5623d2c30d` |
| `scph5500.bin` | `ps-30j` | `8dd7d5296a650fac7319bce665a6a53c` | `b05def971d8ec59f346f2d9ac21fb742e3eb6917` |
| `scph5501.bin` | `ps-30a` | `490f666e1afb15b7362b406ed1cea246` | `0555c6fae8906f3f09baf5988f00e55f88e9f30b` |
| `scph5502.bin` | `ps-30e` | `32736f17079d0b2b7024407c39bd3050` | `f6bc2d1f5eb6593de7d089c425ac681d6fffd3f0` |
| `scph7000.bin` | `ps-40j` | `8e4c14f567745eff2f0408c8129f72a6` | `77b10118d21ac7ffa9b35f9c4fd814da240eb3e9` |
| `scph7001.bin` | `ps-41a` | `1e68c231d0896b7eadcad1d7d8e76129` | `14df4f6c1e367ce097c11deae21566b4fe5647a9` |
| `scph7502.bin` | `ps-41e` | `b9d9a0286c33dc6b7237bb13cd46fdee` | `8d5de56a79954f29e9006929ba3fed9b6a418c1d` |
| `scph101.bin` | `psone-45a` | `6e3735ff4c7dc899ee98981385f6f3d0` | `dcffe16bd90a723499ad46c641424981338d8378` |
| `psxonpsp660.bin` | | `c53ca5908936d412331790f4426c6c33` | `96880d1ca92a016ff054be5159bb06fe03cb4e14` |

## Names in other emulators

- Several models share one BIOS, so libretro lists the same image under several names: `scph5501.bin` and `scph7003.bin`; `scph5502.bin` and `scph5552.bin`; `scph7502.bin`, `scph7002.bin` and `scph9002(7502).bin`.
- libretro's `scph5000.bin` (MD5 `eb201d2d98251a598af467d4347bb62f`) is another image than Redump's SCPH-5000 BIOS above, and Redump does not list it. A RetroArch core checking `scph5000.bin` by hash rejects Redump's; the common cores (Beetle PSX, SwanStation) use `scph5500.bin`, `scph5501.bin` and `scph5502.bin` instead.
- libretro has no `scph7000.bin`. DuckStation knows it, as "SCPH-7000, 7500, 9000 (v4.0 08-18-97 J)".

## Sources

- Checksums and models: Redump's [PlayStation BIOS DAT](http://redump.org/datfile/psx-bios/); file names: the `Sony - PlayStation` section of libretro's [`System.dat`](https://github.com/libretro/libretro-database/blob/master/dat/System.dat), and DuckStation's known-BIOS table ([`src/core/bios.cpp`](https://github.com/stenzek/duckstation/blob/master/src/core/bios.cpp)).
- [psx-spx, Kernel (BIOS)](https://psx-spx.consoledev.net/kernelbios/): kernel versions, the v1.0 exception-handler bug and missing functions, the v4.0 kernel change, character sets.
- [Wikipedia, PlayStation models](https://en.wikipedia.org/wiki/PlayStation_models): Sound Scope arriving with the SCPH-7000 series.
- [Looking at the PlayStation BIOS interface](https://drew1440.com/2021/09/23/looking-at-the-playstation-bios-interface/): menu differences between models and regions.
- The ROMs themselves: version and kernel strings, and byte comparisons (`scph7001.bin` and `scph7502.bin` differ in one byte; `scph5501.bin` is the SCPH-7003's BIOS).
