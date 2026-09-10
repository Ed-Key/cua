# CUA Driver Test Harnesses Guide

This contributor guide explains where the CUA Driver tests live, how to run
them, and where to add a new case. For current coverage and platform gaps, use
`test-matrix.md` and `action-support.md`.

## The Short Version

There are two main test layers:

1. **Unit and protocol tests** exercise Rust code and the public MCP/CLI
   contract without launching a real application.
2. **Harness E2E tests** launch a small application built from this repository,
   drive it through the Rust driver, and verify an external application or
   desktop state.

The Rust tests are the source of truth. Python tests, old shell runners, and
historical recording scripts are not part of the canonical E2E path.

The canonical E2E command on every OS runs the complete matrix and takes no
suite selector:

```text
Linux:  scripts/ci/linux/run-rust-e2e.sh
Windows: .\scripts\ci\windows\run-rust-e2e.ps1 -RequireGui
macOS:  libs/cua-driver/tests/runners/macos-lume/run-all.sh
```

The OS workflow may fan the complete matrix out into independent jobs for
reporting and failure isolation. That is an execution detail; contributors
should think of it as one canonical suite.

### Evidence authority

A canonical result is the complete repository harness run at the exact source
SHA. A one-off app smoke, manually assembled script, video, or environment
replay can diagnose a failure or provide release presentation evidence, but it
does not replace the complete matrix.

Windows and Linux use the repository's GitHub-hosted workflows when their
strict environment preflights pass. Windows Azure RDP runs are optional
environment-parity replays or a fallback when the hosted preflight cannot prove
a required capability. macOS uses the logged-in Lume maintainer wrapper.
For browser-facing changes or browser-use release certification, also run the
standalone Chrome/Edge matrix: the macOS wrapper accepts
`--standalone-browser`, while the Windows and Linux workflow is
`.github/workflows/e2e-rust-standalone-browsers.yml`.

Historical `*-plan.md`, `*-journal.md`, and release evidence documents record
what was run at that time. They are not current execution instructions and do
not override this guide or `scripts/ci/README.md`.

### Hyprland validation decision (2026-09-07)

