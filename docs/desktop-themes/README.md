# Sable Desktop — 10 Theme ("Rice") Design Candidates

This directory contains **10 complete theme ("rice") design candidates**, KDL v2 configurations,
**Sable Shading Language (SSL)** shaders, and interactive HTML mockups for the AbyssBSD desktop
described in [../DESKTOP.md](../DESKTOP.md).

## Quick Start

- **Interactive Showcase (All 10 Candidates + Live KDL/SSL Inspector):**
  Open [index.html](index.html) in any browser (use keys `1`–`0` or the top bar to switch themes,
  and toggle the Launcher, Notification Toast, and KDL/SSL inspector).
- **Standalone HTML Mockups:**
  Each candidate also has a standalone mockup under [mockups/](mockups/).
- **Rice Directories (`rice.kdl` + `shaders/*.ssl`):**
  Under [rices/](rices/), structured exactly as specified in [DESKTOP.md §8.7](../DESKTOP.md#87-distribution-and-ricing)
  and restricted to the Rice cascade layer keys in [DESKTOP.md §7.3](../DESKTOP.md#73-the-cascade)
  (`theme`, `panel`, `dock`, `layout`, `animation`, `shader` only — never `grants`, `bind`, `app`,
  `security`, `input`, or `output`).

---

## Candidate Matrix

| # | Rice ID | Name | Mode | Layout | SSL Shaders (`lib/shade`) | Files |
|---|---|---|---|---|---|---|
| 01 | `hadal` | **Hadal Bioluminescence** *(Flagship)* | Dark Glass | `tile` | [caustic_glass.ssl](rices/hadal/shaders/caustic_glass.ssl) (`backdrop`), [pressure_wave.ssl](rices/hadal/shaders/pressure_wave.ssl) (`transition`) | [rice.kdl](rices/hadal/rice.kdl) · [Mockup](mockups/01-hadal.html) |
| 02 | `pelagic-day` | **Pelagic Daylight** | High-Contrast Light | `columns` | [paper_grain.ssl](rices/pelagic-day/shaders/paper_grain.ssl) (`fill`) | [rice.kdl](rices/pelagic-day/rice.kdl) · [Mockup](mockups/02-pelagic-day.html) |
| 03 | `daemon-bsd` | **Beastie Carbon** | Dark Carbon | `stack` | [ember_pulse.ssl](rices/daemon-bsd/shaders/ember_pulse.ssl) (`fill`) | [rice.kdl](rices/daemon-bsd/rice.kdl) · [Mockup](mockups/03-daemon-bsd.html) |
| 04 | `esports-zero` | **0ms Tournament** | OLED Black | `monocle` | *None* (zero GPU composition overhead, 0 ms animations) | [rice.kdl](rices/esports-zero/rice.kdl) · [Mockup](mockups/04-esports-zero.html) |
| 05 | `broadcast-deck` | **Studio Control Room** | Dark Studio | `tile` | [tally_glow.ssl](rices/broadcast-deck/shaders/tally_glow.ssl) (`fill`), [studio_wipe.ssl](rices/broadcast-deck/shaders/studio_wipe.ssl) (`transition`) | [rice.kdl](rices/broadcast-deck/rice.kdl) · [Mockup](mockups/05-broadcast-deck.html) |
| 06 | `phosphor-amber` | **P3 Vector Terminal** | Retro CRT | `tile` | [crt_aperture.ssl](rices/phosphor-amber/shaders/crt_aperture.ssl) (`post`), [phosphor_decay.ssl](rices/phosphor-amber/shaders/phosphor_decay.ssl) (`transition`) | [rice.kdl](rices/phosphor-amber/rice.kdl) · [Mockup](mockups/06-phosphor-amber.html) |
| 07 | `fluorite` | **Iridescent Frosted Glass** | Translucent | `tile` / `float` | [chromatic_frost.ssl](rices/fluorite/shaders/chromatic_frost.ssl) (`backdrop`), [aurora_mesh.ssl](rices/fluorite/shaders/aurora_mesh.ssl) (`fill`) | [rice.kdl](rices/fluorite/rice.kdl) · [Mockup](mockups/07-fluorite.html) |
| 08 | `swiss-grid` | **International Typographic Grid** | Light Stone | `columns` | [grid_shutter.ssl](rices/swiss-grid/shaders/grid_shutter.ssl) (`transition`) | [rice.kdl](rices/swiss-grid/rice.kdl) · [Mockup](mockups/08-swiss-grid.html) |
| 09 | `solaris-cde` | **Neo-Workstation Bevel** | Mid-tone Bevel | `float` | [sdf_chisel.ssl](rices/solaris-cde/shaders/sdf_chisel.ssl) (`fill`) | [rice.kdl](rices/solaris-cde/rice.kdl) · [Mockup](mockups/09-solaris-cde.html) |
| 10 | `benthic-eink` | **Zero-Strain Monochrome** | Warm Monochrome | `monocle` | [bayer_dither.ssl](rices/benthic-eink/shaders/bayer_dither.ssl) (`post`) | [rice.kdl](rices/benthic-eink/rice.kdl) · [Mockup](mockups/10-benthic-eink.html) |

---

## Directory Layout

```
docs/desktop-themes/
  README.md
  index.html                                  # Interactive 10-theme gallery + KDL/SSL inspector
  mockups/
    01-hadal.html
    02-pelagic-day.html
    03-daemon-bsd.html
    04-esports-zero.html
    05-broadcast-deck.html
    06-phosphor-amber.html
    07-fluorite.html
    08-swiss-grid.html
    09-solaris-cde.html
    10-benthic-eink.html
  rices/
    hadal/          rice.kdl, shaders/{caustic_glass,pressure_wave}.ssl
    pelagic-day/    rice.kdl, shaders/paper_grain.ssl
    daemon-bsd/     rice.kdl, shaders/ember_pulse.ssl
    esports-zero/   rice.kdl
    broadcast-deck/ rice.kdl, shaders/{tally_glow,studio_wipe}.ssl
    phosphor-amber/ rice.kdl, shaders/{crt_aperture,phosphor_decay}.ssl
    fluorite/       rice.kdl, shaders/{chromatic_frost,aurora_mesh}.ssl
    swiss-grid/     rice.kdl, shaders/grid_shutter.ssl
    solaris-cde/    rice.kdl, shaders/sdf_chisel.ssl
    benthic-eink/   rice.kdl, shaders/bayer_dither.ssl
```
