# OSC address reference — `tf-bridge.py`

Send OSC over **UDP to port 8000** (the bridge's listen port). Two grammars coexist:

- **Legacy `/tf/*`** — unchanged from tf3.py; existing QLab cues keep working. Stateful.
- **New `/<strip>/<selector>/<param>`** — richer control. Direct (fire-and-forget) except scene.

Console writes are **batched** (several commands per TCP write) and all running fades advance
together, so wide moves (`/ch/1-24/...`) cost about the same as narrow ones.

Channel/selector numbers are **1-based**, matching the console surface (channel 1 = `InCh 0`).

## Strips
`ch`/`input` → InCh · `stin` → StInCh · `fxrtn` → FxRtnCh · `dca` → DCA · `mix` → Mix ·
`mtrx`/`matrix` → Mtrx · `st`/`stereo` → St · `mono` → Mono

## Selectors
A single number (`5`), a range (`1-8`), or a comma list (`1,3,5`) — e.g. `/ch/1-8/mute 1` mutes
inputs 1–8. Out-of-range numbers are dropped; the console rejects anything it doesn't have.

**Groups with `|`:** `/ch/1-5|12-24/on off on` — values are paired to groups by position (here
1–5 → `off`, 12–24 → `on`; the extra value is logged and ignored). A single value applies to every
group. Works for `fader`, `fader/raw`, `fader/rel`, `on`, `mute`, `name`, `color`, `send/.../fader|on`
and `/mutegroup`. `fade` takes one shared set of args for all groups; `only` takes a single selector.

## New grammar
| Address | Args | Effect |
|---|---|---|
| `/<strip>/<sel>/fader` | `<dB>` or `inf`/`-inf` | Set fader level (dB). `-inf` = −∞. Max +10 dB. |
| `/<strip>/<sel>/fader/raw` | `<raw>` | Set fader in raw units (hundredths of dB; −32768 = −∞). |
| `/<strip>/<sel>/fade` | `<to_dB> <ms> [curve] [from_dB]` | Ramp to `to_dB` over `ms`. curve ∈ `linear`\|`cosine`\|`exp`. A later fade/fader on the same target cancels it. |
| `/<strip>/<sel>/fader/rel` | `<±dB>` | Nudge from the current level (our last write or the console's real level). −∞ + positive trim → just above the floor; clamps at +10 dB. |
| `/<strip>/<sel>/fade/stop` | — | Cancel fades on those channels (they stay where they are). |
| `/<strip>/<sel>/on` | `0`\|`1`\|`on`\|`off` | Fader on/off (1 = on). **Stateful** for `ch`/`mix`/`mtrx`/`dca` (see below). |
| `/<strip>/<sel>/mute` | `0`\|`1` | Mute (1 = muted = fader off). Stateful, same strips. |
| `/<strip>/<sel>/only` | `<list>` e.g. `3,7,12` or `3 7 12` | Listed channels **on**, every other channel in `<sel>` **off** — one batch. Numbers outside `<sel>` are ignored. No args = all off. Stateful. |
| `/<strip>/<sel>/name` | `"text"` | Set the channel name. |
| `/<strip>/<sel>/color` | `Purple\|Pink\|Red\|Orange\|Yellow\|Blue\|SkyBlue\|Green` | Set the label colour. |
| `/<strip>/<sel>/send/mix/<dest>/fader` | `<dB>` | Input→mix send level. (`dest` 1-based.) |
| `/<strip>/<sel>/send/mix/<dest>/on` | `0`\|`1` | Input→mix send on/off. |
| `/<strip>/<sel>/send/mix/<dest>/fade` | `<to_dB> <ms> [curve]` | Ramp a send level. |
| `/<strip>/<sel>/send/mtrx/<dest>/…` | | Output strips (mix/st/mono) send to matrices instead. |
| `/<strip>/<sel>/send/mix/<dest>/only` | `<list>` | Sends to `<dest>` (single, range or list e.g. `2-3`) are **on** for the listed channels and **off** for every other channel in `<sel>`. No args = all off. Direct (not stateful). |
| `/<strip>/<sel>/send/mix/<dest>/prepost` | `0`\|`1` | Raw send point as the console stores it. Which value is pre-fader is not assumed - confirm on the desk. |
| `/mutegroup/<sel>` | `0`\|`1` | Engage/release mute group(s) (1–6). |
| `/scene/recall` | `<a\|b> <n> [keep]` | Recall scene `n` from bank a/b. **The scene wins**: remembered mic on/off from earlier cues is forgotten first. Add `keep` to re-assert the remembered mutes on top of the scene (legacy `/tf/scene` always does). |
| `/panic` or `/fade/stop` | — | Stop every running fade where it is. |
| `/preset/<name>` | — | Run a named preset from `presets.json` (see below) as one batch. `/preset/reload` re-reads the file. |
| `/snapshot/save` | `<name> <strip/sel>…` | Read level + on/off of e.g. `ch/1-24 dca/1-8` and store as `<name>` in `snapshots.json`. |
| `/snapshot/recall` | `<name> [ms] [curve]` | Restore a snapshot: crossfade over `ms` if given, else instant. On/off is applied statefully. |

Notes:
- **Stateful on/off.** `on`/`mute`/`only` on `ch`, `mix`, `mtrx`, `dca` share the legacy `/tf/mute`
  desired-state map: applied immediately (as one burst), re-applied after a console reconnect, and re-asserted after a
  `/tf/scene` recall or `/scene/recall ... keep`. A plain `/scene/recall` forgets them first so the scene wins. `stin`, `fxrtn`, `st`, `mono` on/off stay direct.
- `dB` accepts an int or float; `-inf` (string) or a value ≤ −327 dB → the −∞ sentinel.
- `/dca` on/mute writes both `DCA/` and `DcaCh/` to match the console surface.
- Input/stin/fxrtn strips send to **mix**; mix/st/mono send to **mtrx**. Using the wrong bus word
  is rejected (logged).

## Legacy grammar (still supported)
| Address | Args | Effect |
|---|---|---|
| `/tf/mute` | `<tokens…>` | Mute. Tokens: bare numbers = inputs; `aux 9/10`, `matrix 1-4`, `dca 1`, `fx1`, `st1`, `master`, `sub` (stereo-pair & DCA-dual-write aware). Stateful. |
| `/tf/unmute` | `<tokens…>` | Unmute the same token grammar. |
| `/tf/scene` | `<A\|B><n>` (e.g. `A12`) | Recall a scene; re-asserts mutes afterward. |

## Virtual DCAs (`/vdca/<n>/...`)
The TF's remote protocol cannot assign channels to its real DCAs, so the bridge keeps its own groups, **virtual DCAs 1-8**
of input channels (1-40). They are separate from the real `/dca/<n>/...` strips, which still drive the desk's DCA faders.
| Address | Args | Effect |
|---|---|---|
| `/vdca/<n>/only` | `<channels>` e.g. `3,7,12` | The group contains exactly these channels. Channels that leave go back to their own level. |
| `/vdca/<n>/add` / `/remove` | `<channels>` | Add / remove channels. |
| `/vdca/<n>/clear` | - | Empty the group (members restored). |
| `/vdca/<n>/fader` | `<dB>` or `-inf` | Group offset: every member sits `dB` away from its own base level. |
| `/vdca/<n>/fade` | `<dB> <ms> [curve]` | Fade the group offset. |
Rules: a member's level = its base level + the offsets of every virtual DCA it is in; leaving the last group restores the
base. A **direct** write to a member's fader (`/ch/5/fader -10`) becomes its new base. State is in memory only (a bridge restart
forgets groups, channels stay where they were left) and a **scene recall overwrites levels** without re-applying offsets.
Channels at -inf stay at -inf. Example: `/vdca/2/only 6-13,15-24` then `/vdca/2/fader -5` = backing 5 dB down.

## Presets (`presets.json`)
Named lists of OSC lines, in a file beside the script (`TF_PRESETS` overrides). Fire with
`/preset/<name>`; the file is reloaded when it changes. A preset's console writes go out as one batch.
Bad file → logged, previous good copy kept. Presets can't call presets.
```json
{ "act1-sc2": ["/ch/1-24/only 3,7,12", "/dca/1/fade 0 1500", "/ch/5/name \"Lead Vox\""] }
```
See `src/presets.example.json`.

## Examples
```
/ch/5/fader -6            # input 5 to -6 dB
/ch/5/fader -inf          # input 5 to -inf
/ch/1-8/mute 1            # mute inputs 1..8
/ch/5/fade -30 2000 cosine
/ch/5/send/mix/3/fader -12
/mix/3/fader 0            # mix bus 3 to 0 dB
/st/1/fader -3           # stereo master
/scene/recall a 12
/ch/1-24/only 3,7,12      # mic scene: 3, 7, 12 live; the rest of 1-24 off
/ch/1-5|12-24/on off on   # 1-5 off, 12-24 on
/ch/5/fader/rel -3        # nudge down 3 dB
/panic                    # stop all fades
/snapshot/save pit ch/1-4 dca/1
/snapshot/recall pit 2000 cosine
```