[PR #3572](https://github.com/trycua/cua/pull/3572) is the dated, exact-source
result record for the input v3 candidate. It distinguishes ordinary CI, the
complete hosted X11, Sway, and Windows harnesses, native Hyprland acceptance,
and bounded app evidence. A native behavioral pass with a failed wrapper or
source-provenance check is not an accepted run. Use that record for results;
the coverage requirements below do not assert a passing result or add macOS
certification.

The maintainer selects the same canonical Linux Rust harness for native
Hyprland: run `scripts/ci/linux/run-rust-e2e.sh` with its complete `all` suite
in the prepared native Hyprland desktop at the exact candidate SHA. Preserve
the runner's required cells, assertions, and evidence checks. Record compositor
and backend provenance; an X11 run does not establish native Hyprland coverage.
Environment failures and failed cells remain failures, not permission to skip
tests or change expected results.

Before launching the native Hyprland harness, apply this map-time rule in the
disposable desktop's Lua configuration and reload it:

```lua
hl.window_rule({
    name = "cua-canonical-sentinel-animation",
    match = {title = "^CuaTestHarness Sentinel \\[cdp=[0-9]+\\]$"},
    no_anim = true,
})
```

The preflight independently verifies `no_anim` for the exact sentinel, then
waits for mapped fullscreen readiness before its first Driver activation.
`hyprctl clients` reports geometry goals, not animated surface bounds; repeated
equal goals alone cannot establish animation completion. This rule changes
only the test sentinel's animation, not its placement or the product's geometry
guard. Restore the original configuration after the run. It is a deterministic
fixture requirement, not an Omarchy user configuration requirement.

This is fixture regression coverage. For background `TARGET` input, the production compatibility gate in
`platform-linux/src/wayland/hyprland_compatibility.rs` admits only the qualified
native Calc `26.2.5-3` and Inkscape `1.4.4-6` packages. Ordinary GTK, Electron,
and Tauri fixtures do not qualify for v3 background raw input. Their declared refusals
can prove refusal behavior, but cannot prove plugin delivery or isolation.
Do not widen background production admission or add a test bypass to make them qualify.

The [accepted foreground extension](https://github.com/trycua/cua/issues/3550#issuecomment-5564996417)
adds a separate production `FOREGROUND_TARGET` route for ordinary native
top-level surfaces, advertised by `HELLO` with `foreground_target:true`.
It does not apply the Calc/Inkscape background package gate. The existing
complete suite covers defined native GTK3, Electron, and Tauri foreground
cases. Acceptance requires those cases to pass. Preserve the runner and tests.
Foreground activation and primary-cursor movement are intentional, with no
restoration promise. Verify exact-target delivery and refusals for held
keys/buttons, grabs, constraints, and drag-and-drop before primary takeover.
Foreground drag cancellation on primary-input/focus transitions requires
review and native evidence. No background refusal may escalate to this route.

Retain a short real-app production smoke and instrumented isolation proof on
both qualified apps as supporting compatibility evidence. Verify actual app
effects, plugin transport attribution, primary-seat isolation, and cleanup;
the uninstrumented smoke alone cannot establish those trace-based claims.
The retained proof at source `f180e8828b8f31cc153e3c44eaa89a9c13c5bc68`
includes 20 instrumented actions, nine pointer effects, and six observation
intervals, plus an uninstrumented six-action smoke. Saved outputs confirm Calc
cell A1 contains `a` and the Inkscape object's x-coordinate changes from 40 to
42 while y remains 60. The plugin tree and uninstrumented module hash are
unchanged at `1133a06e4f205cf80188a7ac9e41102f37611fea`. This is bounded
supporting evidence, not a complete app matrix or release-byte proof. Raw
background qualification remains native Calc from `libreoffice-fresh 26.2.5-3`,
Inkscape `1.4.4-6`, the plain compiled `evdev`/`pc105`/`us` keymap, and two
seats. Chromium, Electron, and XWayland raw background input remain unqualified;
semantic AT-SPI actions are separate.
See [production proof preparation](../hyprland-plugin/tests/production-proof.md)
for the bounded plans and their limits.

The explicit [Inkscape-only qualification profile](../hyprland-plugin/tests/production-inkscape-profile.md)
supports a bounded packaging candidate using exact Inkscape `1.4.4-6`, with
independent native clients, two app lanes, separate SVG oracles, and third-owner
capacity refusal. It preserves the default Calc/Inkscape profile and the native
all-suite gate. Product, harness, kit, and mapped module identities remain
separate; adding the profile records no new native passing result and does not
let diagnostic trace evidence certify trace-disabled package bytes.

Three complete repetitions of the long Python Calc/Inkscape plan, including
the 34 policy cases across both apps, are no longer a merge requirement.
Extended Python stress runs remain diagnostics for specific unresolved
failures. This decision changes test strategy, records no new passing result,
and does not waive the affected CI, native evidence, or release gates in
[RFC 3550](../../../rfcs/3550-hyprland-isolated-input.md).

## Repository Map

```text
cua/
|-- libs/cua-driver/
|   |-- rust/
|   |   |-- crates/
|   |   |   |-- cua-driver/          Rust driver and integration tests
|   |   |   |-- cua-driver-core/     Shared driver logic and unit tests
|   |   |   |-- cua-driver-testkit/  Shared Rust E2E helpers and evidence capture
|   |   |   |-- platform-linux/      Linux backend
|   |   |   |-- platform-macos/      macOS backend
|   |   |   |-- platform-windows/    Windows backend
|   |   |   `-- cursor-overlay/      Cursor evidence helper
|   |   |-- test-apps/               Ignored staged harness binaries
|   |   `-- Cargo.toml               Rust workspace
|   |-- tests/
|   |   |-- fixtures/
|   |   |   |-- shared/web/           Shared web page and external markers
|   |   |   |-- apps/                 Repo-local fixture sources
|   |   |   `-- build/                macOS/Linux/Windows fixture builders
|   |   `-- runners/
|   |       `-- macos-lume/           Maintainer Lume setup and guest entrypoint
|   `-- docs/                         Test matrix, reporting, and contributor docs
`-- scripts/ci/
    |-- linux/run-rust-e2e.sh         Linux canonical runner
    |-- windows/run-rust-e2e.ps1      Windows canonical runner
    `-- macos/run-rust-e2e.sh         macOS canonical runner
```

The important separation is:

| Layer                 | Owns                                                                      | Does not own                     |
| --------------------- | ------------------------------------------------------------------------- | -------------------------------- |
| Rust integration test | Scenarios, driver calls, assertions, action metadata                      | OS setup and fixture compilation |
| `cua-driver-testkit`  | Session helpers, fixture launching, screenshots, recordings, trajectories | The scenario list                |
| Fixture app           | Visible controls and externally observable state markers                  | Driver correctness assertions    |
| OS runner             | Build environment, user session, test selection, artifact collection      | Test behavior definitions        |

There is deliberately no second Python E2E implementation that the Rust suite
has to mirror.

## How One E2E Test Works

Every canonical E2E cell follows this shape:

```text
OS runner
  -> builds the Rust driver and repo-local fixture
  -> starts or connects to a real desktop session
  -> Rust testkit starts one harness application
  -> Rust test discovers the target window
  -> get_window_state provides accessibility tree and screenshot
  -> action addresses a target by AX element index or PX coordinates
  -> delivery is foreground or background where the action supports it
  -> fixture state, focus, pixels, or protocol response is checked
  -> testkit writes video, screenshots, trajectory, logs, and result data
```

A tool returning `ok` is not enough to pass an E2E cell. The fixture must show
that the action happened, or the test must verify a documented structured
refusal and the absence of focus or input side effects.

## The Test Layers

### Unit and Protocol Tests

These run without a repo-local GUI application and normally run without
`--ignored`:

| Location or prefix                     | What it proves                                                |
| -------------------------------------- | ------------------------------------------------------------- |
| `rust/crates/*/src/**`                 | Core driver, platform-independent logic, schemas, and helpers |
| `protocol_*_test.rs`                   | MCP handshake, tool calls, sessions, media, and errors        |
| `schema_*_test.rs`                     | Shared schema and backend consistency                         |
| `transport_config_persistence_test.rs` | CLI/MCP configuration persistence                             |
| `protocol_element_token_test.rs`       | Element-token protocol behavior                               |

These tests should be fast, deterministic, and safe to run on ordinary CI
workers. They do not prove that a real click, key, scroll, or background input
reached an application.

### Harness E2E Tests

These are Rust integration tests under:

```text
libs/cua-driver/rust/crates/cua-driver/tests/
```

Most are marked `#[ignore]` because they require a desktop, built fixtures, and
platform permissions. They are selected by the OS runner rather than the
ordinary unit command.

The canonical E2E suite has two behavior owners:

| Owner          | Purpose                                              |
| -------------- | ---------------------------------------------------- |
| Shared app     | Same web behavior tested through Electron and Tauri  |
| Native harness | Toolkit-specific controls and native window behavior |

WebView, CDP, and page-tool integration stays inside the shared or native
owner that exercises it. It is not a third public test family or command.

Delivery is not a test family. It is a dimension on each action row: an action
is tested in foreground and background modes whenever the driver and OS support
both. Capture and desktop scope are separate environment checks, while focus
preservation is a cross-cutting oracle that can be attached to any action row.
Focus, z-order, cursor, and desktop-state checks are cross-cutting invariants,
not a separate family. These names describe responsibilities, not separate
sources of truth or commands to run instead of the selector-free canonical
invocation.

## What The Complete Run Includes

### Windows

Runner: `scripts/ci/windows/run-rust-e2e.ps1`

| Runner area       | Rust test                         | Real harness or app                            |
| ----------------- | --------------------------------- | ---------------------------------------------- |
| Shared app matrix | `cross_platform_behavior_test.rs` | Electron and Tauri                             |
| Native controls   | `harness_wpf_test.rs`             | Repo-local WPF app                             |
| Native controls   | `harness_winui3_test.rs`          | Repo-local WinUI3 app                          |
| Web integration   | `harness_web_test.rs`             | WebView2 and Electron                          |
| Capture contract  | `capture_contract_test.rs`        | WPF plus driver tree/image output              |
| Launch contract   | `launch_windows_test.rs`          | Repo-local Electron launch and focus behavior  |
| Agent cursor      | `agent_cursor_windows_test.rs`    | Source-built cursor overlay and pixel evidence |
| Desktop scope     | `desktop_scope_windows_test.rs`   | Windowless desktop input and scope rejection   |

Cross-cutting instrumentation used by these rows includes the testkit
`DesktopObserver`, capture validation, cursor evidence, and desktop-scope
checks. These invariants are attached to their owning action rows rather than
run as a separate test family.

Windows is currently the broadest native matrix. It covers UIA controls, web
integration, background focus checks, and Windows-specific input routes. The
desktop observer is attached to shared and native action rows wherever
background delivery is tested.

### macOS

Runner: `libs/cua-driver/tests/runners/macos-lume/run-all.sh`

| Runner area               | Rust test                              | Real harness or app                     |
| ------------------------- | -------------------------------------- | --------------------------------------- |
| Shared app matrix         | `cross_platform_behavior_test.rs`      | Electron and Tauri                      |
| Native web matrix         | `cross_platform_behavior_test.rs`      | Repo-local WKWebView host               |
| Native controls           | `harness_appkit_test.rs`               | Repo-local AppKit app                   |
| Native controls           | `harness_swiftui_test.rs`              | Repo-local SwiftUI app                  |
| Installed app launch      | `installed_app_launch_macos_test.rs`   | Calculator and TextEdit                 |
| Installed app AX delivery | `installed_app_textedit_macos_test.rs` | TextEdit                                |
| Capture contract          | `capture_contract_test.rs`             | Installed driver and macOS capture APIs |
| Desktop scope             | `desktop_scope_macos_test.rs`          | macOS window and desktop scope          |

The WKWebView host runs the same typed shared-web catalog as Electron and
Tauri. Calculator and TextEdit add typed supporting rows for built-in app
launch focus and native Cocoa background value delivery. They run in the
canonical logged-in macOS lane, but they do not replace repo-local fixtures.
The maintainer wrapper provisions the exact source build and verifies the
private Lume seed's TCC/signing contract before delegating the behavior matrix
to `scripts/ci/macos/run-rust-e2e.sh`.

### Linux

Runner: `scripts/ci/linux/run-rust-e2e.sh`

| Runner area       | Rust test                         | Real harness or app       |
| ----------------- | --------------------------------- | ------------------------- |
| Shared app matrix | `cross_platform_behavior_test.rs` | Electron and Tauri        |
| Native controls   | `harness_gtk3_test.rs`            | Repo-local GTK3 app       |
| Capture contract  | `capture_contract_test.rs`        | Linux capture backend     |
| Desktop scope     | `desktop_scope_linux_test.rs`     | X11/Wayland desktop scope |

Linux has separate X11 and Wayland concerns. Nix supplies the reproducible
build and desktop environment, but the E2E test still needs an actual X11 or
Wayland session. Linux does not need GIF output; MP4, screenshots, accessibility
trees, trajectories, and logs are the useful evidence.

Wayland results are compositor-specific. The hosted lane uses Sway to prove
wlroots protocols. GNOME requires the optional WinRects Shell helper for
authoritative frame and buffer geometry, observation, capture, and verified
target activation. A portal/libei grant persists until the user revokes it, so
subsequent driver processes do not reopen the consent dialog.
KDE requires a future target-addressable KWin adapter; portal availability by
itself is not evidence that input can be sent safely to a named window.
Standard Wayland does not expose the physical pointer position, so canonical
Wayland rows do not claim the real-cursor preservation oracle. Focus, full
occlusion, sentinel input isolation, liveness, and fixture-state oracles remain
mandatory. [Issue #2194](https://github.com/trycua/cua/issues/2194) tracks
compositor, portal/libei, sentinel, and capture-based ways to add a proven
cursor observer where the environment supports one.

## AX, PX, and Delivery

See [`action-support.md`](action-support.md) for the current Windows, macOS, and Linux
delivery, refusal, and unproven-action ledger.

These terms describe different dimensions:

| Term          | Meaning                                                                     |
| ------------- | --------------------------------------------------------------------------- |
| AX            | Address a target through its accessibility/UI automation element            |
| PX            | Address a target by screen coordinates or pointer geometry                  |
| Foreground    | The target may be brought to the foreground for delivery                    |
| Background    | The target should receive the action without being raised or stealing focus |
| Window scope  | Capture or action is limited to one target window                           |
| Desktop scope | Capture or action covers the full desktop                                   |

The shared and native action matrices should test left click, right click,
double click, typing, keys, hotkeys, scroll, child windows, and drag across
AX/PX and foreground/background combinations where the driver supports them.
Unsupported background routes require an explicit refusal contract with an
allowed structured code and desktop-side-effect oracles. A refusal fails a
cell that requires delivery. There should not be a separate "delivery" family
whose only purpose is to repeat those same actions in the background.

Native harness rows use the same typed case/result contract as the shared
matrix. Current native `set_value` rows declare background delivery because
their contract includes no-focus and no-raise observations; actions without a
delivery concept use `not_applicable` explicitly.

## Cross-Cutting Invariants

The desktop observer is cross-cutting test instrumentation. It answers the same
question for any action, harness, or catalog area:

> Did the driver perform or reject the operation without disturbing the user's
> foreground application or desktop?

`cua-driver-testkit::DesktopObserver` owns the shared interface. Native Windows,
macOS, and Linux backends snapshot foreground-window, target z-order, cursor,
and focus state before and after an action. A separate full-desktop Electron
sentinel journals keyboard, pointer, wheel, visibility, focus, and heartbeat
events while it fully covers the target. Background rows opt into both pieces
of instrumentation directly; there is no special guard suite.

| Invariant or scenario     | What it checks                                                               |
| ------------------------- | ---------------------------------------------------------------------------- |
| Background click/type/key | The target action does not move focus away from the user's foreground window |
| Minimized app launch      | `launch_app(start_minimized=true)` does not raise the new app                |
| Background hotkey         | A keyboard chord does not steal focus                                        |
| Child-window click        | A target-created window does not unexpectedly become foreground              |
| Background screenshot     | Reading the target does not change focus or z-order                          |
| Agent cursor visibility   | The cursor appears in the captured pixels when enabled and moved             |

The sentinel contract fails closed when the target is only partly covered or
the heartbeat stops. Before any behavioral cells run, the strict environment
preflight deliberately sends input to the sentinel and deliberately raises the
background target. The lane proceeds only if the leaked input and transient
focus loss are observed, the sentinel is restored, and it once again fully
occludes the target. Windows, macOS, and X11 require the sentinel's live focus
journal to report the loss. Wayland uses the compositor-backed native focus
observer because Electron/Ozone does not reliably emit a DOM `blur` event for
an external surface focus transition. The sentinel heartbeat and leaked-input
journal remain mandatory on Wayland. This positive control prevents a broken
guard from making every background row look green.

A focus assertion can prove "no focus steal" while failing to prove that a
click changed the target application state. An action row must therefore check
both the target's external state and, when background delivery is under test,
the cross-cutting desktop observer.

These tests require a real interactive Windows user desktop. They reject
Session 0, locked desktops, and disconnected RDP sessions. Without
`CUA_REQUIRE_GUI=1`, an unusable desktop can self-skip for local development;
the canonical Windows runner enables the hard-failure behavior.

## Evidence

Canonical GUI runs are expected to produce evidence per test cell:

```text
artifacts/cua-driver/<os>/
|-- recordings/<cell-label>-pid<pid>-<sequence>/recording.mp4
|-- recordings/<cell-label>-pid<pid>-<sequence>/trajectory.json
|-- recordings/<cell-label>-pid<pid>-<sequence>/turn-*/before_state.json
|-- recordings/<cell-label>-pid<pid>-<sequence>/turn-*/before.png
|-- recordings/<cell-label>-pid<pid>-<sequence>/turn-*/after_state.json
|-- recordings/<cell-label>-pid<pid>-<sequence>/turn-*/after.png
|-- cases.jsonl
|-- environment.jsonl
|-- results.jsonl
|-- summary.md
`-- <rust-target>.log
```

The GitHub Actions summary contains one row per meaningful behavioral cell,
including its OS, harness, action, AX/PX targeting, delivery mode, driver route,
expected and observed behavior, oracles, and one evidence link. The link uses
the exact video path as its label and opens the owning lane archive. Unit tests
need normal test output and logs; they do not need desktop video.

## What Is Implemented Today

- Rust owns the canonical scenario definitions and external-state assertions.
- Electron and Tauri use the same shared web fixture across supported OSs.
- Native Windows, macOS, and Linux harnesses are repo-local applications built
  from source.
- The three OS runners use a selector-free command for the complete matrix.
- Shared and native harness owners emit the same typed v2 result records.
- Canonical GUI rows collect a trajectory and MP4, validated before reporting.
- Canonical GUI rows require parseable pre/post state and non-empty pre/post
  target-window images for each targeted turn. The one narrow exception is a
  successful Windows `bring_to_front` restore whose minimized target has no
  pre-action image or whose host capture remains unavailable afterward: it must
  retain the successful action response, captured post-action accessibility
  state, an explicit capture classification, trajectory, and MP4. Any other
  missing expected evidence fails the report.
- Per-cell video starts after fixture readiness and foreground/background
  posture. A 300 ms baseline precedes dispatch, and capture continues through
  external oracle collection. `trajectory.json` must finish with
  `behavior_video.status = "finalized"`.
- Use `agent_cursor_showcase_test` for cursor review media. Shared behavior
  matrix daemons deliberately use `--no-overlay` so synthetic cursor pixels do
  not contaminate action oracles; their videos prove tool behavior, not cursor
  rendering.
- Windows hosted runs use `GetConsoleWindow` to select the inherited
  HostedComputeAgent/runner console, verify its identity, and minimize it
  through `ShowWindow(SW_MINIMIZE)` before fixture or sentinel posture is
  established. The sentinel remains a separate test fixture and is reasserted
  after console cleanup.
- Strict lane preflights fail on missing fixtures, desktop access, permissions,
  accessibility, capture, recording support, or ineffective background guards
  instead of silently skipping.
- Canonical runners set `CUA_E2E_FORBID_SKIPS=1`. Unfiltered shared runs also
  set `CUA_E2E_EXPECTED_MIN_CELLS` to 80 on Windows/Linux and 120 on macOS, so
  a filtered, shortened, or accidentally emptied catalog cannot report green.
  Explicit diagnostic cell or harness filters disable only the minimum-count
  check; matching no cells still fails inside the Rust matrix.
- GitHub summaries link every evidence-bearing row to its lane archive and
  display the exact recording path. The trajectory path remains in the typed
  evidence and archive.
- Unit/protocol tests remain separate from interactive E2E tests.

## What Still Needs Implementation

The remaining work is platform coverage and validation, not another test
hierarchy:

1. **Broaden native action rows.** The shared web matrix covers every declared
   AX/PX and foreground/background cell. AppKit, SwiftUI, WPF, WinUI3,
   WebView2, and non-GTK3 Linux toolkits still have unproven native combinations listed in
   [`action-support.md`](action-support.md).
2. **Preserve exact-source validation.** Every accepted platform run must record
   one immutable source SHA and retain the typed evidence contract.
3. **Close representative-desktop gaps.** Hosted Sway passes the complete
   Electron, Tauri, GTK3, capture, and desktop-scope catalogs. A real GNOME 46
   session passes GTK3, capture, and desktop scope, but still needs the shared
   renderer catalog and portal-video parity. Plasma 6 still needs a verified
   KWin activation adapter and its first accepted behavioral lane. Issue `#1922`
   tracks the grouped backend work.
4. **Add representative toolkit surfaces.** GTK4, Qt5/Qt6, VTE, VCL, and GL
   canvases remain optional real-app gaps; shared Electron/Tauri coverage does
   not substitute for those native stacks.
5. **Flake cleanup.** Replace remaining fixed native waits with external-state
   polling and add fixture reset tokens before reusing a harness process.

## File Convergence Plan

The goal is not to put every assertion into one enormous test file. The goal is
to give each behavior one clear owner and make cross-cutting evidence reusable.

### Target Ownership

```text
rust/crates/cua-driver-testkit/src/
`-- observer.rs                     Cross-OS desktop-side-effect interface

rust/crates/cua-driver/tests/
|-- cross_platform_behavior_test.rs Shared Electron/Tauri action matrix
|-- harness_wpf_test.rs             Windows WPF action rows
|-- harness_winui3_test.rs          Windows WinUI3 action rows
|-- harness_web_test.rs             WebView2/Electron page and CDP rows
|-- harness_appkit_test.rs          macOS AppKit action rows
|-- harness_swiftui_test.rs         macOS SwiftUI action rows
|-- harness_gtk3_test.rs            Linux GTK3 action rows
|-- capture_contract_test.rs        Tree and screenshot read contract
|-- desktop_scope_<os>_test.rs      Window/desktop scope invariants
`-- protocol_*_test.rs              Protocol and schema tests
```

The desktop observer is a helper, not a test family. An action row invokes it when
the row is testing background delivery. The row then records both outcomes:

1. Did the target application state change, or did the driver return the
   documented structured refusal?
2. Did focus, z-order, cursor, and desktop state remain within the contract?

Focus, z-order, cursor, and input-leak assertions belong to the typed action
rows that exercise them; launch, capture, cursor, and desktop-scope contracts
retain their narrowly owned scenarios. The canonical runner is the only
user-facing command; lane selectors are internal diagnostics.

## Contributor Workflow

When adding a new scenario:

1. Add or update the repo-local fixture and its external state marker.
2. Add the Rust scenario under `rust/crates/cua-driver/tests/`.
3. Declare AX/PX addressing, foreground/background delivery, scope, and oracle.
4. Add the scenario to `docs/test-matrix.md` and this guide when it changes the
   cross-OS structure.
5. Update only the OS runner selection when the test is platform-specific.
6. Run the smallest Rust test locally, then run the OS command before
   calling the matrix complete.

The goal is one understandable Rust E2E model across platforms, with
platform-specific harnesses where the OS genuinely differs.

### Focused exact-window cursor geometry (Slice A)

Run `harness_gtk3_test slice_a_linux_window_move` for the focused Linux case.
On X11 it compares fractional screenshot targets against independent
`xwininfo` geometry before and after moving the fixture window, reverses the
capture downscale, and checks the real pointer and focus. The checked move
requires a live XID owned by the requested PID, positive dimensions, and a
same-screen XTranslateCoordinates reply. Desktop and legacy untargeted moves
retain their existing coordinate conventions.

Native Wayland support has separate geometry and overlay requirements:

- Hyprland resolves the exact address and PID. Its toplevel capture already
  normalizes exported pixels to logical window dimensions, so the move uses
  capture scale 1 after reversing screenshot downscaling. This does not infer
  scale from a display mode or from a title match.
- Sway resolves an exact tree ID, but the current output-crop capture does not
  attest the buffer scale and output identity. Exact-window cursor moves
  refuse with a detail naming the unproven capture scale.
- Other compositor paths, including foreign-toplevel PID/title matching,
  do not establish this exact window transform. They refuse with a detail
  naming the missing exact geometry and capture scale. Native Wayland moves
  never consult X11 geometry as a fallback.
- Even when geometry is proven, a native overlay requires layer-shell or the
  GNOME Shell cursor helper. Missing overlay support is reported separately
  from missing geometry. No real-pointer injection substitutes for an overlay.

These limitations use `code: background_unavailable`,
`reason: unsupported_operation`, and `effect: refused`, before cursor registry
or visual updates. Pure tests cover proven 1x and 2x transforms, missing scale,
missing geometry, heuristic identity, and unchanged state on refusal. Those
unit results do not certify a native compositor runtime. Run the selected GTK
case on the actual compositor to collect placement or explicit refusal evidence.

The Windows focused case is `agent_cursor_windows_test
slice_a_windows_window_move`. It independently reads DWM bounds and window DPI,
checks the downscaled screenshot target numerically, and checks pointer and
focus. Windows target compilation and host execution of the pure geometry
module do not replace this interactive desktop test.

## Focused local macOS Slice A evidence

These ignored AppKit rows support the local quick-approach policy. They are not
full-matrix or cross-platform certification. With the overlay enabled, native
Click-family delivery intentionally waits for target-frame acknowledgement,
with an approximate 80 to 140 ms approach and a bounded 250 ms admission wait.
The former under-5-percent enabled/disabled latency gate is obsolete. Core
Animation submission does not guarantee physical scanout or visibility through
other windows. Earlier debug clips and asynchronous-playback clips remain
historical milestones, not evidence for a newer candidate.

| Row | Evidence |
| --- | --- |
| `harness_appkit_counter_px_background` | Reused AX hit-test row: exactly one counter transition, registry target, foreground, z-order, real cursor and leaked-input oracles. |
| `slice_a_cursor_window_geometry` | Actual 2x window screenshot on the right half, moved window, explicit 600-pixel resized screenshot, invalid-window refusal, independent native bounds and annotated painted tip. |
| `slice_a_cursor_first_target` | Fresh named session, near-target first appearance, quick approach, observed arrow arrival versus observed counter change, then target pulse. |
| `slice_a_cursor_display_geometry` | Reused strict unmirrored 2x plus 1x secondary display with a negative origin. Missing hardware fails its precondition. |
| `slice_a_cursor_latency` | Descriptive paired timings and native background oracles, separately named `same_point` and `different_target` setup modes. No performance pass threshold. |

One physical display with 1512 by 982 logical points and 3024 by 1964 pixels can
exercise the local 2x rows. It cannot qualify native 1x, secondary-display or
negative-origin behavior. Record these as unavailable, not passed. Synthetic
helper tests cannot replace those native rows. A successful click smoke without
all desktop oracles is not isolation certification.

### Preparation and candidate identity

Only the controller performs native runs, builds/installs the candidate and
starts its TCC-authorized CuaDriverLocal daemon. Preserve the actual installer
invocation and output, with `CUA_DRIVER_SOURCE_SHA` set to the exact clean Git
SHA during the build. The local installer supports `--release` and
`--require-stable-signing`. Do not reconstruct a build transcript afterward.
Use the existing `CUA_LOG=cua_cursor_approach=debug` setting on the candidate
process and retain its stderr in a regular file for timing runs.

Set the installed candidate executable and dedicated socket explicitly:

```sh
cd /Users/edkiboma/Projects/cua/libs/cua-driver/rust
export CUA_TEST_DRIVER_BIN="$HOME/.local/bin/cua-driver-local"
export CUA_E2E_MACOS_DAEMON_SOCKET="$HOME/Library/Caches/cua-driver-local/cua-driver-local.sock"
export CUA_TEST_REQUIRE_FIXTURES=1
export CUA_REQUIRE_GUI=1
export CUA_SLICE_A_CANDIDATE=/absolute/evidence/candidate.json
```

`candidate.json` contains these required fields. Artifact paths resolve relative
to that JSON file; all hashes are SHA-256 of the actual retained bytes:

```json
{
  "candidate_sha": "FULL_40_HEX_GIT_SHA",
  "daemon_pid": 12345,
  "candidate_binary_sha256": "64_HEX_SHA256_OF_INSTALLED_SIGNED_EXECUTABLE",
  "build_profile": "release",
  "build_log": {"path": "build.log", "sha256": "64_HEX_SHA256"},
  "launch_log": {"path": "launch.json", "sha256": "64_HEX_SHA256"}
}
```

The build transcript must contain the actual `CUA_DRIVER_SOURCE_SHA=<sha>`
invocation and Cargo's `Finished` line for the declared profile. `debug` maps
to Cargo's `dev` build profile and is accepted only for visual milestones;
final timing requires `release`. `launch_log` now references a structured
`launch.json`, with these required fields:

- `daemon_pid`, canonical absolute `executable`, `executable_sha256`, and
  canonical absolute `socket`, matching the candidate's actual running process.
- `command`: the actual launch argument array; `launched_epoch_ms`: the observed
  launch wall time; `process_started`: the trimmed original output of
  `LC_ALL=C ps -p <PID> -o lstart=`. Do not synthesize missing observations.
- `environment`: the recorded launch environment switches. The harness checks
  `CUA_LOG` and `CUA_PRIVATE_CURSOR_ORDER_TRACE` against the live process without
  saving its complete environment. Avoid including unrelated secrets.
- `stderr`: canonical absolute path of the actual regular file attached to
  descriptor 2; `stderr_identity`: `{ "device": <st_dev>, "inode": <st_ino> }`
  observed at launch. The harness checks both against the running process.
- `transcript`: `{ "path": "launch.log", "sha256": "64_HEX_SHA256" }`, retaining
  the original launch transcript alongside the structured record.

Timing requires exactly `CUA_LOG=cua_cursor_approach=debug`, with
`CUA_PRIVATE_CURSOR_ORDER_TRACE` unset or disabled. The approach log path must
match that process's verified stderr. The title-free ordering diagnostic remains
opt-in and bounded to one probe per display, at most 120 presentation samples
within two seconds per request. It observes current application eligibility and
before/after native stacks. Its overhead is not normal latency evidence.

Each new row checks a clean tracked checkout, current HEAD, socket-owning PID,
process start record, executable hashes, signature verification and production
`get_config.source_sha` from a persistent MCP connection. The binary path uses
testkit `driver_binary()`, including its explicit override. A file hash alone
cannot identify an already-loaded image. Keep the daemon unchanged between
capture and verification; a newer process cannot certify old frames.

The controller must have the AppKit fixture and Electron foreground sentinel
built through `tests/fixtures/build/macos.sh`, and the necessary Accessibility
and screen-capture grants. The fixed-size AppKit window is positioned through
native AX fixture setup. Cursor moves and clicks under test always go through
the candidate's MCP proxy. Native CGWindow and AX readbacks independently check
the window, control bounds and counter. Returned registry coordinates are not
painted-arrow evidence.

Every new visual/timing session explicitly sets `cua.default` with
`reduced_motion: "off"` and a session-only `max_image_dimension: 4096`, then
checks the effective readback. `reduced_motion` is an enum, not a boolean.
Screenshots and subsequent actions use the same public session label. The
baseline must have resize ratio 1 and native pixel dimensions; the resized
row must have a 600-pixel long edge and ratio greater than 1. Requesting a large
`max_dimension` without the session override would not bypass the configured
capture ceiling. No global settings are changed.

### Capture, annotate, then verify without repeating input

Start the controller's external recorder before the capture command. Retain the
original video, crop and clock provenance. First-target acceptance requires the
complete selected display, so an earlier arrow at another location cannot be
hidden by cropping. Cropped pilot clips remain supporting evidence. Window
geometry stages may use a crop. Capture the visible fixture, counter
label, full expected seed region and arrow, with no resizing of the video.
Requested 120 fps is only a request. Use measured decoded PTS values and retain
all frames in each selected interval. No recorder is started by these rows.

Use a new directory per capture, and different reporter files per phase:

```sh
export CUA_SLICE_A_RUN_DIR=/absolute/evidence/first-target
export CUA_SLICE_A_PHASE=capture
export CUA_E2E_DECLARATIONS_FILE=/absolute/evidence/first-capture-declarations.jsonl
export CUA_E2E_RESULTS_FILE=/absolute/evidence/first-capture-results.jsonl
cargo test --offline -p cua-driver --test harness_appkit_test slice_a_cursor_first_target -- --ignored --exact --nocapture --test-threads=1
```

Capture intentionally reports an error/pending result after saving `ready.json`,
`trace.json`, window screenshots, raw call results and `disabled.json`. It cannot
pass before pixel evidence exists. The fixture closes after capture. The
controller stops the recorder, annotates it, and reruns only verification:

```sh
export CUA_SLICE_A_PHASE=verify
export CUA_E2E_DECLARATIONS_FILE=/absolute/evidence/first-verify-declarations.jsonl
export CUA_E2E_RESULTS_FILE=/absolute/evidence/first-verify-results.jsonl
cargo test --offline -p cua-driver --test harness_appkit_test slice_a_cursor_first_target -- --ignored --exact --nocapture --test-threads=1
```

Repeat this procedure for `slice_a_cursor_window_geometry` in a different new
run directory. That row produces `right-half`, `moved` and `resized` stages.
Verification does not relaunch the fixture or repeat the cursor actions.
Keep capture-phase failures separate from final verification results.

For each stage, supply `<stage>-visual.json`, plus `disabled-visual.json`:

```json
{
  "trace_sha256": "64_HEX_SHA256_OF_TRACE_JSON",
  "stage": "first-click",
  "video": {"path": "original.mov", "sha256": "64_HEX_SHA256"},
  "capture_log": {"path": "capture.json", "sha256": "64_HEX_SHA256"},
  "video_started_epoch_ms": 1789075000000.0,
  "clock_uncertainty_ms": 2.0,
  "first_frame": 20,
  "annotation_method": "Manual tip, ring center and counter read on every selected original frame",
  "crop_bounds": [0.0, 0.0, 1512.0, 982.0],
  "pixel_size": [3024, 1964],
  "scale": 2.0,
  "frames": [
    {"index": 20, "pts": 0.166667,
     "image": {"path": "frames/000020.png", "sha256": "64_HEX_SHA256"},
     "tip": [], "pulse_center": [], "counter": 0}
  ]
}
```

This is a schema illustration, not valid acceptance evidence: provide every
frame in the selected interval, not the single example frame. `index` is the
original video's zero-based frame index. `pts` is its decoded timestamp in
seconds. `tip` and `pulse_center` are independently annotated native-pixel
`[x,y]` positions relative to the crop. An empty array means inspected and
absent. `counter` is the visible integer, or explicit `null` when unreadable;
omitting it is malformed evidence. Do not mark the label, click ring or
registry target as the arrow tip.

`capture.json` records `candidate_sha`, `daemon_pid`, `session` from `ready.json`,
`video_sha256`, `video_started_epoch_ms`, `clock_uncertainty_ms`, `crop_bounds`,
`pixel_size`, `scale`, the exact `capture_command` as a string array, and a
nonempty `clock_alignment_method`. Derive video epoch alignment from the
recorder's actual start/sample-clock records, retaining that procedure. Do not
use a guessed launch delay. Uncertainty over 10 ms fails the clock precondition.
The crop must be inside the observed physical display.

Extract frames and timings from the original file without changing frame rate:

```sh
mkdir -p frames
ffprobe -v error -select_streams v:0 -show_frames -show_entries frame=best_effort_timestamp_time,width,height -of json original.mov > frame-times.json
ffmpeg -v error -i original.mov -map 0:v:0 -fps_mode passthrough -start_number 0 frames/%06d.png
```

The verifier re-decodes the selected original range with `ffmpeg`, checks every
PNG's pixels and hash, and compares timestamps and dimensions with `ffprobe`.
A stage interval must begin at least 50 ms before its call and continue at least
100 ms after return, including pulse expiry for first-click. It must include
all preceding empty frames in that interval. The disabled interval must contain
at least 300 ms of absent arrow/ring, entirely after its recorded disable plus
50 ms and before `disabled.json`'s end.

Numeric conversion and registry errors allow at most one screen unit per axis;
painted tip and ring centers allow two native pixels. First-click verification
requires a near-target first appearance and samples consistent with approximate
80 to 140 ms travel and a 150 ms pulse. It reports frame-bounded intervals,
not exact renderer timestamps. A counter change before arrival fails. A change
in the same frame as arrival, unreadable counter, insufficient travel samples,
a gap over 40 ms, no intermediate tip distinct from both endpoints by more than
4 native pixels (combined annotation tolerance), or unresolved timing remains **indeterminate**, with the row
still failing/pending. An earlier captured arrival frame establishes sampled UI
ordering only. It does not establish a separate earlier physical scanout, the
native accepted-input timestamp, or causality from a delayed label redraw.
Production boundary tests supply the dispatch/contact ordering evidence.

For the deferred mixed-display row, `CUA_SLICE_A_DISPLAY_EVIDENCE` contains
`candidate_sha`, `candidate_binary_sha256`, `daemon_pid`, `build_profile` and
exactly two `captures`. Each capture has `display_id`, `bounds`, `scale`,
`screenshot`, `screenshot_sha256`, `target`, separately recorded `registry`,
`measured_tip_pixels`, and a `capture_log` artifact. That log repeats the four
candidate fields, screenshot hash and `measurement_method`. Retain native
full-display screenshots and MCP/capture-session provenance. Do not run this
row on the single-display setup or substitute synthetic captures.

### Descriptive timing and isolation

Stop external and behavior recordings before timing. The row checks
`get_recording_state.enabled == false` before and after measured calls, uses an
unrecorded MCP proxy, and requires an explicit external-recorder-stopped
attestation. Candidate configuration must report PiP disabled; no PiP is started. The separate first-target artifacts must prove
actual enabled and disabled pixels. Indeterminate sampled arrival/counter
ordering may accompany descriptive timings and is retained as such in the
report; it never becomes first-target acceptance.

```sh
unset CUA_E2E_RECORDINGS_ROOT
export CUA_SLICE_A_PHASE=measure
export CUA_SLICE_A_PREFLIGHT_DIR=/absolute/evidence/first-target
export CUA_SLICE_A_EXTERNAL_CAPTURE_STOPPED=1
export CUA_SLICE_A_APPROACH_LOG=/absolute/evidence/candidate-stderr.log
export CUA_SLICE_A_TIMING_MODE=different_target
export CUA_SLICE_A_RUN_DIR=/absolute/evidence/timing-different-target
export CUA_E2E_DECLARATIONS_FILE=/absolute/evidence/timing-declarations.jsonl
export CUA_E2E_RESULTS_FILE=/absolute/evidence/timing-results.jsonl
cargo test --offline -p cua-driver --test harness_appkit_test slice_a_cursor_latency -- --ignored --exact --nocapture --test-threads=1
```

Use `same_point` and a separate new run directory for the other planned mode.
Each run performs 10 alternating warmup clicks and three blocks of 30 complete
pairs. Pair order is enabled/disabled, disabled/enabled, enabled/disabled across
the three blocks. All pairs share a session, candidate and exact fixture
geometry. Before each sample, same-point setup requests the click target;
different-target setup requests a point 100 logical units to its right. The
setup move, overlay toggle, 400 ms pacing, AX readbacks, config reads and file IO
are outside the measured call. A setup request does not itself prove rendering
or an already-arrived physical arrow.

`samples.json` has `mode`, ten `warmups` (`enabled`, `ns`) and 90 `pairs`
(`block`, `pair`, `enabled_first`, `enabled_ns`, `disabled_ns`). Empty,
nonfinite, incomplete, reordered or malformed samples fail. Block prefixes and
per-call raw results survive later failures. The report contains per-condition
median and nearest-rank p95, plus descriptive enabled/disabled ratios for each
block and the aggregate. There is no bootstrap, seed or ratio verdict for this
amended policy. Do not repeat runs until a preferred result appears.

Per-sample `*-timing.json` and `*-approach.log` retain original byte ranges from
the candidate's verified stderr, its device/inode identity, and the extracted
bytes' hash. One open descriptor spans RPC and read; changed path identity,
truncation, short reads or a slice over 1 MiB fail. The raw sample retains both
actual wall-clock endpoints and the monotonic RPC duration. No timestamps are
invented when logs or capture clock measurements are missing. Enabled samples require exactly one
matching registered/ack_received/released action identity, in that line order.
RFC3339 timestamps must also be ordered and fall inside the actual RPC bracket,
with a conservative 5 ms admission tolerance. Wall and monotonic RPC durations
must agree within that tolerance. This tolerance is not measured clock precision.
The records must contain finite ordered
first-frame, target-frame, submission and acknowledgement measurements. Disabled
samples require no approach record. Missing, rotated, mixed or incomplete logs
fail rather than invent timing. `rpc_outside_registered_approach_ms` subtracts
the **measured** registration-to-acknowledgement interval from the measured RPC;
it includes resolution, native delivery, readback, restoration and transport.
It is not a measurement of pure dispatch overhead. Submission/acknowledgement
measurements make no physical scanout guarantee. Private debug logging remains
enabled for both timing conditions and is part of this instrumented observation.

Timing success means complete descriptive data and passing unchanged background
oracles. Also run the existing `harness_appkit_counter_px_background` row once
on the same verified final candidate, serially, through the same socket. Its
prior-candidate success does not certify the current candidate. Do not suppress
real-pointer changes or any other observer violation. Preserve failures and
investigate their cause before asserting local isolation.

Disabled-overlay annotations must cover the complete selected physical display
at its recorded native scale, even for the geometry row. An empty unrelated crop
cannot establish absence of prior cursor or pulse artwork. Preserve every
original capture, raw transcript and structured provenance file. A later still,
an overlay-only image and a video frame are separate instants unless their
observed timing establishes otherwise.

### Deferred Windows/Linux integration limitation

This local Mac work does not establish cross-platform compatibility. The shared
`RenderStateCore::new` now starts at `(0, 0)` with `placed = false`, but Windows
`platform-windows/src/overlay.rs::seed_start_in_map` and X11
`platform-linux/src/overlay.rs::seed_start_if_sentinel` still require `pos.0 < -50`.
Their first command therefore skips target-adjacent seeding and can plan from the
origin. The Windows `seed_moves_sentinel_cursor_on_screen_for_first_action` test
contradicts that retained predicate by source inspection. Assigning `core.pos`
alone would also fail to establish explicit placement. Retained sign-based paint
checks hide valid negative-X positions. Wayland's
`platform-linux/src/wayland/overlay.rs::apply_keyed_command` retains the same
sentinel seed pattern.

These are concrete adapter integration regressions, separate from unrun native
qualification. The user deferred their correction; the early parity contribution
is retained. A future source fix and focused adapter tests are required before
universal landing. No Windows/Linux quick-approach support or native result is
claimed here. Physical 1x, secondary display and negative-origin Mac qualification
also remain pending when that hardware is unavailable.

Helper-only preparation, with no native run:

```sh
cargo test --offline -p cua-driver --test harness_appkit_test slice_a_latency:: -- --nocapture
cargo test --offline -p cua-driver --test harness_appkit_test slice_a_identity_tests -- --nocapture
cargo test --offline -p cua-driver --test harness_appkit_test slice_a_cursor --no-run
```

These focused commands do not run the complete GUI module or desktop matrix.
