# Native test fixtures

`cadence.py` writes the generated configuration and SQLite queue used by the
listening cadence consumer tests. The media argument is a synthetic file.
The test covers the byte layout and server binding of that queue.

`q2-pstore` is the installed OS collector from the selected q2-os source
revision. The player runs it in source-only mode against disposable pstore
records and checks the archive through its real diagnostics reader. Set
`Q2_PSTORE_COLLECTOR` to the absolute path of an updated OS collector to check
that producer before refreshing the fixture.
