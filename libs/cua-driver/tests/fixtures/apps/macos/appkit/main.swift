// CuaTestHarness.AppKit — deterministic Cocoa AppKit host app for the
// cua-driver-rs test harness. Mirrors the role of CuaTestHarness.Wpf.
//
// Single-file Swift, compiled with swiftc against AppKit. Wrapped in a
// minimal .app bundle by build.sh so AX / TCC behaves like a real app.
//
// Scenarios covered (see ../scenarios/scenarios.json `appkit` section):
//   counter        — NSButton increments NSTextField counter
//   text_body      — NSTextField with the shared HARNESS_TEXT_MARKER_v1
//   text_input     — NSTextField with a mirror label for type_text / set_value
//                    and an opt-in controlled child-process command oracle
//   click_target   — NSButton (AX-addressable) records click/double_click/right_click
//   slider         — NSSlider drives drag / set_value (slider_value=)
//   checkable_controls — NSButton checkbox (agreed=)
//   context_menu   — NSButton + NSMenu (Cut/Copy/Paste → menu_action=)
//   scroll_target  — NSScrollView with a tall body and offset label
//   ns_menubar     — main menu item with known title (Mac-specific)
//   exit           — NSButton terminates the app
//
// AX identifiers (via `setAccessibilityIdentifier(_:)`) match the IDs in
// scenarios.json. Window title is set to "CuaTestHarness AppKit" so the
// Rust tests can find it via NSWorkspace / accessibility queries.

import AppKit

// MARK: - Constants (must match ../scenarios/scenarios.json `appkit`)
let kWindowTitle = "CuaTestHarness AppKit"
let kWindowAID = "wnd-main"
let kIncrementButtonAID = "btn-increment"
let kCounterLabelAID = "lbl-counter"
let kResetButtonAID = "btn-reset"
let kTextBodyAID = "txt-body"
let kTextBodyMarker = "HARNESS_TEXT_MARKER_v1"
let kTextInputAID = "txt-input"
let kTextInputMirrorAID = "lbl-input-mirror"
let kTextInputCommitAID = "lbl-input-commit"
let kClickTargetAID = "btn-clicktarget"
/// Present only when CUA_APPKIT_THIEF_DELAY_MS is set (focus-theft tests).
let kStealButtonAID = "btn-steal"
/// Present only when CUA_APPKIT_OPENER is set (act_and_read surface notes).
let kOpenWindowButtonAID = "btn-open-window"
let kOpenedWindowTitle = "CuaTestHarness Opened"
let kLastActionAID = "lbl-last-action"
let kClickCountAID = "lbl-click-count"
let kSliderAID = "sld-value"
let kSliderValueAID = "lbl-slider-value"
let kCheckboxAID = "chk-agree"
let kCheckStateAID = "lbl-chk-state"
let kSelectionStateAID = "lbl-selection-state"
let kContextButtonAID = "btn-context"
let kMenuActionAID = "lbl-menu-action"
let kScrollerAID = "scroll-tall"
let kScrollOffsetAID = "lbl-scroll-offset"
let kAccelCountAID = "lbl-accel-count"
let kScrollTopMarker = "SCROLL_TOP_MARKER_v1"
let kScrollBottomMarker = "SCROLL_BOTTOM_MARKER_v1"
let kExitButtonAID = "btn-exit"
let kMenuItemTitle = "Harness Test Item"
let kSecondaryWindowTitle = "CuaTestHarness AppKit Secondary"
let kSheetWindowTitle = "CuaTestHarness AppKit Sheet"
let kFloatingWindowTitle = "CuaTestHarness AppKit Floating"

// MARK: - Controls

/// An `NSTextField` whose `AXValue` catches up with the text it holds only
/// after `CUA_APPKIT_AX_VALUE_LAG_MS`, growing a character at a time.
///
/// AppKit rebuilds a field's editor around an insertion, so the value read
/// microseconds after an `AXSelectedText` write is a prefix of what landed:
/// measured in Contacts, "(408) " of "(408) 961-1560", complete ~20 ms later.
/// Unset, the field behaves like a stock `NSTextField`.
final class LaggingTextField: NSTextField {
    private var reported = ""
    private var changedAt: Date?

    override func accessibilityValue() -> String? {
        let actual = stringValue
        let lag = HarnessWindowController.envSeconds("CUA_APPKIT_AX_VALUE_LAG_MS")
        let unreadable = ProcessInfo.processInfo.environment["CUA_APPKIT_AX_VALUE_UNREADABLE"] == "1"
        guard lag > 0 || unreadable else { return actual }
        if actual != reported {
            reported = actual
            changedAt = Date()
        }
        guard let changedAt, !actual.isEmpty else { return actual }
        let elapsed = Date().timeIntervalSince(changedAt)
        let value: String?
        if unreadable {
            value = nil
        } else {
            let visible = max(1, Int(Double(actual.count) * elapsed / lag))
            value = elapsed >= lag ? actual : String(actual.prefix(visible))
        }
        if let path = ProcessInfo.processInfo.environment["CUA_APPKIT_AX_VALUE_TRACE"],
           let data = try? JSONSerialization.data(withJSONObject: ["actual": actual, "reported": value.map { $0 as Any } ?? NSNull(), "elapsed": elapsed]) {
            let line = data + Data([10])
            if let handle = FileHandle(forWritingAtPath: path) {
                handle.seekToEndOfFile()
                handle.write(line)
                handle.closeFile()
            } else {
                try? line.write(to: URL(fileURLWithPath: path))
            }
        }
        return value
    }
}

