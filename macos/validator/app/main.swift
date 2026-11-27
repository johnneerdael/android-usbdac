import AppKit
import Foundation
import Darwin

final class App: NSObject, NSApplicationDelegate {
    let window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 720, height: 520), styleMask: [.titled, .closable, .miniaturizable], backing: .buffered, defer: false)
    let status = NSTextField(wrappingLabelWithString: "Ready to capture diagnostics.")
    let detail = NSTextView()
    let start = NSButton(title: "Start capture", target: nil, action: nil)
    let cancel = NSButton(title: "Cancel", target: nil, action: nil)
    let save = NSButton(title: "Save diagnostics…", target: nil, action: nil)
    let sourceBacked = NSButton(checkboxWithTitle: "Include source-backed compatibility modes", target: nil, action: nil)
    var worker: Process?
    var input: FileHandle?
    let stateLock = NSLock()
    var cancelled = false
    var archive: URL?
    var temporary: URL?
    func applicationDidFinishLaunching(_ notification: Notification) {
        window.title = "USB DAC Validator"
        window.center()
        let warning = NSTextField(wrappingLabelWithString: "Before starting: remove headphones and mute or disconnect downstream amplification. DoP tests can produce loud marker noise on DACs that do not decode it. Keep amplification muted throughout the run.")
        warning.font = NSFont.boldSystemFont(ofSize: 14)
        warning.textColor = .systemRed
        sourceBacked.state = .on
        let takeover = NSTextField(wrappingLabelWithString: "If the ordinary USB claim is denied or busy, the worker will attempt to take over the entire selected USB device, including its other interfaces. macOS administrator approval is required.")
        let scroll = NSScrollView(); scroll.hasVerticalScroller = true; scroll.borderType = .bezelBorder
        detail.isEditable = false; detail.font = .monospacedSystemFont(ofSize: 11, weight: .regular); scroll.documentView = detail
        let buttons = NSStackView(views: [start, cancel, save]); buttons.orientation = .horizontal; buttons.spacing = 12
        let stack = NSStackView(views: [warning, takeover, sourceBacked, status, scroll, buttons]); stack.orientation = .vertical; stack.alignment = .leading; stack.spacing = 14; stack.translatesAutoresizingMaskIntoConstraints = false
        window.contentView?.addSubview(stack)
        NSLayoutConstraint.activate([stack.leadingAnchor.constraint(equalTo: window.contentView!.leadingAnchor, constant: 24), stack.trailingAnchor.constraint(equalTo: window.contentView!.trailingAnchor, constant: -24), stack.topAnchor.constraint(equalTo: window.contentView!.topAnchor, constant: 24), stack.bottomAnchor.constraint(equalTo: window.contentView!.bottomAnchor, constant: -24), warning.widthAnchor.constraint(equalTo: stack.widthAnchor), takeover.widthAnchor.constraint(equalTo: stack.widthAnchor), scroll.widthAnchor.constraint(equalTo: stack.widthAnchor), scroll.heightAnchor.constraint(greaterThanOrEqualToConstant: 160)])
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
    // The directory is owned by the GUI user; only the root worker can be
    // accepted after getpeereid. It is deliberately short for sockaddr_un.
    func makeListener() throws -> (Int32, URL, String) {
        let directory = URL(fileURLWithPath: "/private/tmp", isDirectory: true).appendingPathComponent("ud-" + UUID().uuidString, isDirectory: true)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: false, attributes: [.posixPermissions: 0o700])
        let path = directory.appendingPathComponent("s").path
        let fd = Darwin.socket(AF_UNIX, SOCK_STREAM, 0)
        guard fd >= 0 else { try? FileManager.default.removeItem(at: directory); throw NSError(domain: NSPOSIXErrorDomain, code: Int(errno)) }
        var address = sockaddr_un(); address.sun_family = sa_family_t(AF_UNIX)
        let bytes = Array(path.utf8) + [0]
        guard bytes.count <= MemoryLayout.size(ofValue: address.sun_path) else { Darwin.close(fd); try? FileManager.default.removeItem(at: directory); throw NSError(domain: NSPOSIXErrorDomain, code: Int(ENAMETOOLONG)) }
        withUnsafeMutableBytes(of: &address.sun_path) { target in target.copyBytes(from: bytes) }
        address.sun_len = UInt8(MemoryLayout<sockaddr_un>.size)
        let bound = withUnsafePointer(to: &address) { $0.withMemoryRebound(to: sockaddr.self, capacity: 1) { Darwin.bind(fd, $0, socklen_t(MemoryLayout<sockaddr_un>.size)) } }
        guard bound == 0, Darwin.listen(fd, 1) == 0 else { let error = errno; Darwin.close(fd); try? FileManager.default.removeItem(at: directory); throw NSError(domain: NSPOSIXErrorDomain, code: Int(error)) }
        return (fd, directory, path)
    }
    func isCancelled() -> Bool { stateLock.lock(); defer { stateLock.unlock() }; return cancelled }
    @objc func begin() {
        guard worker == nil else { return }
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("usbdac-validator-" + UUID().uuidString, isDirectory: true)
        do { try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: false, attributes: [.posixPermissions: 0o700]) } catch { status.stringValue = "Cannot create capture: \(error)"; return }
        temporary = dir; archive = nil; save.isEnabled = false; detail.string = ""
        let useSourceBacked = sourceBacked.state == .on
        let startEvidence: [String: Any] = ["started_unix_seconds": Date().timeIntervalSince1970, "source_backed": useSourceBacked, "allow_device_capture": true, "authorization": "administrator approval required"]
        if let data = try? JSONSerialization.data(withJSONObject: startEvidence) { try? data.write(to: dir.appendingPathComponent("app-start.json"), options: .atomic) }
        guard let executable = Bundle.main.url(forAuxiliaryExecutable: "usbdac-validator-worker") else { offerRecovery(dir, reason: "Worker is missing from the app bundle.", output: ""); return }
        guard getuid() != 0 else { offerRecovery(dir, reason: "Start the app as a regular user. It requests administrator approval for its worker.", output: ""); return }
        let listener: Int32, socketDir: URL, socketPath: String
        do { (listener, socketDir, socketPath) = try makeListener() } catch { offerRecovery(dir, reason: "Cannot create local worker channel: \(error)", output: ""); return }
        let script = """
        on run argv
          set commandText to quoted form of item 1 of argv & " --privileged-socket " & quoted form of item 2 of argv & " " & quoted form of item 3 of argv
          if (count of argv) > 3 then
            repeat with argValue in items 4 thru -1 of argv
              set commandText to commandText & " " & quoted form of (contents of argValue)
            end repeat
          end if
          with timeout of 3600 seconds
            do shell script commandText with administrator privileges
          end timeout
        end run
        """
        let process = Process(); let output = Pipe()
        process.executableURL = URL(fileURLWithPath: "/usr/bin/osascript")
        process.arguments = ["-e", script, "--", executable.path, socketPath, String(getuid()), "--allow-capture"] + (useSourceBacked ? [] : ["--verified-only"])
        process.standardOutput = output; process.standardError = output
        stateLock.lock(); cancelled = false; stateLock.unlock()
        worker = process; start.isEnabled = false; cancel.isEnabled = true; sourceBacked.isEnabled = false
        append("Compatibility policy: " + (useSourceBacked ? "source-backed" : "verified only"))
        status.stringValue = "Approve administrator access to start diagnostics…"
        do { try process.run() } catch {
            Darwin.close(listener); try? FileManager.default.removeItem(at: socketDir)
            worker = nil; start.isEnabled = true; cancel.isEnabled = false; sourceBacked.isEnabled = true
            offerRecovery(dir, reason: "Could not request administrator approval: \(error)", output: ""); return
        }
        DispatchQueue.global(qos: .userInitiated).async { [weak self] in
            guard let self = self else { Darwin.close(listener); return }
            defer { Darwin.close(listener); try? FileManager.default.removeItem(at: socketDir) }
            var client: Int32 = -1
            let deadline = Date().addingTimeInterval(180)
            while process.isRunning && !self.isCancelled() && Date() < deadline {
                var wait = pollfd(fd: listener, events: Int16(POLLIN), revents: 0)
                if Darwin.poll(&wait, 1, 500) > 0 && (wait.revents & Int16(POLLIN)) != 0 { client = Darwin.accept(listener, nil, nil); break }
            }
            var validPeer = false
            if client >= 0 {
                var uid: uid_t = 1; var gid: gid_t = 0
                var noSigpipe: Int32 = 1
                validPeer = getpeereid(client, &uid, &gid) == 0 && uid == 0 && setsockopt(client, SOL_SOCKET, SO_NOSIGPIPE, &noSigpipe, socklen_t(MemoryLayout<Int32>.size)) == 0
                if !validPeer { Darwin.close(client); client = -1 }
            }
            var finished = false; var archiveEnded = false; var expected: Int64 = -1; var received: Int64 = 0
            var workerOutput = ""; let target = dir.appendingPathComponent("diagnostics.zip")
            var writer: FileHandle?
            if client >= 0 && validPeer {
                let channel = FileHandle(fileDescriptor: client, closeOnDealloc: true)
                self.stateLock.lock(); self.input = channel; let alreadyCancelled = self.cancelled; self.stateLock.unlock()
                if alreadyCancelled { try? channel.write(contentsOf: Data("cancel\n".utf8)) }
                var buffered = Data(); var protocolFailed = false; var cancelDeadline: Date?
                while !protocolFailed {
                    if self.isCancelled() && cancelDeadline == nil { cancelDeadline = Date().addingTimeInterval(15) }
                    if let limit = cancelDeadline, Date() >= limit { workerOutput += "Worker did not stop within the cancellation deadline.\n"; break }
                    var wait = pollfd(fd: client, events: Int16(POLLIN), revents: 0)
                    let ready = Darwin.poll(&wait, 1, 500)
                    if ready == 0 { continue }
                    if ready < 0 { break }
                    var bytes = [UInt8](repeating: 0, count: 65536)
                    let count = Darwin.read(client, &bytes, bytes.count)
                    if count <= 0 { break }
                    buffered.append(contentsOf: bytes.prefix(count))
                    while let index = buffered.firstIndex(of: 10) {
                        let line = Data(buffered.prefix(upTo: index)); buffered.removeSubrange(...index)
                        guard let event = (try? JSONSerialization.jsonObject(with: line)) as? [String: Any], let type = event["type"] as? String else { continue }
                        if type == "archive_start" {
                            expected = (event["bytes"] as? NSNumber)?.int64Value ?? -1
                            if expected < 0 || expected > 64 * 1024 * 1024 || writer != nil || archiveEnded { protocolFailed = true; break }
                            _ = FileManager.default.createFile(atPath: target.path, contents: nil, attributes: [.posixPermissions: 0o600])
                            writer = try? FileHandle(forWritingTo: target)
                            if writer == nil { protocolFailed = true; break }
                        } else if type == "archive_chunk" {
                            guard let encoded = event["data"] as? String, let bytes = Data(base64Encoded: encoded), bytes.count <= 24576, let writer = writer, received + Int64(bytes.count) <= expected else { protocolFailed = true; break }
                            do { try writer.write(contentsOf: bytes); received += Int64(bytes.count) } catch { protocolFailed = true; break }
                        } else if type == "archive_end" {
                            archiveEnded = writer != nil && expected >= 0 && received == expected && (event["bytes"] as? NSNumber)?.int64Value == expected
                            if !archiveEnded { protocolFailed = true; break }
                            try? writer?.close(); writer = nil
                        } else {
                            if workerOutput.utf8.count < 1_000_000, let text = String(data: line, encoding: .utf8) { workerOutput += text + "\n" }
                            if type == "finished" { finished = archiveEnded }
                            if let text = String(data: line, encoding: .utf8) { DispatchQueue.main.async { self.handle(text) } }
                        }
                    }
                    if buffered.count > 131072 { protocolFailed = true }
                }
                if protocolFailed { finished = false; workerOutput += "Invalid or incomplete archive transfer.\n" }
                try? writer?.close()
                self.stateLock.lock(); self.input = nil; self.stateLock.unlock()
                try? channel.close()
            }
            if process.isRunning && (self.isCancelled() || client < 0) { process.terminate() }
            let exitDeadline = Date().addingTimeInterval(5)
            while process.isRunning && Date() < exitDeadline { Thread.sleep(forTimeInterval: 0.05) }
            if process.isRunning { process.terminate() }
            let terminateDeadline = Date().addingTimeInterval(1)
            while process.isRunning && Date() < terminateDeadline { Thread.sleep(forTimeInterval: 0.05) }
            let exited = !process.isRunning
            let scriptOutput = exited ? (String(data: output.fileHandleForReading.readDataToEndOfFile(), encoding: .utf8) ?? "") : "Authorization broker did not exit promptly."
            let complete = finished && archiveEnded && exited && process.terminationStatus == 0
            let reason = self.isCancelled() ? "Cancelled during authorization or worker startup." : (scriptOutput.contains("-128") ? "Administrator approval was denied or cancelled." : (Date() >= deadline && client < 0 ? "Administrator approval timed out." : "Administrator approval or worker failed. Recovered diagnostics may be incomplete."))
            let recovered = complete ? nil : self.recover(dir, reason: reason, output: workerOutput + scriptOutput)
            DispatchQueue.main.async {
                guard self.worker === process else { return }
                self.worker = nil; self.start.isEnabled = true; self.cancel.isEnabled = false; self.sourceBacked.isEnabled = true
                if complete { self.archive = target; self.save.isEnabled = true }
                else { try? FileManager.default.removeItem(at: target); if let recovered = recovered { self.archive = recovered; self.save.isEnabled = true }; self.status.stringValue = reason }
            }
        }
    }
    func handle(_ text: String) {
        guard let data = text.data(using: .utf8), let json = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any] else { append(text); return }
        let type = json["type"] as? String ?? ""
        if let message = json["message"] as? String { status.stringValue = message; append(message) }
        if type == "progress", let p = json["progress"] as? [String: Any] { let index = p["index"] as? Int ?? 0; let total = p["total"] as? Int ?? 0; let leg = p["leg"] as? String ?? ""; if (p["phase"] as? String) != "finished" { let phase = p["phase"] as? String; let action = phase == "probing" ? "Checking exact admission" : (phase == "fallback" ? "Testing converted fallback" : "Testing exact format"); status.stringValue = "\(action) \(min(index + 1, total)) of \(total)"; append("\(action): \(leg)") } }
    }
    @objc func stop() { stateLock.lock(); cancelled = true; let channel = input; if let channel = channel { try? channel.write(contentsOf: Data("cancel\n".utf8)) }; stateLock.unlock(); if channel == nil, let worker = worker, worker.isRunning { worker.terminate() }; cancel.isEnabled = false; status.stringValue = "Stopping and saving diagnostics…" }
    @objc func saveArchive() { guard let archive = archive else { return }; let panel = NSSavePanel(); panel.nameFieldStringValue = "usbdac-diagnostics.zip"; panel.allowedContentTypes = [.zip]; panel.beginSheetModal(for: window) { result in guard result == .OK, let destination = panel.url else { return }; do { let staging = destination.deletingLastPathComponent().appendingPathComponent("." + UUID().uuidString + ".zip"); defer { try? FileManager.default.removeItem(at: staging) }; try FileManager.default.copyItem(at: archive, to: staging); if FileManager.default.fileExists(atPath: destination.path) { _ = try FileManager.default.replaceItemAt(destination, withItemAt: staging) } else { try FileManager.default.moveItem(at: staging, to: destination) }; self.status.stringValue = "Diagnostics saved." } catch { self.status.stringValue = "Could not save diagnostics: \(error)" } } }
    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool { true }
    func applicationShouldTerminate(_ sender: NSApplication) -> NSApplication.TerminateReply { if worker != nil { stop(); return .terminateCancel }; return .terminateNow }
}
let app = NSApplication.shared
let delegate = App(); app.delegate = delegate; app.setActivationPolicy(.regular); app.run()
