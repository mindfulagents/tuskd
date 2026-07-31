# OpenTusk logo & theme

Open `specimen.html` in a browser for the full brand sheet (concept, palette, rules).

## Concept

**Deep memory.** The woolly mammoth — memory that outlasts the ice age, local and
off-grid, excavated intact later — drawn as an engraved natural-history plate to match
the site's field-journal aesthetic (Charter serif + mono labels). Amber is tusk ivory
*and* fossil resin: the thing that preserves. Palette names follow the dig-site theme:
Tar, Soot, Umber, Ash, Bone, Tusk Amber, Deep Tusk, Lichen — all already shipping on
opentusk.ai (CSS vars in the `mindfulagents/opentusk-ai` repo).

## Files

| File | Use |
|---|---|
| `mark.svg` | Fine-line mammoth, `currentColor`. Display sizes ≥ 96px. |
| `mark-bold.svg` | Same drawing thickened (stroke over fill). 32–96px: UI chrome, README header. |
| `badge.svg` | Circular avatar — bone mammoth on soot, amber ring. GitHub org, social. OK down to ~80px. |
| `favicon.svg` | Glyph tier ≤ 32px: amber tusk on soot disc (refined version of the shipped favicon). |
| `lockup-dark.svg` / `lockup-light.svg` | Mark + "OpenTusk" wordmark for dark / bone grounds. |
| `specimen.html` | Brand sheet: all of the above, palette, usage rules. |
| `preview/` | Rendered PNG checks (`rsvg-convert`). |

## Size tiers (the one rule that matters)

- **≥ 96px** — fine mark (`mark.svg`)
- **32–96px** — bold mark (`mark-bold.svg`)
- **≤ 32px** — tusk glyph (`favicon.svg`); the full drawing turns to mud below 32px

Weight trick: the drawing is a filled outline, so adding `stroke="currentColor"` with a
`stroke-width` inflates the line weight without redrawing. 0.7 (viewBox units) = bold;
don't exceed ~1.0 or interior details (eye, trunk lines) close up.

## Attribution — resolve before public use

The mammoth is adapted from The Noun Project, icon #2202222 ("paleo" collection,
source file `noun-paleo-2202222.svg`). Noun Project's standard (free) license is
CC BY 3.0 and **requires visible attribution**; the royalty-free license (NounPro
subscription or per-icon purchase) does not, but neither grants trademark rights in
the underlying drawing. Before this ships as *the* opentusk.ai logo: either buy the
icon / add attribution to the site footer, or commission an original redraw of the
same concept (the theme, tiers, and palette here all carry over unchanged).
