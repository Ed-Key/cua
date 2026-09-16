import AppKit

// Opt-in input/readback diagnostic. A key-only control makes the selected
// input route explicit: AX string setters are not implemented. No timers or
// external input drive transitions; the first received key triggers them.
final class IdentityTextField: NSView {
    var value: String = "" { didSet { needsDisplay = true } }
    var onKey: ((IdentityTextField, String) -> Void)?
    override var acceptsFirstResponder: Bool { true }
    override func isAccessibilityElement() -> Bool { true }
    override func accessibilityRole() -> NSAccessibility.Role? { .textField }
    override func accessibilityValue() -> Any? { value }
    override func isAccessibilityFocused() -> Bool { window?.firstResponder === self }
    override func setAccessibilityFocused(_ focused: Bool) {
        if focused { window?.makeFirstResponder(self) }
    }
    override func accessibilityPerformPress() -> Bool {
        window?.makeFirstResponder(self) ?? false
    }
    override func isAccessibilitySelectorAllowed(_ selector: Selector) -> Bool {
        if ["setAccessibilityValue:", "setAccessibilitySelectedText:"].contains(NSStringFromSelector(selector)) {
            return false
        }
        return super.isAccessibilitySelectorAllowed(selector)
    }
    override func mouseDown(with event: NSEvent) { window?.makeFirstResponder(self) }
    override func keyDown(with event: NSEvent) {
        guard let text = event.characters, !text.isEmpty else { return }
        onKey?(self, text)
    }
    override func draw(_ dirtyRect: NSRect) {
        NSColor.textBackgroundColor.setFill()
        bounds.fill()
        NSColor.separatorColor.setStroke()
        NSBezierPath(rect: bounds.insetBy(dx: 0.5, dy: 0.5)).stroke()
        (value as NSString).draw(at: NSPoint(x: 5, y: 7), withAttributes: [
            .font: NSFont.monospacedSystemFont(ofSize: 13, weight: .regular),
            .foregroundColor: NSColor.textColor
        ])
    }
}

final class EditorIdentityFixture {
    let row = NSStackView()
    private var target = IdentityTextField()
    private let other = IdentityTextField()
    private var retired: [IdentityTextField] = []
    private let mode: String
    private let trace: String
    private let keyMode: String
    private var keys = 0
    private var transitions = 0

    init?(environment: [String: String]) {
        guard let mode = environment["CUA_APPKIT_EDITOR_TRANSITION"],
              ["stable", "replace", "divert"].contains(mode),
              let trace = environment["CUA_APPKIT_EDITOR_TRACE"] else { return nil }
        self.mode = mode
        self.trace = trace
        let keyMode = environment["CUA_APPKIT_EDITOR_KEYS"] ?? "all"
        guard ["all", "drop", "first"].contains(keyMode) else { return nil }
        self.keyMode = keyMode
        row.orientation = .horizontal
        row.spacing = 12
        configure(target, id: "txt-transition-target", label: "Transition target")
        configure(other, id: "txt-transition-other", label: "Other field")
        target.value = environment["CUA_APPKIT_EDITOR_INITIAL"] ?? ""
        other.value = "hello"
        row.addArrangedSubview(target)
        row.addArrangedSubview(other)
        record("initial", key: "")
    }

    private func configure(_ field: IdentityTextField, id: String, label: String) {
        field.setAccessibilityIdentifier(id)
        field.setAccessibilityLabel(label)
        field.translatesAutoresizingMaskIntoConstraints = false
        NSLayoutConstraint.activate([
            field.widthAnchor.constraint(equalToConstant: 240),
            field.heightAnchor.constraint(equalToConstant: 32)
        ])
        field.onKey = { [weak self] field, key in self?.receive(field, key: key) }
    }

    private func receive(_ field: IdentityTextField, key: String) {
        keys += key.count
        if field === target && transitions == 0 && mode == "replace" {
            let old = target
            let replacement = IdentityTextField()
            configure(replacement, id: "txt-transition-target", label: "Transition target")
            replacement.value = old.value + key
            row.removeArrangedSubview(old)
            old.removeFromSuperview()
            row.insertArrangedSubview(replacement, at: 0)
            target = replacement
            retired.append(old)
            transitions += 1
            row.window?.makeFirstResponder(replacement)
            NSAccessibility.post(element: row, notification: .layoutChanged)
        } else if field === target && transitions == 0 && mode == "divert" {
            // The attempted edit did not land. Focus changes to a different,
            // already populated field, which deliberately ignores later keys.
            transitions += 1
            row.window?.makeFirstResponder(other)
        } else if field === target && (keyMode == "all" || (keyMode == "first" && keys == 1)) {
            target.value += key
        }
        NSAccessibility.post(element: target, notification: .valueChanged)
        record("key", key: key)
    }

    private func record(_ phase: String, key: String) {
        let focused = row.window?.firstResponder === target ? "target"
            : row.window?.firstResponder === other ? "other" : "none"
        let state: [String: Any] = [
            "phase": phase, "mode": mode, "keyMode": keyMode, "key": key, "keys": keys,
            "transitions": transitions, "target": target.value,
            "other": other.value, "focused": focused,
            "retired": retired.map { $0.value }
        ]
        let line = try! JSONSerialization.data(withJSONObject: state, options: [.sortedKeys]) + Data([10])
        if let handle = FileHandle(forWritingAtPath: trace) {
            handle.seekToEndOfFile()
            handle.write(line)
            handle.closeFile()
        } else {
            try! line.write(to: URL(fileURLWithPath: trace))
        }
    }
}
