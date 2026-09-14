# Unknown action effects and foreground recommendations

The public action result must not infer delivery failure from a recommended
next input route. An internal record with effect Unverifiable and a foreground
recommendation now publishes escalation.reason effect_unconfirmed. Its effect
remains unverifiable, its route and delivery mode stay intact, and no evidence
or delivered-character count is invented.

This corrects a misleading report observed when background Electron typing
fully reached the renderer but the driver could not independently confirm it.
The recommendation itself is not proof that foreground input is necessary.
Observe the current app state before retrying text. Repeating an unknown edit
can duplicate text that has already arrived.

Explicit refused and partial records retain their existing classification.
Suspected no-ops retain suspected_noop; permission and stale-surface reasons
remain distinct. Confirmed results still require their existing evidence.
This correction changes no input routing or platform permissions.

The mapping lives in cua-driver-core and applies to all platform adapters.
Focused tests exercise macOS, Windows, X11 and libei transport records without
requiring those operating systems. A macOS live replay supplies application
evidence for the observed Electron case. No Windows or Linux desktop delivery
claim follows from the shared formatter tests. The public schema is unchanged.

```sh
cargo test -p cua-driver-core action_record::tests --lib
```

Related reports include trycua/cua issue 3389. Pull request 3811 addresses a
different stale-element readback issue; this formatter correction does not
replace that contribution or claim to solve its Contacts reproduction.