final class WindowDiscoveryApplication: NSApplication {
    private var discoveryMode: String? {
        ProcessInfo.processInfo.environment["CUA_HARNESS_AX_WINDOW_DISCOVERY"]
    }

    override func accessibilityWindows() -> [Any]? {
        guard let mode = discoveryMode else { return super.accessibilityWindows() }
        if mode == "listed-sibling" {
            // AppKit can clear mainWindow when the sentinel takes focus. Keep
            // the intended target absent from AXWindows throughout the test.
            return super.accessibilityWindows()?.filter {
                ($0 as? NSWindow)?.accessibilityIdentifier() != kWindowAID
            }
        }
        return []
    }

    override func accessibilityChildren() -> [Any]? {
        guard discoveryMode != nil else { return super.accessibilityChildren() }
        return super.accessibilityChildren()?.filter { !($0 is NSWindow) }
    }

    override func accessibilityMainWindow() -> Any? {
        switch discoveryMode {
        case "focused", "none": return nil
        case "invalid": return self
        default: return super.accessibilityMainWindow()
        }
    }

    override func accessibilityFocusedWindow() -> Any? {
        switch discoveryMode {
        case "main", "none", "invalid": return nil
        default: return super.accessibilityFocusedWindow()
        }
    }
}

// MARK: - Controller

final class HarnessWindowController: NSObject, NSTextFieldDelegate, NSTableViewDataSource, NSTableViewDelegate, NSMenuItemValidation {
    private var editorIdentity: EditorIdentityFixture?
    /// True while show() clears AppKit's automatic first responder, so ending
    /// that edit does not record a commit the test never made.
    private var clearingLaunchFocus = false
    /// Focus-theft fixture: pressing btn-steal makes this app take focus
    /// after this delay, the way an app reacting to a click often does.
    private let thiefDelayMs = ProcessInfo.processInfo.environment["CUA_APPKIT_THIEF_DELAY_MS"]
        .flatMap { Int($0) }
    let window: NSWindow
    /// Opened by btn-open-window; kept so it is not released while shown.
    private var openedWindow: NSWindow?
    let counterLabel = NSTextField(labelWithString: "counter=0")
    var counterValue = 0
    let textInput = LaggingTextField(string: "")
    let textInputMirror = NSTextField(labelWithString: "")
    let textInputCommit = NSTextField(labelWithString: "committed=none")
    let lastActionLabel = NSTextField(labelWithString: "last_action=none")
    let clickCountLabel = NSTextField(labelWithString: "clicks=0")
    var clicks = 0
    let sliderValueLabel = NSTextField(labelWithString: "slider_value=0")
    let checkStateLabel = NSTextField(labelWithString: "agreed=false")
    let selectionItems = ["alpha", "beta", "gamma"]
    let selectionTable = NSTableView()
    let selectionStateLabel = NSTextField(labelWithString: "selection=none")
    let menuActionLabel = NSTextField(labelWithString: "menu_action=none")
    let scrollOffsetLabel = NSTextField(labelWithString: "scroll_offset=0")
    let accelCountLabel = NSTextField(labelWithString: "accel_fired=0")
    var accelCount = 0
    var keyMonitor: Any?

    // Pinned content size — every launch MUST produce a byte-identical window
    // so screenshot dimensions (and the hardcoded pixel coords the harness tests
    // rely on) never drift.
    static let kContentSize = NSSize(width: 720, height: 860)

    override init() {
        let rect = NSRect(origin: NSPoint(x: 100, y: 100), size: HarnessWindowController.kContentSize)
        // No `.resizable`: a resizable window can be left at a different size,
        // and macOS would persist/restore that drifted frame on the next launch.
        let mask: NSWindow.StyleMask = [.titled, .closable, .miniaturizable]
        window = NSWindow(contentRect: rect, styleMask: mask, backing: .buffered, defer: false)
        window.title = kWindowTitle
        window.setAccessibilityIdentifier(kWindowAID)
        window.isReleasedWhenClosed = false
        // Deterministic geometry across launches. macOS persists and restores a
        // window's frame by default (Cocoa state restoration + frame autosave),
        // so a window that was nudged/resized — or laid out a hair differently on
        // a prior run — reopens at a drifted height (observed 832 vs 858 pt),
        // shifting screenshot dimensions and breaking tests that assume fixed
        // pixel coords. Opt out of restoration entirely and re-pin the content
        // size on every launch so each run is identical.
        window.isRestorable = false
        window.setFrameAutosaveName("")
        window.setContentSize(HarnessWindowController.kContentSize)
        super.init()
        buildContent()
        installKeyboardMonitor()
    }

    func show() {
        window.makeKeyAndOrderFront(nil)
        window.center()
        // AppKit makes the first text field the window's first responder when
        // the window becomes key, so every run would start with txt-input
        // already focused. Start with no focused control instead: tests that
        // exercise focus preparation and change notifications need a field
        // that is not yet being edited, and tests that need focus set it.
        clearingLaunchFocus = true
        window.makeFirstResponder(nil)
        clearingLaunchFocus = false
    }

    // MARK: - Layout

