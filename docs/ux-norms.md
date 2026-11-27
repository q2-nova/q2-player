# UX norms

These norms guide every change to the player's screens and controls. The
requirements they produce are in the
[specification](specification.md#home-and-navigation) (UX-001 to UX-012).
Core listening journeys stay usable at all times: starting music, a book or
a podcast, pausing, resuming, skipping and returning to the list (UX-003). A
passing test does not replace a usable journey.

## Errors and recovery

The rules below are UX-006.

- **No error codes on screen.** Numeric codes, internal identifiers, raw OS,
  HTTP or library errors (such as "Bad file descriptor") and stack details
  never appear.
- **Recover first.** Retry, reconnect, reopen or fall back to content on the
  card without asking, when that can succeed.
- **Otherwise say what happened in a few plain words**, and what the
  listener can do: "Can't reach Audiobookshelf", "Not downloaded". One short
  line, no jargon.
- **The code goes to the report.** Every visible failure and every automatic
  recovery records its fixed code (`ux::Code`) in the log and, with
  telemetry on, in the outbox.

A page with nothing to show uses the shared states in
[`ui/state.rs`](../crates/q2nd/src/ui/state.rs):

- **Loading** shows "Loading…". After 15 s the page shows the stuck message
  with Retry, reports `Code::Stuck`, and a late answer still replaces it.
- **Empty** says what is missing ("No albums"), with an optional hint on how
  to fill it.
- **Failed** shows the fixed plain message, an optional hint (offline and
  unreachable failures add "Downloads still play"), and a selected Retry row
  that Center activates.

Back always leaves the page. Hints name the button "Center".

## Hierarchy and interaction

- **iPod as the reference.** Wheel-first navigation, consistent button
  meanings and predictable Back. Check specific iPod behavior before citing
  it, and do not copy a convention that makes a journey worse.
- **Simple but complete.** Common actions are easy to find; actions on the
  selected item live in its long-Center menu.
- **Few steps on frequent journeys.** Count the steps to resume listening,
  pick content, change playback behavior and return to the previous list.
- **Group like with like.** Related services share status presentation and
  grouping; options follow the listener's task, not the internal component.
- **One home per setting.** A shortcut needs a purpose, the same state and
  the same label as its home. Brightness and Equalizer in the Now Playing
  menu open their Settings pages.
- **Dependent settings appear with their switch.** When an option has no use
  while its parent switch is off, it appears below the parent only while the
  parent is on, and keeps its value while hidden. Mix Size under Offline Mix
  is the example (UX-010).
- **Truthful states.** Distinguish connected, unavailable, loading and
  offline. Keep context and selection after a failure or Back.
- **Continuous lists.** Content lists show no backend page numbers, item
  counts as rows, or "Next page" rows. The next or previous page loads when
  the selection comes within four rows of either end of the loaded rows, and
  five pages stay loaded (BROWSE-001, [`ui/paging.rs`](../crates/q2nd/src/ui/paging.rs)).
  An empty group stays empty (BOOK-008).

Wheel and button behavior is in [Wheel and jogging](interaction.md); the
layout and behavior of Settings is in [Settings](settings.md).

## Reviewing a hierarchy change

The UI map [`tests/ui/ui-hierarchy.md`](../tests/ui/ui-hierarchy.md) is
generated from the app's own screens and events over generated fixtures
(UX-001). `python3 tests/ui/map.py` checks it and
`python3 tests/ui/map.py --write` regenerates it. A change that alters the
map carries the map diff and a short UX rationale (UX-002) covering:

- the journey affected and why the placement and grouping fit it;
- steps added or removed;
- duplicate destinations;
- how a hidden action (long-Center, a Center cycle) is discovered;
- Back and cancel behavior.

Check music, books and podcasts separately where their behavior differs, and
add fixture-driven transition tests for the new behavior. Investigate
unexplained map drift instead of accepting a new snapshot. The map lists its
coverage limits: a remote menu without fixture rows is not coverage of its
populated state. The map and tests support UX judgment; they do not show
that a hierarchy is good, and physical wheel feel needs a check on a Q2.
