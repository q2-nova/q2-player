# Saluki DDSketch mapping attribution

The positive logarithmic index/value mapping in `../../src/diagnostics/ddsketch.rs`
is adapted from DataDog/saluki, commit `f7d29428d8681f771c549c4a80c0e0f9a531fbe2`,
`lib/ddsketch/src/canonical/mapping/logarithmic.rs`.
Source SHA256: `7be0a7194bde8dc00c0f506a27b73f75d7858bdd0bda0f61154a0c659d8c4d15`.
Upstream: https://github.com/DataDog/saluki/tree/f7d29428d8681f771c549c4a80c0e0f9a531fbe2/lib/ddsketch

Apache-2.0 LICENSE and upstream NOTICE are retained here. Local adaptation:
fixed1% accuracy, integer nonnegative microseconds through24h, sparse bounded
bins and an explicit JSON wire mapping; no protobuf/Datadog agent dependency.
Aggregation/count/validation/tests are local. This is not Saluki's full crate or
Datadog protobuf wire protocol. It preserves mergeable DDSketch bin semantics.
