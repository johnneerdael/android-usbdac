import AppKit
import Foundation

final class App: NSObject, NSApplicationDelegate {
    let window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 720, height: 520), styleMask: [.titled, .closable, .miniaturizable], backing: .buffered, defer: false)
    let status = NSTextField(wrappingLabelWithString: "Ready to capture diagnostics.")
    let detail = NSTextView()
    let start = NSButton(title: "Start capture", target: nil, action: nil)
    let cancel = NSButton(title: "Cancel", target: nil, action: nil)
    let save = NSButton(title: "Save diagnostics…", target: nil, action: nil)
    let captureDriver = NSButton(checkboxWithTitle: "Allow whole-device USB driver capture (may require administrator authorization)", target: nil, action: nil)
    let sourceBacked = NSButton(checkboxWithTitle: "Include source-backed compatibility modes", target: nil, action: nil)
    var worker: Process?
    var input: Pipe?
    var archive: URL?
    var temporary: URL?
    func applicationDidFinishLaunching(_ notification: Notification) {
        window.title = "USB DAC Validator"
        window.center()
        let warning = NSTextField(wrappingLabelWithString: "Before starting: remove headphones and mute or disconnect downstream amplification. DoP tests can produce loud marker noise on DACs that do not decode it. Keep amplification muted throughout the run.")
        warning.font = NSFont.boldSystemFont(ofSize: 14)
        warning.textColor = .systemRed
        sourceBacked.state = .on
        let scroll = NSScrollView(); scroll.hasVerticalScroller = true; scroll.borderType = .bezelBorder
        detail.isEditable = false; detail.font = .monospacedSystemFont(ofSize: 11, weight: .regular); scroll.documentView = detail
        let buttons = NSStackView(views: [start, cancel, save]); buttons.orientation = .horizontal; buttons.spacing = 12
        let stack = NSStackView(views: [warning, sourceBacked, captureDriver, status, scroll, buttons]); stack.orientation = .vertical; stack.alignment = .leading; stack.spacing = 14; stack.translatesAutoresizingMaskIntoConstraints = false
        window.contentView?.addSubview(stack)
        NSLayoutConstraint.activate([stack.leadingAnchor.constraint(equalTo: window.contentView!.leadingAnchor, constant: 24), stack.trailingAnchor.constraint(equalTo: window.contentView!.trailingAnchor, constant: -24), stack.topAnchor.constraint(equalTo: window.contentView!.topAnchor, constant: 24), stack.bottomAnchor.constraint(equalTo: window.contentView!.bottomAnchor, constant: -24), warning.widthAnchor.constraint(equalTo: stack.widthAnchor), scroll.widthAnchor.constraint(equalTo: stack.widthAnchor), scroll.heightAnchor.constraint(greaterThanOrEqualToConstant: 160)])
        start.target = self; start.action = #selector(begin); cancel.target = self; cancel.action = #selector(stop); save.target = self; save.action = #selector(saveArchive)
        cancel.isEnabled = false; save.isEnabled = false
        window.makeKeyAndOrderFront(nil); NSApp.activate(ignoringOtherApps: true)
    }
    func append(_ text: String) { detail.textStorage?.append(NSAttributedString(string: text + "\n")); detail.scrollToEndOfDocument(nil) }
    // Recover only raw evidence, never an incomplete or nested ZIP.
    func recover(_ dir: URL, reason: String, output: String) -> URL? {
        let fm = FileManager.default
        let staging = fm.temporaryDirectory.appendingPathComponent("usbdac-recovery-" + UUID().uuidString, isDirectory: true)
        let zip = fm.temporaryDirectory.appendingPathComponent("usbdac-recovered-" + UUID().uuidString + ".zip")
        do {
            try fm.createDirectory(at: staging, withIntermediateDirectories: true)
            defer { try? fm.removeItem(at: staging) }
            var warnings: [String] = []
            for name in ["timeline.jsonl", "debug.jsonl", "report.json", "app-start.json"] {
                do { try fm.copyItem(at: dir.appendingPathComponent(name), to: staging.appendingPathComponent(name)) }
                catch { warnings.append("\(name) unavailable: \(error)") }
            }
            try output.write(to: staging.appendingPathComponent("worker-output.txt"), atomically: true, encoding: .utf8)
            let status: [String: Any] = ["partial": true, "reason": reason, "warnings": warnings]
            try JSONSerialization.data(withJSONObject: status, options: [.prettyPrinted, .sortedKeys]).write(to: staging.appendingPathComponent("capture-status.json"), options: .atomic)
            let zipper = Process(); zipper.executableURL = URL(fileURLWithPath: "/usr/bin/ditto"); zipper.arguments = ["-c", "-k", staging.path, zip.path]
            try zipper.run(); zipper.waitUntilExit()
            guard zipper.terminationStatus == 0 else { try? fm.removeItem(at: zip); return nil }
            return zip
        } catch { try? fm.removeItem(at: zip); return nil }
    }
    func offerRecovery(_ dir: URL, reason: String, output: String) {
        if let recovered = recover(dir, reason: reason, output: output) { archive = recovered; save.isEnabled = true }
        status.stringValue = reason
    }
    @objc func begin() {
        guard worker == nil else { return }
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("usbdac-validator-" + UUID().uuidString, isDirectory: true)
        do { try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true) } catch { status.stringValue = "Cannot create capture: \(error)"; return }
        temporary = dir; archive = nil; save.isEnabled = false; detail.string = ""
        let startEvidence: [String: Any] = ["started_unix_seconds": Date().timeIntervalSince1970, "source_backed": sourceBacked.state == .on, "allow_device_capture": captureDriver.state == .on]
        if let data = try? JSONSerialization.data(withJSONObject: startEvidence) { try? data.write(to: dir.appendingPathComponent("app-start.json"), options: .atomic) }
        guard let executable = Bundle.main.url(forAuxiliaryExecutable: "usbdac-validator-worker") else { offerRecovery(dir, reason: "Worker is missing from the app bundle.", output: ""); return }
        let process = Process(); let output = Pipe(); let control = Pipe()
        let useSourceBacked = sourceBacked.state == .on
        process.executableURL = executable; process.arguments = [dir.path] + (captureDriver.state == .on ? ["--allow-capture"] : []) + (useSourceBacked ? [] : ["--verified-only"])
        append("Compatibility policy: " + (useSourceBacked ? "source-backed" : "verified only"))
        process.standardOutput = output; process.standardError = output; process.standardInput = control
        worker = process; input = control; start.isEnabled = false; cancel.isEnabled = true; captureDriver.isEnabled = false; sourceBacked.isEnabled = false
        do { try process.run() } catch { worker = nil; input = nil; start.isEnabled = true; cancel.isEnabled = false; captureDriver.isEnabled = true; sourceBacked.isEnabled = true; offerRecovery(dir, reason: "Could not start worker: \(error)", output: ""); return }
        DispatchQueue.global(qos: .userInitiated).async { [weak self] in
            var buffered = Data(); var workerOutput = ""; var finishedPath: String?
            while true { let data = output.fileHandleForReading.availableData; if data.isEmpty { break }; buffered.append(data)
                while let index = buffered.firstIndex(of: 10) {
                    let line = buffered.prefix(upTo: index); buffered.removeSubrange(...index)
                    guard let text = String(data: line, encoding: .utf8) else { continue }
                    if workerOutput.utf8.count < 1_000_000 { workerOutput += text + "\n" }
                    if let bytes = text.data(using: .utf8), let event = (try? JSONSerialization.jsonObject(with: bytes)) as? [String: Any], event["type"] as? String == "finished" { finishedPath = event["path"] as? String }
                    DispatchQueue.main.async { self?.handle(text) }
                }
            }
            process.waitUntilExit()
            let expected = dir.appendingPathComponent("diagnostics.zip")
            let complete = process.terminationStatus == 0 && finishedPath == expected.path && FileManager.default.fileExists(atPath: expected.path)
            let reason = "Worker exited (\(process.terminationStatus)); recovered diagnostics may be incomplete."
            let recovered = complete ? nil : self?.recover(dir, reason: reason, output: workerOutput)
            DispatchQueue.main.async {
                guard let self = self, self.worker === process else { return }
                self.worker = nil; self.input = nil; self.start.isEnabled = true; self.cancel.isEnabled = false; self.captureDriver.isEnabled = true; self.sourceBacked.isEnabled = true
                if complete { self.archive = expected; self.save.isEnabled = true }
                else { if let recovered = recovered { self.archive = recovered; self.save.isEnabled = true }; self.status.stringValue = reason }
            }
        }
        status.stringValue = "Preparing USB baseline…"
    }
    func handle(_ text: String) {
        guard let data = text.data(using: .utf8), let json = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any] else { append(text); return }
        let type = json["type"] as? String ?? ""
        if let message = json["message"] as? String { status.stringValue = message; append(message) }
        if type == "progress", let p = json["progress"] as? [String: Any] { let index = p["index"] as? Int ?? 0; let total = p["total"] as? Int ?? 0; let leg = p["leg"] as? String ?? ""; if (p["phase"] as? String) != "finished" { let phase = p["phase"] as? String; let action = phase == "probing" ? "Checking exact admission" : (phase == "fallback" ? "Testing converted fallback" : "Testing exact format"); status.stringValue = "\(action) \(min(index + 1, total)) of \(total)"; append("\(action): \(leg)") } }
    }
    @objc func stop() { guard let input = input else { return }; try? input.fileHandleForWriting.write(contentsOf: Data("cancel\n".utf8)); cancel.isEnabled = false; status.stringValue = "Stopping and saving diagnostics…" }
    @objc func saveArchive() { guard let archive = archive else { return }; let panel = NSSavePanel(); panel.nameFieldStringValue = "usbdac-diagnostics.zip"; panel.allowedContentTypes = [.zip]; panel.beginSheetModal(for: window) { result in guard result == .OK, let destination = panel.url else { return }; do { let staging = destination.deletingLastPathComponent().appendingPathComponent("." + UUID().uuidString + ".zip"); defer { try? FileManager.default.removeItem(at: staging) }; try FileManager.default.copyItem(at: archive, to: staging); if FileManager.default.fileExists(atPath: destination.path) { _ = try FileManager.default.replaceItemAt(destination, withItemAt: staging) } else { try FileManager.default.moveItem(at: staging, to: destination) }; self.status.stringValue = "Diagnostics saved." } catch { self.status.stringValue = "Could not save diagnostics: \(error)" } } }
    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool { true }
    func applicationShouldTerminate(_ sender: NSApplication) -> NSApplication.TerminateReply { if worker != nil { stop(); return .terminateCancel }; return .terminateNow }
}
let app = NSApplication.shared
let delegate = App(); app.delegate = delegate; app.setActivationPolicy(.regular); app.run()
