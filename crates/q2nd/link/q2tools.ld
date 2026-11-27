/* Code that only `--bench` (q2nd::perf) and `--script` (q2nd::script) run,
 * gathered into its own output section, which platform/resident.rs leaves
 * out of the locked set. Given to GNU ld with -T for Q2 builds (build.rs);
 * INSERT keeps the default script for everything else.
 *
 * Rust's v0 mangling spells a q2nd path as `..._4q2nd4perf...`, and every
 * function sits in its own `.text.<symbol>` input section (also
 * `.text.hot.`/`.text.unlikely.` with PGO). The patterns match functions of
 * the two modules, their closures and async state machines, and generic code
 * instantiated for their types when the symbol spells the path out (not after
 * a back-reference). ld parses -T scripts before the default script, so these
 * input sections are assigned here before the default `.text` rule sees
 * them (ld manual, INSERT). */
SECTIONS
{
  .q2tools :
  {
    *(.text.*_4q2nd4perf*)
    *(.text.*_4q2nd6script*)
  }
}
INSERT BEFORE .text;
