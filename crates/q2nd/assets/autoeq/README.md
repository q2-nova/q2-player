# AutoEQ catalog snapshot

Source: https://github.com/jaakkopasanen/AutoEq
Revision: `7ae0f56d53074872b028649617a22bbb4232feb7`
Retrieved: 2026-10-30
License: [MIT](LICENSE), Jaakko Pasanen.

`index.tsv` contains all 6,033 recommended entries from `results/README.md`,
with one tab between the display name and its percent-encoded relative path.
The recommendation is AutoEQ's choice of measurement, not a Q2 endorsement.
The profile URL appends `/<last path component>%20ParametricEQ.txt` to the
entry's path under `results/` at the pinned revision.

`hd650-fixture.txt` is the unmodified export at
`results/oratory1990/over-ear/Sennheiser%20HD%20650/Sennheiser%20HD%20650%20ParametricEQ.txt`.
It is used for parser, DSP and benchmark verification, not automatically enabled
for listeners. Measurement: oratory1990; correction export: AutoEQ.

To refresh, fetch `results/README.md` and LICENSE at a new explicit commit,
extract the Markdown bullet links to this TSV (preserving encoded paths), and
update `audio/autoeq.rs::REVISION` and the catalog-count regression test together.
Keep the fixture's original revision above if it is not also refreshed.

## Catalog verification and path correction — 2026-10-30

The full upstream index has 8,850 profiles for the same 6,033 unique model names;
all model names are represented here. Git-tree verification confirmed every
recommended ParametricEQ file after one case correction: Samsung Galaxy Buds2
Pro `(passive mode)` is displayed as upstream, but its real directory and filename
use `(Passive mode)`. Preserve that correction when refreshing the catalog.
See the audit summary.

## Recovered historical entries — 2026-11-04

[Four Custom Studio bass-switch profiles](custom-studio/README.md) supplement
this snapshot at their own pinned historical revision. The combined searchable
catalog has 6,037 entries; the 6,033-entry normal snapshot above is unchanged.
The recovered exports are bundled for first-use offline selection, with the
upstream shelf-label correction explicitly documented and tested.
