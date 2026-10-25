// Dumps the accessibility tree of a running app (what VoiceOver reads) and
// lists interactive elements without a spoken name.
//
//   swift scripts/ax-audit.swift <pid> [max-depth]
//
// The calling terminal needs Accessibility permission (System Settings →
// Privacy & Security → Accessibility).

import ApplicationServices
import Foundation

guard CommandLine.arguments.count > 1, let pid = pid_t(CommandLine.arguments[1]) else {
    print("usage: ax-audit <pid> [max-depth]")
    exit(2)
}
let maxDepth = CommandLine.arguments.count > 2 ? Int(CommandLine.arguments[2]) ?? 40 : 40
guard AXIsProcessTrusted() else {
    print("this process is not trusted for accessibility")
    exit(2)
}

func attr(_ e: AXUIElement, _ name: String) -> AnyObject? {
    var v: AnyObject?
    return AXUIElementCopyAttributeValue(e, name as CFString, &v) == .success ? v : nil
}

func str(_ e: AXUIElement, _ name: String) -> String {
    guard let v = attr(e, name) else { return "" }
    if let s = v as? String { return s }
    if let n = v as? NSNumber { return n.stringValue }
    return ""
}

let interactive: Set<String> = [
    "AXButton", "AXCheckBox", "AXRadioButton", "AXPopUpButton", "AXMenuButton", "AXTextField", "AXTextArea",
    "AXSlider", "AXIncrementor", "AXLink", "AXTab", "AXComboBox", "AXDisclosureTriangle",
]
var total = 0
var unnamed: [String] = []
var counts: [String: Int] = [:]

func walk(_ e: AXUIElement, _ depth: Int, _ path: String) {
    guard depth <= maxDepth else { return }
    let role = str(e, kAXRoleAttribute)
    // The menu bar is the system's (it also lists recent documents).
    if role == "AXMenuBar" { return }
    let title = str(e, kAXTitleAttribute)
    let desc = str(e, kAXDescriptionAttribute)
    let value = str(e, kAXValueAttribute)
    let help = str(e, kAXHelpAttribute)
    total += 1
    counts[role, default: 0] += 1
    let name = [title, desc].first { !$0.trimmingCharacters(in: .whitespaces).isEmpty } ?? ""
    let indent = String(repeating: "  ", count: depth)
    let shownValue = value.count > 40 ? String(value.prefix(40)) + "…" : value
    print("\(indent)\(role) \"\(name)\"\(shownValue.isEmpty ? "" : " value=\"\(shownValue)\"")\(help.isEmpty ? "" : " help=\"\(help)\"")")
    // Window controls (close/minimize/zoom) are named by their subrole.
    let subrole = str(e, kAXSubroleAttribute)
    if interactive.contains(role) && name.isEmpty && help.isEmpty && !subrole.hasSuffix("Button") {
        unnamed.append("\(path) > \(role)\(shownValue.isEmpty ? "" : " value=\"\(shownValue)\"")")
    }
    let children = (attr(e, kAXChildrenAttribute) as? [AXUIElement]) ?? []
    for c in children {
        walk(c, depth + 1, name.isEmpty ? path : "\(path) > \(name)")
    }
}

func touch(_ e: AXUIElement, _ depth: Int) {
    guard depth < 6 else { return }
    for c in (attr(e, kAXChildrenAttribute) as? [AXUIElement]) ?? [] { touch(c, depth + 1) }
}

let app = AXUIElementCreateApplication(pid)
// AccessKit builds its tree after the first request reaches the window's
// content view: touch it, give the app a few frames, then walk.
touch(app, 0)
Thread.sleep(forTimeInterval: 1.5)
total = 0
counts = [:]
walk(app, 0, "app")
print("\n\(total) elements: " + counts.sorted { $0.value > $1.value }.map { "\($0.key)=\($0.value)" }.joined(separator: ", "))
print("\(unnamed.count) interactive elements without a name:")
for u in unnamed { print("  " + u) }