    private func buildContent() {
        let content = NSStackView()
        content.orientation = .vertical
        content.alignment = .leading
        content.spacing = 8
        content.edgeInsets = NSEdgeInsets(top: 12, left: 20, bottom: 12, right: 20)
        content.translatesAutoresizingMaskIntoConstraints = false

        // counter
        content.addArrangedSubview(sectionLabel("counter"))
        let counterRow = NSStackView()
        counterRow.orientation = .horizontal
        counterRow.spacing = 12
        let inc = NSButton(title: "Increment", target: self, action: #selector(onIncrement))
        inc.setAccessibilityIdentifier(kIncrementButtonAID)
        let reset = NSButton(title: "Reset", target: self, action: #selector(onReset))
        reset.setAccessibilityIdentifier(kResetButtonAID)
        counterLabel.setAccessibilityIdentifier(kCounterLabelAID)
        counterLabel.font = NSFont.monospacedSystemFont(ofSize: 18, weight: .semibold)
        counterRow.addArrangedSubview(inc)
        counterRow.addArrangedSubview(reset)
        counterRow.addArrangedSubview(counterLabel)
        if thiefDelayMs != nil {
            let steal = NSButton(title: "Steal focus", target: self, action: #selector(onSteal))
            steal.setAccessibilityIdentifier(kStealButtonAID)
            counterRow.addArrangedSubview(steal)
        }
        if ProcessInfo.processInfo.environment["CUA_APPKIT_OPENER"] != nil {
            let open = NSButton(title: "Open window", target: self, action: #selector(onOpenWindow))
            open.setAccessibilityIdentifier(kOpenWindowButtonAID)
            counterRow.addArrangedSubview(open)
        }
        content.addArrangedSubview(counterRow)

        // text_body
        content.addArrangedSubview(sectionLabel("text_body"))
        let body = NSTextField(labelWithString:
            "This is the body of the harness test app. Marker: \(kTextBodyMarker). " +
            "Used to verify get_window_state can extract known text.")
        body.setAccessibilityIdentifier(kTextBodyAID)
        body.maximumNumberOfLines = 3
        body.preferredMaxLayoutWidth = 600
        content.addArrangedSubview(body)

        // text_input
        content.addArrangedSubview(sectionLabel("text_input"))
        textInput.setAccessibilityIdentifier(kTextInputAID)
        textInput.placeholderString = "Type here…"
        textInput.delegate = self
        textInput.translatesAutoresizingMaskIntoConstraints = false
        textInputMirror.setAccessibilityIdentifier(kTextInputMirrorAID)
        textInputMirror.font = NSFont.monospacedSystemFont(ofSize: 14, weight: .regular)
        textInputCommit.setAccessibilityIdentifier(kTextInputCommitAID)
        textInputCommit.font = NSFont.monospacedSystemFont(ofSize: 14, weight: .regular)
        let inputRow = NSStackView()
        inputRow.orientation = .horizontal
        inputRow.spacing = 12
        inputRow.addArrangedSubview(textInput)
        inputRow.addArrangedSubview(textInputMirror)
        inputRow.addArrangedSubview(textInputCommit)
        // Opt-in read-only text field (CUA_APPKIT_READONLY_FIELD=1): an
        // AXTextField whose AXValue is not settable, for set_value refusal.
        if ProcessInfo.processInfo.environment["CUA_APPKIT_READONLY_FIELD"] == "1" {
            // Stays an AXTextField (a non-editable NSTextField would report
            // AXStaticText) but refuses the AX value setter.
            let readOnly = AXValueLockedTextField(string: "fixed text")
            readOnly.setAccessibilityIdentifier("txt-readonly")
            inputRow.addArrangedSubview(readOnly)
        }
        NSLayoutConstraint.activate([
            textInput.widthAnchor.constraint(equalToConstant: 240),
        ])
        content.addArrangedSubview(inputRow)

        if let fixture = EditorIdentityFixture(environment: ProcessInfo.processInfo.environment) {
            editorIdentity = fixture
            content.addArrangedSubview(fixture.row)
        }

        // click_target — a REAL NSButton so it is in the AX tree and addressable
        // by element_index (AppKit NSButton ignores synthetic pixel clicks, but
        // AXPress works). AXPress / single mouse → click; pixel double → double_click;
        // right-click → right_click. (matches WPF btn-clicktarget contract.)
        content.addArrangedSubview(sectionLabel("click_target"))
        let clickTarget = ClickTargetButton(title: "Click target (left / right / double)",
                                             target: self, action: #selector(onClickTarget))
        clickTarget.harness = self
        clickTarget.setAccessibilityIdentifier(kClickTargetAID)
        lastActionLabel.setAccessibilityIdentifier(kLastActionAID)
        lastActionLabel.font = NSFont.monospacedSystemFont(ofSize: 14, weight: .regular)
        clickCountLabel.setAccessibilityIdentifier(kClickCountAID)
        clickCountLabel.font = NSFont.monospacedSystemFont(ofSize: 14, weight: .regular)
        accelCountLabel.setAccessibilityIdentifier(kAccelCountAID)
        accelCountLabel.font = NSFont.monospacedSystemFont(ofSize: 14, weight: .regular)
        let clickRow = NSStackView()
        clickRow.orientation = .horizontal
        clickRow.spacing = 12
        clickRow.addArrangedSubview(clickTarget)
        clickRow.addArrangedSubview(lastActionLabel)
        clickRow.addArrangedSubview(clickCountLabel)
        clickRow.addArrangedSubview(accelCountLabel)
        content.addArrangedSubview(clickRow)

        // slider — NSSlider drives the `drag` / `set_value` tools (AXValue).
        content.addArrangedSubview(sectionLabel("slider"))
        let slider = NSSlider(value: 0, minValue: 0, maxValue: 100,
                              target: self, action: #selector(onSlider))
        slider.setAccessibilityIdentifier(kSliderAID)
        slider.isContinuous = true
        slider.translatesAutoresizingMaskIntoConstraints = false
        sliderValueLabel.setAccessibilityIdentifier(kSliderValueAID)
        sliderValueLabel.font = NSFont.monospacedSystemFont(ofSize: 14, weight: .regular)
        let sliderRow = NSStackView()
        sliderRow.orientation = .horizontal
        sliderRow.spacing = 12
        sliderRow.addArrangedSubview(slider)
        sliderRow.addArrangedSubview(sliderValueLabel)
        NSLayoutConstraint.activate([slider.widthAnchor.constraint(equalToConstant: 320)])
        content.addArrangedSubview(sliderRow)

        // checkable_controls — NSButton checkbox toggles AXValue.
        content.addArrangedSubview(sectionLabel("checkable_controls"))
        let checkbox = NSButton(checkboxWithTitle: "I agree",
                                target: self, action: #selector(onCheckbox(_:)))
        checkbox.setAccessibilityIdentifier(kCheckboxAID)
        checkbox.state = .off
        checkStateLabel.setAccessibilityIdentifier(kCheckStateAID)
        checkStateLabel.font = NSFont.monospacedSystemFont(ofSize: 14, weight: .regular)
        let checkRow = NSStackView()
        checkRow.orientation = .horizontal
        checkRow.spacing = 12
        checkRow.addArrangedSubview(checkbox)
        checkRow.addArrangedSubview(checkStateLabel)
        selectionTable.headerView = nil
        selectionTable.allowsMultipleSelection = true
        selectionTable.dataSource = self
        selectionTable.delegate = self
        let selectionColumn = NSTableColumn(identifier: NSUserInterfaceItemIdentifier("selection-column"))
        selectionColumn.width = 140
        selectionTable.addTableColumn(selectionColumn)
        let selectionScroll = NSScrollView()
        selectionScroll.documentView = selectionTable
        selectionScroll.hasVerticalScroller = true
        selectionScroll.borderType = .lineBorder
        selectionScroll.translatesAutoresizingMaskIntoConstraints = false
        selectionStateLabel.setAccessibilityIdentifier(kSelectionStateAID)
        selectionStateLabel.font = NSFont.monospacedSystemFont(ofSize: 14, weight: .regular)
        checkRow.addArrangedSubview(selectionScroll)
        checkRow.addArrangedSubview(selectionStateLabel)
        NSLayoutConstraint.activate([
            selectionScroll.widthAnchor.constraint(equalToConstant: 150),
            selectionScroll.heightAnchor.constraint(equalToConstant: 68),
        ])
        content.addArrangedSubview(checkRow)

        // context_menu — NSButton with an attached NSMenu. Right-click opens the
        // native contextual menu; selecting an item updates menu_action=.
        content.addArrangedSubview(sectionLabel("context_menu"))
        let contextButton = NSButton(title: "Right-click for context menu",
                                     target: nil, action: nil)
        contextButton.setAccessibilityIdentifier(kContextButtonAID)
        let ctxMenu = NSMenu()
        for title in ["Cut", "Copy", "Paste"] {
            let item = NSMenuItem(title: title, action: #selector(onContextItem(_:)), keyEquivalent: "")
            item.target = self
            item.setAccessibilityIdentifier("ctx-\(title.lowercased())")
            ctxMenu.addItem(item)
        }
        contextButton.menu = ctxMenu
        menuActionLabel.setAccessibilityIdentifier(kMenuActionAID)
        menuActionLabel.font = NSFont.monospacedSystemFont(ofSize: 14, weight: .regular)
        let contextRow = NSStackView()
        contextRow.orientation = .horizontal
        contextRow.spacing = 12
        contextRow.addArrangedSubview(contextButton)
        contextRow.addArrangedSubview(menuActionLabel)
        content.addArrangedSubview(contextRow)

        // scroll_target
        content.addArrangedSubview(sectionLabel("scroll_target"))
        let scrollWrap = NSStackView()
        scrollWrap.orientation = .horizontal
        scrollWrap.spacing = 12
        let scroller = NSScrollView(frame: NSRect(x: 0, y: 0, width: 480, height: 120))
        scroller.translatesAutoresizingMaskIntoConstraints = false
        scroller.hasVerticalScroller = true
        scroller.borderType = .lineBorder
        let bodyText = NSTextView(frame: NSRect(x: 0, y: 0, width: 460, height: 600))
        bodyText.isEditable = false
        // The NSScrollView's AXScrollArea is NOT surfaced by get_window_state — only
        // the document AXTextArea is. Put scroll-tall on the document view so the
        // scroller_aid contract resolves to the actual scrollable AX node.
        bodyText.setAccessibilityIdentifier(kScrollerAID)
        // Keep this small (one AX node per line on macOS) so the get_window_state
        // tree walk doesn't exhaust its element budget before reaching the rest
        // of the scenarios. 30 lines is plenty for verifying scroll offset.
        var bigBody = kScrollTopMarker + "\n"
        for i in 0..<30 {
            bigBody += "Line \(i)\n"
        }
        bigBody += kScrollBottomMarker
        bodyText.string = bigBody
        scroller.documentView = bodyText
        scrollOffsetLabel.setAccessibilityIdentifier(kScrollOffsetAID)
        scrollOffsetLabel.font = NSFont.monospacedSystemFont(ofSize: 14, weight: .regular)
        NotificationCenter.default.addObserver(
            self, selector: #selector(onScroll),
            name: NSView.boundsDidChangeNotification,
            object: scroller.contentView)
        scroller.contentView.postsBoundsChangedNotifications = true
        scrollWrap.addArrangedSubview(scroller)
        scrollWrap.addArrangedSubview(scrollOffsetLabel)
        NSLayoutConstraint.activate([
            scroller.widthAnchor.constraint(equalToConstant: 480),
            scroller.heightAnchor.constraint(equalToConstant: 120),
        ])
        content.addArrangedSubview(scrollWrap)

        // exit
        let exit = NSButton(title: "Exit", target: self, action: #selector(onExit))
        exit.setAccessibilityIdentifier(kExitButtonAID)
        content.addArrangedSubview(exit)

        // No outer scroll-view wrap: the content is sized to fit the window
        // so the only scrollable surface is the inner scroll_target NSScrollView.
        // Otherwise scroll events delivered at window-local coords get
        // consumed by the outer scroll view before reaching the inner one,
        // and `scroll` tool tests can't deterministically move the inner offset.
        let container = NSView(frame: window.contentLayoutRect)
        container.autoresizingMask = [.width, .height]
        content.translatesAutoresizingMaskIntoConstraints = false
        container.addSubview(content)
        NSLayoutConstraint.activate([
            content.topAnchor.constraint(equalTo: container.topAnchor),
            content.leadingAnchor.constraint(equalTo: container.leadingAnchor),
            content.trailingAnchor.constraint(equalTo: container.trailingAnchor),
            content.widthAnchor.constraint(equalToConstant: 700),
        ])
        window.contentView = container
    }

    private func sectionLabel(_ id: String) -> NSTextField {
        let f = NSTextField(labelWithString: "▸ \(id)")
        f.font = NSFont.systemFont(ofSize: 13, weight: .bold)
        f.textColor = .secondaryLabelColor
        return f
    }

    private func installKeyboardMonitor() {
        keyMonitor = NSEvent.addLocalMonitorForEvents(matching: .keyDown) {
            [weak self] event in
            guard let self else { return event }
            let flags = event.modifierFlags.intersection(.deviceIndependentFlagsMask)
            let chordFlags: NSEvent.ModifierFlags = [.control, .shift]
            let key = event.charactersIgnoringModifiers?.lowercased()
            let isChord = flags.contains(chordFlags) && key == "k"
            let hasModifiers = !flags.intersection([.command, .control, .option, .shift]).isEmpty
            let isPlainF5 = event.keyCode == 96 && !hasModifiers
            if isChord || isPlainF5 {
                self.accelCount += 1
                self.accelCountLabel.stringValue = "accel_fired=\(self.accelCount)"
                return nil
            }
            return event
        }
    }

    deinit {
        if let keyMonitor {
            NSEvent.removeMonitor(keyMonitor)
        }
    }

    // MARK: - Actions

    @objc private func onIncrement() {
        counterValue += 1
        counterLabel.stringValue = "counter=\(counterValue)"
    }

    /// Milliseconds from `name`, as seconds. Absent or unparseable is 0.
    static func envSeconds(_ name: String) -> TimeInterval {
        guard let raw = ProcessInfo.processInfo.environment[name],
              let ms = Double(raw), ms > 0 else { return 0 }
        return ms / 1000
    }

    /// Opens a second window without activating the app, like an app that
    /// shows a panel in response to a click. Ordered behind so it cannot
    /// cover the user's front window during background tests.
    @objc private func onOpenWindow() {
        let opened = NSWindow(
            contentRect: NSRect(x: 80, y: 80, width: 320, height: 140),
            styleMask: [.titled, .closable], backing: .buffered, defer: false)
        opened.title = kOpenedWindowTitle
        opened.isReleasedWhenClosed = false
        opened.contentView = NSTextField(labelWithString: "opened by btn-open-window")
        opened.orderBack(nil)
        openedWindow = opened
    }

    @objc private func onSteal() {
        stealFocus(after: thiefDelayMs ?? 0, window: window)
    }

    @objc private func onReset() {
        counterValue = 0
        counterLabel.stringValue = "counter=0"
    }

    @objc private func onExit() {
        NSApp.terminate(nil)
    }

    @objc private func onScroll(_ note: Notification) {
        guard let clip = note.object as? NSClipView else { return }
        scrollOffsetLabel.stringValue = "scroll_offset=\(Int(clip.documentVisibleRect.origin.y))"
    }

    @objc private func onSlider(_ sender: NSSlider) {
        sliderValueLabel.stringValue = "slider_value=\(Int(sender.doubleValue.rounded()))"
    }

    @objc private func onCheckbox(_ sender: NSButton) {
        checkStateLabel.stringValue = "agreed=\(sender.state == .on)"
    }

    func numberOfRows(in tableView: NSTableView) -> Int {
        selectionItems.count
    }

    func tableView(_ tableView: NSTableView,
                   viewFor tableColumn: NSTableColumn?, row: Int) -> NSView? {
        let value = selectionItems[row]
        let field = NSTextField(labelWithString: value)
        field.isSelectable = true
        field.setAccessibilityIdentifier("selection-\(value)")
        return field
    }

    func tableViewSelectionDidChange(_ notification: Notification) {
        let values = selectionTable.selectedRowIndexes.map { selectionItems[$0] }
        selectionStateLabel.stringValue = values.isEmpty
            ? "selection=none"
            : "selection=\(values.joined(separator: ","))"
    }

    @objc private func onContextItem(_ sender: NSMenuItem) {
        menuActionLabel.stringValue = "menu_action=\(sender.title)"
    }

    @objc func onArrangeLeft(_ sender: NSMenuItem) {
        menuActionLabel.stringValue = "menu_action=window_arrange_left"
    }

    func validateMenuItem(_ menuItem: NSMenuItem) -> Bool {
        if menuItem.action == #selector(onArrangeLeft(_:)) {
            // Real macOS Window-menu commands are contextual: the application
            // being active is insufficient when the requested window is not
            // key. Keep this fixture honest so invoke_menu must establish the
            // exact window context before resolving the final item.
            return NSApp.isActive && window.isKeyWindow
        }
        return true
    }

    func controlTextDidChange(_ obj: Notification) {
        guard let field = obj.object as? NSTextField else { return }
        if field === textInput {
            textInputMirror.stringValue = field.stringValue
        }
    }

    func controlTextDidEndEditing(_ obj: Notification) {
        guard !clearingLaunchFocus, let field = obj.object as? NSTextField else { return }
        if field === textInput {
            textInputCommit.stringValue = "committed=\(field.stringValue)"
            runControlledCommand(field.stringValue)
        }
    }

    /// Test-only terminal-like command seam. It accepts exactly one harmless
    /// synthetic command and has a child process create the external oracle;
    /// arbitrary field contents are never executed.
    private func runControlledCommand(_ command: String) {
        guard command == "printf cua-press-key",
              let oraclePath = ProcessInfo.processInfo.environment["CUA_APPKIT_COMMAND_ORACLE"]
        else { return }

        let process = Process()
        process.executableURL = URL(fileURLWithPath: "/bin/sh")
        process.arguments = [
            "-c",
            "printf cua-press-key > \"$1\"",
            "cua-appkit-command",
            oraclePath,
        ]
        do {
            try process.run()
        } catch {
            textInputCommit.stringValue = "command_error=launch_failed"
        }
    }

    // Click target — single source of truth for all three actions.
    @objc func onClickTarget() { recordClick("click") }
    func clickTargetSawDouble() { recordClick("double_click") }
    func clickTargetSawRight() { recordClick("right_click") }
    private func recordClick(_ action: String) {
        clicks += 1
        lastActionLabel.stringValue = "last_action=\(action)"
        clickCountLabel.stringValue = "clicks=\(clicks)"
    }
}

// MARK: - Click target button

// A real NSButton (so it shows up in the AX tree and is element_index-addressable
// via AXPress) that additionally reports double-click and right-click. AXPress and
// single mouse-up fire the target/action (→ click); a pixel double-click is caught
// here before super so it reports double_click; right-click reports right_click.
final class ClickTargetButton: NSButton {
    weak var harness: HarnessWindowController?

    override func mouseDown(with event: NSEvent) {
        if event.clickCount == 2 {
            harness?.clickTargetSawDouble()
            return
        }
        super.mouseDown(with: event)
    }
    override func rightMouseDown(with event: NSEvent) {
        harness?.clickTargetSawRight()
    }
    override func acceptsFirstMouse(for event: NSEvent?) -> Bool { true }
}

// Opt-in windows for the persistent exact-window activation certification.
// Ordinary harness launches keep the existing window layout and behavior.
final class BringToFrontMatrixWindows: NSObject, NSTextFieldDelegate {
    let secondary: NSWindow
    var sheet: NSWindow?
    var floating: NSPanel?
    private var editField: NSTextField?
    private let editMirror = NSTextField(labelWithString: "")
    private let editCommit = NSTextField(labelWithString: "committed=none")

    init(parent: NSWindow, mode: String) {
        secondary = NSWindow(
            contentRect: NSRect(x: 40, y: 40, width: 420, height: 240),
            styleMask: [.titled, .closable], backing: .buffered, defer: false)
        super.init()
        secondary.title = kSecondaryWindowTitle
        secondary.isReleasedWhenClosed = false
        secondary.isRestorable = false
        secondary.contentView = NSTextField(labelWithString: "bring_to_front secondary ordinary window")
        if mode == "editable" {
            let content = NSView(frame: NSRect(x: 0, y: 0, width: 420, height: 240))
            let field = NSTextField(string: "")
            field.frame = NSRect(x: 20, y: 180, width: 380, height: 24)
            field.setAccessibilityIdentifier("txt-sibling-input")
            field.delegate = self
            editField = field
            editMirror.frame = NSRect(x: 20, y: 140, width: 380, height: 24)
            editMirror.setAccessibilityIdentifier("lbl-sibling-mirror")
            editCommit.frame = NSRect(x: 20, y: 100, width: 380, height: 24)
            editCommit.setAccessibilityIdentifier("lbl-sibling-commit")
            let endEdit = NSButton(title: "End sibling edit", target: self,
                                   action: #selector(endSiblingEdit))
            endEdit.frame = NSRect(x: 20, y: 40, width: 180, height: 32)
            endEdit.setAccessibilityIdentifier("btn-sibling-end-edit")
            for view in [field, editMirror, editCommit, endEdit] { content.addSubview(view) }
            secondary.contentView = content
        }
        secondary.orderFront(nil)
        if mode == "editable",
           let draft = ProcessInfo.processInfo.environment["CUA_HARNESS_SIBLING_DRAFT"],
           let field = editField {
            // Seed a real, uncommitted edit before agent observation begins.
            // Let AppKit deliver its normal delegate callback to the mirror.
            field.selectText(nil)
            guard let editor = field.currentEditor() else { fatalError("missing sibling field editor") }
            editor.insertText(draft)
        }

        if mode == "sheet" {
            let candidate = NSWindow(
                contentRect: NSRect(x: 0, y: 0, width: 360, height: 160),
                styleMask: [.titled], backing: .buffered, defer: false)
            candidate.title = kSheetWindowTitle
            candidate.contentView = NSTextField(labelWithString: "modal sheet blocks parent key status")
            sheet = candidate
            parent.beginSheet(candidate)
        } else if mode == "floating" {
            let candidate = NSPanel(
                contentRect: NSRect(x: 180, y: 180, width: 320, height: 140),
                styleMask: [.titled, .utilityWindow], backing: .buffered, defer: false)
            candidate.title = kFloatingWindowTitle
            candidate.level = .floating
            candidate.isFloatingPanel = true
            candidate.contentView = NSTextField(labelWithString: "floating accessory panel")
            candidate.orderFront(nil)
            floating = candidate
        }
    }

    func controlTextDidChange(_ notification: Notification) {
        guard let field = notification.object as? NSTextField, field === editField else { return }
        editMirror.stringValue = field.stringValue
    }

    func controlTextDidEndEditing(_ notification: Notification) {
        guard let field = notification.object as? NSTextField, field === editField else { return }
        editCommit.stringValue = "committed=\(field.stringValue)"
    }

    @objc private func endSiblingEdit() {
        secondary.makeFirstResponder(nil)
    }

}

func writeBringToFrontWindowReport(
    main: NSWindow,
    matrix: BringToFrontMatrixWindows?
) {
    guard let path = ProcessInfo.processInfo.environment["CUA_HARNESS_WINDOW_REPORT"] else {
        return
    }
    var lines = ["main=\(main.windowNumber)"]
    if let matrix {
        lines.append("secondary=\(matrix.secondary.windowNumber)")
        if let sheet = matrix.sheet {
            lines.append("sheet=\(sheet.windowNumber)")
        }
        if let floating = matrix.floating {
            lines.append("floating=\(floating.windowNumber)")
        }
    }
    do {
        try (lines.joined(separator: "\n") + "\n").write(
            toFile: path, atomically: true, encoding: .utf8)
    } catch {
        fputs("failed to write window report: \(error)\n", stderr)
    }
}

// MARK: - Menu bar (Mac-specific scenario: ns_menubar)

func installMenuBar(target: HarnessWindowController) {
    let main = NSMenu()
    let appItem = NSMenuItem()
    main.addItem(appItem)
    let appMenu = NSMenu(title: "App")
    let testItem = NSMenuItem(title: kMenuItemTitle, action: nil, keyEquivalent: "")
    testItem.setAccessibilityIdentifier("menu-test-item")
    appMenu.addItem(testItem)
    appMenu.addItem(NSMenuItem.separator())
    appMenu.addItem(NSMenuItem(title: "Quit",
                               action: #selector(NSApplication.terminate(_:)),
                               keyEquivalent: "q"))
    appItem.submenu = appMenu

    let windowItem = NSMenuItem(title: "Window", action: nil, keyEquivalent: "")
    let windowMenu = NSMenu(title: "Window")
    let arrangeItem = NSMenuItem(title: "Arrange", action: nil, keyEquivalent: "")
    let arrangeMenu = NSMenu(title: "Arrange")
    let leftItem = NSMenuItem(
        title: "Left",
        action: #selector(HarnessWindowController.onArrangeLeft(_:)),
        keyEquivalent: ""
    )
    leftItem.target = target
    leftItem.setAccessibilityIdentifier("menu-window-arrange-left")
    arrangeMenu.addItem(leftItem)
    arrangeItem.submenu = arrangeMenu
    windowMenu.addItem(arrangeItem)
    windowItem.submenu = windowMenu
    main.addItem(windowItem)
    NSApp.mainMenu = main
    NSApp.windowsMenu = windowMenu
}

// MARK: - Entry

/// Take focus after `delay` ms, the way an app reacting to a click often
/// does, and record the attempt (CUA_APPKIT_THIEF_TRACE) so a test can prove
/// the theft was actually tried.
func stealFocus(after delay: Int, window: NSWindow?) {
    DispatchQueue.main.asyncAfter(deadline: .now() + .milliseconds(delay)) {
        NSApp.activate(ignoringOtherApps: true)
        window?.makeKeyAndOrderFront(nil)
        let attempted = Date().timeIntervalSince1970
        DispatchQueue.main.asyncAfter(deadline: .now() + .milliseconds(100)) {
            guard let path = ProcessInfo.processInfo.environment["CUA_APPKIT_THIEF_TRACE"] else { return }
            let line = "{\"delay_ms\":\(delay),\"attempted_at\":\(attempted),\"active_after_100ms\":\(NSApp.isActive)}\n"
            if let handle = FileHandle(forWritingAtPath: path) {
                handle.seekToEndOfFile()
                handle.write(Data(line.utf8))
                handle.closeFile()
            } else {
                FileManager.default.createFile(atPath: path, contents: Data(line.utf8))
            }
        }
    }
}

/// An accessibility text field whose AXValue is not settable, like a field
/// an app has locked. NSTextField always reports its value settable, so this
/// view describes itself to accessibility directly.
final class AXValueLockedTextField: NSView {
    private let text: String
    init(string: String) {
        text = string
        super.init(frame: NSRect(x: 0, y: 0, width: 120, height: 24))
        wantsLayer = true
        layer?.borderWidth = 1
        layer?.borderColor = NSColor.gray.cgColor
    }
    required init?(coder: NSCoder) { fatalError("init(coder:) is unsupported") }
    override var intrinsicContentSize: NSSize { NSSize(width: 120, height: 24) }
    override func isAccessibilityElement() -> Bool { true }
    override func accessibilityRole() -> NSAccessibility.Role? { .textField }
    override func accessibilityValue() -> Any? { text }
    override func accessibilityPerformPress() -> Bool { true }
    override func isAccessibilitySelectorAllowed(_ selector: Selector) -> Bool {
        if selector == NSSelectorFromString("setAccessibilityValue:") {
            return false
        }
        return super.isAccessibilitySelectorAllowed(selector)
    }
}

final class SingleClickReceiver: NSView {
    let journal: URL

    init(frame: NSRect, journal: URL) {
        self.journal = journal
        super.init(frame: frame)
        wantsLayer = true
        layer?.backgroundColor = NSColor.systemGreen.cgColor
    }

    required init?(coder: NSCoder) { fatalError("init(coder:) is unsupported") }

    override func acceptsFirstMouse(for event: NSEvent?) -> Bool { true }

    func append(_ value: [String: Any]) {
        var data = try! JSONSerialization.data(withJSONObject: value, options: [.sortedKeys])
        data.append(0x0a)
        let file = try! FileHandle(forWritingTo: journal)
        defer { try! file.close() }
        try! file.seekToEnd()
        try! file.write(contentsOf: data)
    }

    func record(_ kind: String, _ event: NSEvent) {
        guard let window else { fatalError("receiver has no window") }
        append([
            "kind": kind,
            "timestamp": event.timestamp,
            "window_id": event.windowNumber,
            "click_count": event.clickCount,
            "x": event.locationInWindow.x,
            "y": window.frame.height - event.locationInWindow.y
        ])
    }

    override func mouseDown(with event: NSEvent) {
        record("down", event)
        // Raw-event focus-theft cases: this view has no AX press, so a click
        // on it must travel as real mouse events.
        if let ms = ProcessInfo.processInfo.environment["CUA_APPKIT_THIEF_DELAY_MS"].flatMap({ Int($0) }) {
            stealFocus(after: ms, window: window)
        }
    }
    override func mouseUp(with event: NSEvent) { record("up", event) }
}

@main
struct CuaAppKitHarness {
    static func main() {
        let app = WindowDiscoveryApplication.shared
        app.setActivationPolicy(.regular)
        if let directory = ProcessInfo.processInfo.environment["CUA_APPKIT_SNAPSHOT_DIR"] {
            let fixture = SnapshotPublicationFixture(directory: URL(fileURLWithPath: directory))
            fixture.show()
            app.activate(ignoringOtherApps: true)
            app.run()
            withExtendedLifetime(fixture) {}
            return
        }
        let controller = HarnessWindowController()
        installMenuBar(target: controller)
        controller.show()
        if ProcessInfo.processInfo.environment["CUA_APPKIT_KEEP_ORDERED_FRONT"] == "1" {
            _ = Timer.scheduledTimer(withTimeInterval: 0.01, repeats: true) { [weak window = controller.window] _ in
                window?.orderFrontRegardless()
            }
        }
        if let path = ProcessInfo.processInfo.environment["CUA_APPKIT_POINTER_ORACLE"] {
            let receiver = SingleClickReceiver(
                frame: controller.window.contentView!.bounds,
                journal: URL(fileURLWithPath: path)
            )
            controller.window.contentView = receiver
            receiver.append([
                "kind": "ready",
                "window_id": controller.window.windowNumber,
                "width": controller.window.frame.width,
                "height": controller.window.frame.height
            ])
        }
        var matrixWindows: BringToFrontMatrixWindows?
        if let mode = ProcessInfo.processInfo.environment["CUA_HARNESS_BRING_TO_FRONT_MODE"] {
            matrixWindows = BringToFrontMatrixWindows(parent: controller.window, mode: mode)
        } else if ProcessInfo.processInfo.environment["CUA_HARNESS_AX_WINDOW_DISCOVERY"] == "listed-sibling" {
            matrixWindows = BringToFrontMatrixWindows(parent: controller.window, mode: "normal")
        }
        app.activate(ignoringOtherApps: true)
        writeBringToFrontWindowReport(main: controller.window, matrix: matrixWindows)
        app.run()
        _ = matrixWindows
    }
}
