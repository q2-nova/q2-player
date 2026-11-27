# Time-zone catalog

`zone.tab` is the unmodified public-domain IANA table from
[tzdata 2026e](https://data.iana.org/time-zones/releases/tzdata2026e.tar.gz).
The release tarball SHA-256 is
`b26882805f26aac59d5b222978e6580484b834ccdc98be89df2f05a6dc53a652`.
Buildroot 2026.08.1 pins the same tzdata release. The `default` Buildroot zone
list compiles the geographic sources, `etcetera`, `backward`, and `factory`.
The target packaging check requires valid TZif files for every catalog ID and
`Etc/UTC`. Update the table with the pinned Buildroot data to keep packaging
and the picker in agreement.

The catalog uses `zone.tab` so each country or territory keeps its own listed
city, including compatible names such as `Asia/Kuwait` and `Asia/Muscat`.
The shared picker adds UTC and the original fixed-offset settings. Device
configurations retain their stored identifiers, including identifiers configured
outside the picker. A changed device selection retains the restart confirmation.

The flat picker sorts the standard GMT offset first and then the city. The
tracked `standard-offsets.tsv` uses the pinned TZif rules for 2027, the first
full year after the 2026 transitions in that release. It reads standard-time
TZif intervals (including negative daylight saving) and keeps offsets in
whole minutes. The offsets are labels for sorting and do not replace the rules
used to determine actual local time. Reproduce the table from a target
`/usr/share/zoneinfo` directory with

```sh
python3 crates/shared/generate_offsets.py PATH_TO_ZONEINFO > crates/shared/standard-offsets.tsv
```
