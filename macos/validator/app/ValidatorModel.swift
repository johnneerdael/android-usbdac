import AppKit
import Darwin
import Foundation
import UniformTypeIdentifiers

// Owns the authorization broker, the private worker channel and the evidence
// archive. The interface only observes published state and calls begin, stop
// and saveArchive. Published state is mutated on the main thread only.
final class ValidatorModel: ObservableObject {
    // Run states the app can determine from its own bookkeeping. None of them
    // is a verdict about the DAC; that evidence lives in the archive.
    enum Phase: Equatable {
        case ready, awaitingApproval, running, stopping, archiveReady, saved, partial, cancelled, failed
    }

    @Published private(set) var phase: Phase = .ready
    @Published private(set) var statusText = "Ready to capture diagnostics."
    @Published private(set) var progress: Double?
    @Published var sourceBacked = true
    @Published private(set) var canStart = true
    @Published private(set) var canCancel = false
    @Published private(set) var canSave = false
    @Published private(set) var canChangePolicy = true
    @Published private(set) var log = ""
    @Published private(set) var logGeneration = 0
    // Whether the saveable archive came from a complete transfer or from
    // recovery of raw evidence; recovered archives may be incomplete.
    enum ArchiveKind { case complete, recovered }
    @Published private(set) var archiveKind: ArchiveKind?

    var worker: Process?
    var input: FileHandle?
    let stateLock = NSLock()
    var cancelled = false
    var archive: URL?
    var temporary: URL?


    func append(_ text: String) { log += text + "\n" }
    private func clearLog() { log = ""; logGeneration += 1 }

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
            if let executable = Bundle.main.url(forAuxiliaryExecutable: "usbdac-validator-worker") {
                let renderer = Process(); renderer.executableURL = executable; renderer.arguments = ["--render-report", staging.path]
                renderer.standardOutput = FileHandle.nullDevice; renderer.standardError = FileHandle.nullDevice
                do { try renderer.run(); renderer.waitUntilExit(); if renderer.terminationStatus != 0 { try "HTML report could not be generated. Raw evidence is retained.\n".write(to: staging.appendingPathComponent("HTML_REPORT_UNAVAILABLE.txt"), atomically: true, encoding: .utf8) } }
                catch { try? "HTML report could not be generated. Raw evidence is retained.\n".write(to: staging.appendingPathComponent("HTML_REPORT_UNAVAILABLE.txt"), atomically: true, encoding: .utf8) }
            } else { try? "HTML report worker is unavailable. Raw evidence is retained.\n".write(to: staging.appendingPathComponent("HTML_REPORT_UNAVAILABLE.txt"), atomically: true, encoding: .utf8) }
            let zipper = Process(); zipper.executableURL = URL(fileURLWithPath: "/usr/bin/ditto"); zipper.arguments = ["-c", "-k", staging.path, zip.path]
            try zipper.run(); zipper.waitUntilExit()
            guard zipper.terminationStatus == 0 else { try? fm.removeItem(at: zip); return nil }
            return zip
        } catch { try? fm.removeItem(at: zip); return nil }
    }

    func offerRecovery(_ dir: URL, reason: String, output: String) {
        if let recovered = recover(dir, reason: reason, output: output) { archive = recovered; archiveKind = .recovered; canSave = true; phase = .partial }
        else { phase = .failed }
        statusText = reason
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

    func begin() {
        guard worker == nil else { return }
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("usbdac-validator-" + UUID().uuidString, isDirectory: true)
        do { try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: false, attributes: [.posixPermissions: 0o700]) } catch { statusText = "Cannot create capture: \(error)"; phase = .failed; return }
        temporary = dir; archive = nil; archiveKind = nil; canSave = false; clearLog(); progress = nil
        let useSourceBacked = sourceBacked
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
        worker = process; canStart = false; canCancel = true; canChangePolicy = false
        append("Compatibility policy: " + (useSourceBacked ? "source-backed" : "verified only"))
        statusText = "Approve administrator access to start diagnostics…"; phase = .awaitingApproval
        do { try process.run() } catch {
            Darwin.close(listener); try? FileManager.default.removeItem(at: socketDir)
            worker = nil; canStart = true; canCancel = false; canChangePolicy = true
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
            let wasCancelled = self.isCancelled()
            DispatchQueue.main.async {
                guard self.worker === process else { return }
                self.worker = nil; self.canStart = true; self.canCancel = false; self.canChangePolicy = true
                self.progress = nil
                if complete { self.archive = target; self.archiveKind = .complete; self.canSave = true; self.phase = wasCancelled ? .cancelled : .archiveReady }
                else {
                    try? FileManager.default.removeItem(at: target)
                    if let recovered = recovered { self.archive = recovered; self.archiveKind = .recovered; self.canSave = true }
                    self.statusText = reason
                    self.phase = wasCancelled ? .cancelled : (recovered != nil ? .partial : .failed)
                }
            }
        }
    }

    func handle(_ text: String) {
        // Any worker event means approval was granted and the worker connected.
        if phase == .awaitingApproval { phase = .running }
        guard let data = text.data(using: .utf8), let json = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any] else { append(text); return }
        let type = json["type"] as? String ?? ""
        if let message = json["message"] as? String { statusText = message; append(message) }
        if type == "progress", let p = json["progress"] as? [String: Any] {
            let index = p["index"] as? Int ?? 0; let total = p["total"] as? Int ?? 0; let leg = p["leg"] as? String ?? ""
            if (p["phase"] as? String) != "finished" {
                let phase = p["phase"] as? String
                let action = phase == "probing" ? "Checking exact admission" : (phase == "fallback" ? "Testing converted fallback" : "Testing exact format")
                statusText = "\(action) \(min(index + 1, total)) of \(total)"; append("\(action): \(leg)")
                progress = total > 0 ? Double(min(index + 1, total)) / Double(total) : nil
            }
        }
    }

    func stop() {
        stateLock.lock(); cancelled = true; let channel = input; if let channel = channel { try? channel.write(contentsOf: Data("cancel\n".utf8)) }; stateLock.unlock()
        if channel == nil, let worker = worker, worker.isRunning { worker.terminate() }
        canCancel = false; statusText = "Stopping and saving diagnostics…"; phase = .stopping
    }

    func saveArchive(from window: NSWindow) {
        guard let archive = archive else { return }
        let panel = NSSavePanel(); panel.nameFieldStringValue = "usbdac-diagnostics.zip"; panel.allowedContentTypes = [.zip]
        panel.beginSheetModal(for: window) { result in
            guard result == .OK, let destination = panel.url else { return }
            do {
                let staging = destination.deletingLastPathComponent().appendingPathComponent("." + UUID().uuidString + ".zip")
                defer { try? FileManager.default.removeItem(at: staging) }
                try FileManager.default.copyItem(at: archive, to: staging)
                if FileManager.default.fileExists(atPath: destination.path) { _ = try FileManager.default.replaceItemAt(destination, withItemAt: staging) } else { try FileManager.default.moveItem(at: staging, to: destination) }
                self.statusText = "Diagnostics saved."; self.phase = .saved
            } catch { self.statusText = "Could not save diagnostics: \(error)" }
        }
    }
}
