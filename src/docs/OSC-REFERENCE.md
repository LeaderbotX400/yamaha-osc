# OSC address reference — `tf-bridge.py`

Send OSC over **UDP to port 8000** (the bridge's listen port). Two grammars coexist:

- **Legacy `/tf/*`** — unchanged from tf3.py; existing QLab cues keep working. Stateful.
- **New `/<strip>/<selector>/<param>`** — richer control. Direct (fire-and-forget) except scene.

Channel/selector numbers are **1-based**, matching the console surface (channel 1 = `InCh 0`).

## Strips
`ch`/`input` → InCh · `stin` → StInCh · `fxrtn` → FxRtnCh · `dca` → DCA · `mix` → Mix ·
`mtrx`/`matrix` → Mtrx · `st`/`stereo` → St · `mono` → Mono

## Selectors
A single number (`5`), a range (`1-8`), or a comma list (`1,3,5`) — e.g. `/ch/1-8/mute 1` mutes
inputs 1–8. Out-of-range numbers are dropped; the console rejects anything it doesn't have.

## New grammar
| Address | Args | Effect |
|---|---|---|
| `/<strip>/<sel>/fader` | `<dB>` or `inf`/`-inf` | Set fader level (dB). `-inf` = −∞. Max +10 dB. |
| `/<strip>/<sel>/fader/raw` | `<raw>` | Set fader in raw units (hundredths of dB; −32768 = −∞). |
| `/<strip>/<sel>/fade` | `<to_dB> <ms> [curve] [from_dB]` | Ramp to `to_dB` over `ms`. curve ∈ `linear`\|`cosine`\|`exp`. A later fade/fader on the same target cancels it. |
| `/<strip>/<sel>/on` | `0`\|`1` | Fader on/off (1 = on). |
| `/<strip>/<sel>/mute` | `0`\|`1` | Mute (1 = muted = fader off). |
| `/<strip>/<sel>/name` | `"text"` | Set the channel name. |
| `/<strip>/<sel>/color` | `Purple\|Pink\|Red\|Orange\|Yellow\|Blue\|SkyBlue\|Green` | Set the label colour. |
| `/<strip>/<sel>/send/mix/<dest>/fader` | `<dB>` | Input→mix send level. (`dest` 1-based.) |
| `/<strip>/<sel>/send/mix/<dest>/on` | `0`\|`1` | Input→mix send on/off. |
| `/<strip>/<sel>/send/mix/<dest>/fade` | `<to_dB> <ms> [curve]` | Ramp a send level. |
| `/<strip>/<sel>/send/mtrx/<dest>/…` | | Output strips (mix/st/mono) send to matrices instead. |
| `/mutegroup/<sel>` | `0`\|`1` | Engage/release mute group(s) (1–6). |
| `/scene/recall` | `<a\|b> <n>` | Recall scene `n` from bank a/b. **Stateful**: mutes are re-asserted on top of the recalled scene. |

Notes:
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
```
