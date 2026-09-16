# Task 4a: native panels and capture

Status: implementation and focused native-platform compile complete. Full task 4 is not complete. Signed VM live evidence, resolved target publication, and cursor composition belong to later parent milestones.

## Changes

- The companion consumes validated version 2 ObserverSnapshot messages with SnapshotReceiver. Main publishes session snapshots. The control reader remains independent of presentation and native capture and exits the private helper on EOF.
- Sessions hold independent generation, image mailbox, availability and visibility state per opaque preview ID. Snapshot revisions prevent older capture/render snapshots restoring removed owners. Missing IDs remove only those owners.
- Frames transfer under a short mailbox lock. AppKit receives detached draws after unlocking, so a static window's only frame is no longer dropped on renderer contention. Existing exact-window PID validation, 1280px aspect-preserving bounds, queue depth 3 and 8fps limit remain.
- AppKit objects live in a main-thread local map in observer_panels.rs. The legacy MacosPipBackend singleton remains separate and unchanged.
- Each native NSPanel has title `Agent <ID>`, stable background hue, native title bar/close/resize controls, nonactivating style and floating level 3. Initial placement staggers; later renders preserve user geometry except clamping to available visible display bounds.
- Native close hides a retained panel; updates never order existing panels front. Shared status item `Previews` contains `Show Agent <ID>` entries targeting native orderFront:. Image pixels and labels are read-only.
- Hidden state is sampled during UI refresh. The capture worker stops hidden streams and resumes reopened streams on its next 500ms iteration. No hard agent cap.

## Test evidence

All commands ran in /Users/edkiboma/Projects/cua-pip-observer, with CARGO_TARGET_DIR=/Users/edkiboma/Projects/cua-parity-integrated/libs/cua-driver/rust/target.

1. Added per-owner presentation and slow-render/hidden-state tests first. `cargo test -p platform-macos --lib pip:: --manifest-path libs/cua-driver/rust/Cargo.toml` failed at missing Sessions/ObserverSnapshot seams: /tmp/task4-red.log.
2. Added geometry preservation/off-display regression before implementing constrain. Same command failed at missing constrain/render: /tmp/task4-red2.log.
3. First complete implementation passed 9 focused tests: /tmp/task4-green.log.
4. Added snapshot replay regression and independent control-reader test. Regression failed with `click` replacing `Preview unavailable`: /tmp/task4-red3.log. Added revision rejection to the presentation map.
5. Final focused suite: 11 passed, 0 failed: /tmp/task4-green2.log. Covers ownership isolation/removal, detached static frame retention, persistent hide, generation rejection, replay rejection, unavailable state, resource release/coalescing, aspect bounds, available-display geometry and independent control reader.
6. `cargo check -p cua-driver --manifest-path libs/cua-driver/rust/Cargo.toml` passed: /tmp/task4-check.log. Two dead-code warnings remain for the retained legacy observer publisher path after switching main to sessions.
7. rustfmt applied only to owned source files; git diff --check passed.

## Limits and follow-up evidence

- No host GUI interaction, native launch, screenshots, permission prompts, public GitHub writes or desktop E2E matrix were performed.
- Parent must verify two live signed VM panels, source pixel identity, independent resize/hide/reopen, owner removal and helper/action isolation. Pure tests do not certify native behavior or Objective-C runtime selector/encoding compatibility.
- One worker serializes native capture startup/stop. A stuck native capture call can delay other preview capture and visibility polling, but the daemon action path and independent helper control/EOF remain isolated. A ponytail comment records the ceiling and per-owner-worker upgrade path.
- Hidden capture pause is eventual, normally within one or two 500ms iterations. It is not instantaneous and depends on AppKit and the capture worker remaining responsive.
- Cursor composition, exact resolved-target metadata, source activation, Windows and Linux are explicitly deferred. No Pause or Show source controls were introduced.
- Retained contribution credit from trycua/cua#3497 is preserved in the capture module and commit coauthor trailer. No source PR communication occurred because public writes are outside this private task.
