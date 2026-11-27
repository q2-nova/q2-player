# Recovered Beyerdynamic Custom Studio profiles

These profiles use measurements discussed in [AutoEq issue 167](https://github.com/jaakkopasanen/AutoEq/issues/167).
The four entries are **Custom Studio (step 1–4)**, matching the headphone's
mechanical bass-switch positions. Match both earcups to the selected position.
They are distinct from Custom One Pro. “Custom One Studio” was an older RAA
label discussed in the issue, not an additional headphone/profile variant.

Measurement: Reference Audio Analyzer, HDM-X. Target: Harman over-ear 2018.
Correction: AutoEq. License: [upstream MIT](LICENSE), copyright Jaakko Pasanen.
The four `step-N.txt` files are byte-for-byte upstream exports, pinned to
`6282acb7846808bbe495568a56287e660d877d87` (2022-09-18), the last commit which
changed these exports. Their bytes also match the parent of the removal commit.
[provenance.json](provenance.json) records SHA-256 values for each original
export and its full source response CSV. Sparse `step-N-response.tsv` oracles
contain original CSV `frequency`/`parametric_eq` values, not newly fitted curves.

## History and import correction

- [Restored Custom Studio measurement, May 2020](https://github.com/jaakkopasanen/AutoEq/commit/d791e3f17521f2d447affb5b2fe18222b74c6c5b).
- [Fixed all four Custom Studio steps, July 2020](https://github.com/jaakkopasanen/AutoEq/commit/bce66221d8ae97a66b0e7db1f28fbae26d0f5fa3).
- [Recomputed exports, September 2022](https://github.com/jaakkopasanen/AutoEq/commit/6282acb7846808bbe495568a56287e660d877d87).
- [Removed Reference Audio Analyzer collection, May 2023](https://github.com/jaakkopasanen/AutoEq/commit/9692ff684d5f4511c65830a9757a5a7c13e8fe8c).

These exports predate the shelf-label fix confirmed by the maintainer in
[AutoEq issue 586](https://github.com/jaakkopasanen/AutoEq/issues/586).
At load time Q2 changes only the bundled `ON LS Fc` and `ON HS Fc` tokens to
`ON LSC Fc` and `ON HSC Fc`. AutoEq's [pinned filter implementation](https://github.com/jaakkopasanen/AutoEq/blob/6282acb7846808bbe495568a56287e660d877d87/peq.py)
uses center-frequency RBJ shelves. This correction reproduces those intended
shelves using Q2's existing strict importer; it is not a global LS/LSC alias.
Frequency, gain, Q and preamp values remain exactly as exported. Regression
tests compare every numeric token and the independent upstream CSV response.

Step 1's text preamp is -6.5 dB; its historical README lists -6.6 dB. The TXT
is the machine-readable authority here. Existing automatic combined headroom
can attenuate further; no export preamp is raised or removed.

Issue 167 includes a January 2022 comment questioning step 1's bass measurement.
These files preserve the latest genuine upstream result, including that
uncertainty; they are not independently remeasured or replaced with a different
headphone's curve. No claim of listening preference or measured device/headphone
accuracy follows from the software checks.

## Exact sources

- [Beyerdynamic Custom Studio (step 1)](https://github.com/jaakkopasanen/AutoEq/blob/6282acb7846808bbe495568a56287e660d877d87/results/referenceaudioanalyzer/referenceaudioanalyzer_hdm-x_harman_over-ear_2018/Beyerdynamic%20Custom%20Studio%20(step%201)/Beyerdynamic%20Custom%20Studio%20(step%201)%20ParametricEQ.txt)
- [Beyerdynamic Custom Studio (step 2)](https://github.com/jaakkopasanen/AutoEq/blob/6282acb7846808bbe495568a56287e660d877d87/results/referenceaudioanalyzer/referenceaudioanalyzer_hdm-x_harman_over-ear_2018/Beyerdynamic%20Custom%20Studio%20(step%202)/Beyerdynamic%20Custom%20Studio%20(step%202)%20ParametricEQ.txt)
- [Beyerdynamic Custom Studio (step 3)](https://github.com/jaakkopasanen/AutoEq/blob/6282acb7846808bbe495568a56287e660d877d87/results/referenceaudioanalyzer/referenceaudioanalyzer_hdm-x_harman_over-ear_2018/Beyerdynamic%20Custom%20Studio%20(step%203)/Beyerdynamic%20Custom%20Studio%20(step%203)%20ParametricEQ.txt)
- [Beyerdynamic Custom Studio (step 4)](https://github.com/jaakkopasanen/AutoEq/blob/6282acb7846808bbe495568a56287e660d877d87/results/referenceaudioanalyzer/referenceaudioanalyzer_hdm-x_harman_over-ear_2018/Beyerdynamic%20Custom%20Studio%20(step%204)/Beyerdynamic%20Custom%20Studio%20(step%204)%20ParametricEQ.txt)

## Integration

All four appear in the existing alphabetically sorted Choose Headphones list
and all-word search. They are bundled, so first selection works offline without
writing a cache. Normal catalog downloads/cache behavior remains unchanged.
The active normalized profile uses the original pinned URL as its provenance
and selection identity and persists through the normal playback settings.
No owner settings are changed by adding the library entries. Because the index
and exports are compiled with `include_str!`, shipping requires an app rebuild.
