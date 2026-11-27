# UX norms and hierarchy review

These norms guide player interaction and review.

The standing requirement (UX-003): **UX must be maintained at all
times; bad UX breaks the product and invalidates other work.** Core playback,
local content, recovery and responsive navigation are product requirements.
An isolated test pass is not a substitute for a usable listening journey.
The Shuffle-start target is 50 ms.

## Errors and recovery — Product feedback, 2026-11-16 (UX-006)

- **Never display error codes.** No numeric codes, internal identifiers, raw
  OS/HTTP/library errors (for example “Bad file descriptor”) or stack details
  appear on screen.
- **Recover automatically first.** Retry, reconnect, reopen or fall back to
  local content without asking the user when that can succeed.
- **If recovery is not possible, say what happened in a few plain words**, and
  what the user can do if anything: “Can't reach Audiobookshelf”, “Not
  downloaded”. One short line, no jargon.
- **The error code goes in the error report.** Every user-visible failure and
  every automatic recovery records its fixed code in telemetry, with the state
  needed to diagnose it.
- **One page state (2026-11-18).** A page with nothing to show uses the shared
  loading/empty/failed presentation (`ui/state.rs`): the plain message, an
  optional hint, and for a failure a selected Retry row that Center activates.
  Every load has a deadline (15 s for pages), after which the page says so and
  offers Retry. Empty pages say what is missing ("No albums"), never a blank
  page or "Nothing here". Hints spell the button "Center".

## Settings that depend on a switch

Product feedback, 2026-11-23. Show Mix Size only when Offline Mix is on. Preserve the saved percentage when the row is hidden. In future Settings work, review whether a subordinate option has any use while its parent is off. Where it has none, reveal it below the parent when enabled.

## Hierarchy and interaction

- **iPod-inspired:** wheel-first navigation, consistent button semantics and
  predictable Back behavior. Research specific historical behavior rather than
  claiming exact fidelity or copying a convention that conflicts with feedback.
- **Simple but powerful:** keep common actions easy to find; contextual actions
  can live in consistent long-press menus without scattering duplicate settings.
- **Minimize friction for frequent critical journeys:** evaluate steps to resume
  listening, select content, change playback behavior and return to the prior
  list. Deliberate requested steps, such as book info before playback, remain.
- **Group like with like:** related services share status presentation and visual
  grouping; organize options by the user's task rather than internal components.
- **One coherent home for a setting:** additional shortcuts need a clear purpose,
  shared state and consistent labels. Avoid unexplained duplicate destinations.
- **Truthful states and recoverable actions:** distinguish connected, unavailable,
  loading and offline; preserve context and selection after failure or Back.

the original product 2026-11-04 book-browsing correction adds: content lists should not expose
backend page numbers, item-count footer rows, “Home preview” or “Next page”. Empty
book groups remain empty; further pages load automatically during browsing until
exhaustion. BOOK-008 is delivered in V0.33, with the updated generated hierarchy,
bounded paging/fault tests and installed native-DRM replay.

## Required review whenever hierarchy or interactions change

Generate the implementation-derived UI map and review the change before merging.
Cover visible menus, contextual/long-press actions, conditional states, input
semantics, Now Playing tap cycles and return paths. State unsupported coverage;
a manually maintained outline is not proof of the actual implementation.

For each change, record a short UX rationale: the affected user journey, why the
placement/grouping makes sense, any added/removed steps, duplicate destinations,
discoverability of hidden actions, and Back/cancel behavior. Check music, books
and podcasts independently where their semantics differ. Add fixture-driven
transition/property coverage for the behavior; investigate unexplained map drift
rather than simply accepting a new snapshot. Run `python3 tests/ui/map.py` to
check the implementation-derived [map](../tests/ui/ui-hierarchy.md), or
`python3 tests/ui/map.py --write` to regenerate it for deliberate review.
The map records its generator coverage and limits.

The generator and tests support human UX judgment; passing them alone cannot
establish that a hierarchy is good. Final hardware checks remain required for
completed product changes.

## Historical reference: iPod classic

Apple's 160 GB iPod classic guide (printed page 29) offers Shuffle in Settings
and in the Now Playing Center-tap cycle, while Repeat is described in Settings.
This informs the interaction design; it does not dictate our final placement or claim
that all generations used the same interaction.
[Apple user guide](https://cdsassets.apple.com/live/6GJYWVAV/user/ma1195_ipod_classic_160gb_user_guide.pdf).
