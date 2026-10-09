# Codex inbox upgrade fixture

`v0.48.0-two-pending.json` was written by Dione's tagged `v0.48.0` code at
commit `68e6da5`. In a disposable checkout of that tag, a temporary test called
`CodexEventQueue::load`, enqueued two notifications with message IDs `4101` and
`4102` through the public queue API, dropped the queue, and copied the resulting
`codex-inbox.json` bytes here. The temporary producer test is not part of this
repository. SHA-256 of the fixture is
`946127bd98a829fccf0bc3f3e651ee04007f376af45d30cd9397c9a57249fcfe`.
The upgrade test asserts this hash so an accidental fixture rewrite fails.

The upgrade test copies these bytes into a fresh state directory and loads them
in another process. Keep the fixture fixed when the current inbox serializer
changes; add a new fixture from the next prior release instead of rewriting it.
