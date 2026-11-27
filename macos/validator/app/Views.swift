import AppKit
import SwiftUI

struct RootView: View {
    @ObservedObject var model: ValidatorModel
    @ObservedObject var appearance: AppearanceStore
    let save: () -> Void

    var body: some View {
        HStack(spacing: 0) {
            Sidebar(appearance: appearance)
                .frame(width: 236)
            Rectangle().fill(Color.hairline.opacity(0.6)).frame(width: 1)
            Workspace(model: model, save: save)
                .frame(maxWidth: .infinity, maxHeight: .infinity)
                .background(Color.canvas)
        }
        .frame(minWidth: 820, idealWidth: 900, maxWidth: .infinity, minHeight: 660, idealHeight: 720, maxHeight: .infinity)
        .edgesIgnoringSafeArea(.all)
    }
}

// MARK: - Sidebar

struct VisualEffect: NSViewRepresentable {
    func makeNSView(context: Context) -> NSVisualEffectView {
        let view = NSVisualEffectView()
        view.material = .sidebar
        view.blendingMode = .behindWindow
        view.state = .followsWindowActiveState
        return view
    }
    func updateNSView(_ view: NSVisualEffectView, context: Context) {}
}

struct Sidebar: View {
    @ObservedObject var appearance: AppearanceStore

    private let steps = [
        "Start capture and approve administrator access.",
        "Connect one DAC when the log asks for it.",
        "Let the run finish, or cancel to stop early.",
        "Save the diagnostics ZIP.",
    ]

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack(spacing: 10) {
                UsbDacMark().frame(width: 30, height: 30)
                VStack(alignment: .leading, spacing: 0) {
                    Text("usbdac").font(.brand(22, 750)).foregroundColor(.ink)
                    Text("Validator for macOS").font(.brand(11.5, 600)).foregroundColor(.muted)
                }
            }
            .accessibilityElement(children: .ignore)
            .accessibilityLabel("usbdac Validator for macOS")
            .padding(.top, 56)

            Eyebrow("How a run works").padding(.top, 32).padding(.bottom, 12)
            VStack(alignment: .leading, spacing: 12) {
                ForEach(Array(steps.enumerated()), id: \.offset) { index, step in
                    HStack(alignment: .top, spacing: 10) {
                        Text("\(index + 1)")
                            .font(.plex(11))
                            .foregroundColor(.brand)
                            .frame(width: 20, height: 20)
                            .background(Circle().fill(Color.brandSoft))
                        Text(step)
                            .font(.brand(12.5, 500))
                            .foregroundColor(.ink)
                            .fixedSize(horizontal: false, vertical: true)
                            .padding(.top, 1)
                    }
                    .accessibilityElement(children: .combine)
                }
            }

            Spacer(minLength: 24)

            Eyebrow("Appearance").padding(.bottom, 8)
            Picker("Appearance", selection: $appearance.choice) {
                ForEach(AppearanceChoice.allCases) { choice in Text(choice.title).tag(choice) }
            }
            .pickerStyle(.segmented)
            .labelsHidden()

            VStack(alignment: .leading, spacing: 2) {
                Text("USB audio diagnostics")
                Text("Clear results, per device.")
            }
            .font(.brand(11.5, 500))
            .foregroundColor(.muted)
            .padding(.top, 20)
            Text(versionLabel)
                .font(.plex(10.5))
                .foregroundColor(.muted)
                .padding(.top, 6)
        }
        .padding(.horizontal, 20)
        .padding(.bottom, 20)
        .frame(maxHeight: .infinity, alignment: .top)
        // Brand-tinted vibrancy: keeps the native sidebar material while
        // pulling the wallpaper tint towards the usbdac palette.
        .background(ZStack { VisualEffect(); Color.surface.opacity(0.62) })
    }

    private var versionLabel: String {
        let version = Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String
        return version.map { "v" + $0 } ?? "development build"
    }
}

// MARK: - Workspace

struct Eyebrow: View {
    let text: String
    init(_ text: String) { self.text = text }
    var body: some View {
        Text(text.uppercased())
            .font(.brand(10.5, 700))
            .tracking(1.4)
            .foregroundColor(.muted)
            .accessibilityAddTraits(.isHeader)
    }
}

struct Workspace: View {
    @ObservedObject var model: ValidatorModel
    let save: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            VStack(alignment: .leading, spacing: 6) {
                Eyebrow("Device workspace")
                Text("Inspect. Test. Understand.")
                    .font(.brand(28, 750))
                    .foregroundColor(.ink)
                    .accessibilityAddTraits(.isHeader)
                Text("Run the direct USB validator on one DAC and keep the evidence it records.")
                    .font(.brand(13, 450))
                    .foregroundColor(.muted)
            }
            SafetyCallout()
            RunCard(model: model, save: save)
            LogPanel(model: model)
        }
        .padding(.horizontal, 32)
        .padding(.top, 44)
        .padding(.bottom, 24)
    }
}

struct SafetyCallout: View {
    var body: some View {
        HStack(alignment: .top, spacing: 12) {
            Image(systemName: "exclamationmark.triangle.fill")
                .font(.system(size: 15, weight: .semibold))
                .foregroundColor(.caution)
                .padding(.top, 1)
                .accessibilityHidden(true)
            VStack(alignment: .leading, spacing: 6) {
                Text("Before starting: remove headphones and mute or disconnect downstream amplification.")
                    .font(.brand(13, 700))
                    .foregroundColor(.caution)
                Text("DoP tests can produce loud marker noise on DACs that do not decode it. Keep amplification muted throughout the run.")
                Text("If the ordinary USB claim is denied or busy, the worker will attempt to take over the entire selected USB device, including its other interfaces. macOS administrator approval is required.")
            }
            .font(.brand(12.5, 450))
            .foregroundColor(.ink)
            .fixedSize(horizontal: false, vertical: true)
            Spacer(minLength: 0)
        }
        .padding(14)
        .background(RoundedRectangle(cornerRadius: 12, style: .continuous).fill(Color.cautionSoft))
        .overlay(RoundedRectangle(cornerRadius: 12, style: .continuous).strokeBorder(Color.caution.opacity(0.35), lineWidth: 1))
        .accessibilityElement(children: .combine)
        .accessibilityLabel("Safety warning")
    }
}

struct RunCard: View {
    @ObservedObject var model: ValidatorModel
    let save: () -> Void
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            HStack(alignment: .top, spacing: 16) {
                VStack(alignment: .leading, spacing: 5) {
                    Eyebrow("Capture")
                    Text(model.statusText)
                        .font(.brand(15, 650))
                        .foregroundColor(.ink)
                        .lineLimit(3)
                        .fixedSize(horizontal: false, vertical: true)
                        .textSelection(.enabled)
                        .accessibilityLabel("Status: \(model.statusText)")
                    if let kind = model.archiveKind {
                        HStack(spacing: 6) {
                            Image(systemName: kind == .complete ? "doc.zipper" : "exclamationmark.triangle")
                                .accessibilityHidden(true)
                            Text(kind == .complete ? "Archive transferred completely." : "Recovered archive. It holds only the evidence the app received and may be incomplete.")
                        }
                        .font(.brand(12, 600))
                        .foregroundColor(kind == .complete ? .muted : .caution)
                        .fixedSize(horizontal: false, vertical: true)
                        .padding(.top, 2)
                    }
                }
                Spacer(minLength: 8)
                StateChip(phase: model.phase)
            }
            if let progress = model.progress {
                ProgressTrack(value: progress)
                    .transition(.opacity)
            }
            Rectangle().fill(Color.hairline.opacity(0.7)).frame(height: 1)
            Toggle(isOn: $model.sourceBacked) {
                VStack(alignment: .leading, spacing: 2) {
                    Text("Include source-backed compatibility modes")
                        .font(.brand(13, 600))
                        .foregroundColor(.ink)
                    Text("Clear to use the strict verified-only policy. The choice is recorded in the ZIP.")
                        .font(.brand(11.5, 450))
                        .foregroundColor(.muted)
                }
            }
            .toggleStyle(.checkbox)
            .disabled(!model.canChangePolicy)
            HStack(spacing: 10) {
                Button(action: model.begin) { Label("Start capture", systemImage: "record.circle") }
                    .buttonStyle(BrandButtonStyle(kind: .primary))
                    .disabled(!model.canStart)
                    .help("Start capture (⌘R). Requests administrator approval.")
                Button(action: model.stop) { Label("Cancel", systemImage: "stop.circle") }
                    .buttonStyle(BrandButtonStyle(kind: .secondary))
                    .disabled(!model.canCancel)
                    .help("Cancel the run and keep its diagnostics (⌘.)")
                Spacer(minLength: 12)
                Button(action: save) { Label("Save diagnostics…", systemImage: "square.and.arrow.down") }
                    .buttonStyle(BrandButtonStyle(kind: .secondary))
                    .disabled(!model.canSave)
                    .help("Save diagnostics (⌘S)")
            }
        }
        .padding(20)
        .background(RoundedRectangle(cornerRadius: Radius.panel, style: .continuous).fill(Color.surface))
        .overlay(RoundedRectangle(cornerRadius: Radius.panel, style: .continuous).strokeBorder(Color.hairline.opacity(0.8), lineWidth: 1))
        .animation(reduceMotion ? nil : .easeOut(duration: Motion.transition), value: model.progress == nil)
    }
}

struct StateChip: View {
    let phase: ValidatorModel.Phase
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    private var spec: (label: String, symbol: String, foreground: Color, background: Color) {
        switch phase {
        case .ready: return ("Ready", "circle", .muted, .raised)
        case .awaitingApproval: return ("Waiting for approval", "lock", .brand, .brandSoft)
        case .running: return ("Running", "waveform.path", .brand, .brandSoft)
        case .stopping: return ("Stopping", "stop.circle", .caution, .cautionSoft)
        case .archiveReady: return ("Archive ready", "doc.zipper", .ink, .raised)
        case .saved: return ("Saved", "checkmark", .success, .successSoft)
        case .partial: return ("Partial", "exclamationmark.triangle", .caution, .cautionSoft)
        case .cancelled: return ("Cancelled", "xmark.circle", .caution, .cautionSoft)
        case .failed: return ("Failed", "xmark.octagon", .danger, .dangerSoft)
        }
    }

    private var active: Bool { phase == .awaitingApproval || phase == .running || phase == .stopping }

    var body: some View {
        let spec = self.spec
        HStack(spacing: 6) {
            if active && !reduceMotion {
                ProgressView().controlSize(.small).scaleEffect(0.7).frame(width: 12, height: 12)
            } else {
                Image(systemName: spec.symbol).font(.system(size: 10.5, weight: .bold))
            }
            Text(spec.label).font(.brand(12, 700))
        }
        .foregroundColor(spec.foreground)
        .padding(.horizontal, 10)
        .frame(height: 26)
        .background(Capsule().fill(spec.background))
        .fixedSize()
        .accessibilityElement(children: .ignore)
        .accessibilityLabel("Run state: \(spec.label)")
        .animation(reduceMotion ? nil : .easeOut(duration: Motion.feedback), value: phase)
    }
}

struct ProgressTrack: View {
    let value: Double
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    var body: some View {
        GeometryReader { geometry in
            ZStack(alignment: .leading) {
                Capsule().fill(Color.raised)
                Capsule().fill(Color.brand).frame(width: max(6, geometry.size.width * CGFloat(min(max(value, 0), 1))))
            }
        }
        .frame(height: 6)
        .animation(reduceMotion ? nil : .easeOut(duration: Motion.transition), value: value)
        .accessibilityElement()
        .accessibilityLabel("Progress in the current test group")
        .accessibilityValue("\(Int((value * 100).rounded())) percent")
    }
}

// MARK: - Log

struct LogPanel: View {
    @ObservedObject var model: ValidatorModel
    @State private var copied = false

    var body: some View {
        VStack(spacing: 0) {
            HStack {
                Eyebrow("Worker log")
                Spacer()
                Button(action: copy) {
                    Label(copied ? "Copied" : "Copy log", systemImage: copied ? "checkmark" : "doc.on.doc")
                }
                .buttonStyle(BrandButtonStyle(kind: .quiet))
                .disabled(model.log.isEmpty)
            }
            .padding(.leading, 16)
            .padding(.trailing, 8)
            .frame(height: 40)
            Rectangle().fill(Color.hairline.opacity(0.7)).frame(height: 1)
            LogTextView(text: model.log, generation: model.logGeneration)
                .overlay(
                    Group {
                        if model.log.isEmpty {
                            Text("Worker output appears here during a run.")
                                .font(.plex(11.5))
                                .foregroundColor(.muted)
                                .accessibilityHidden(true)
                        }
                    }
                )
        }
        .frame(minHeight: 120, maxHeight: .infinity)
        .background(RoundedRectangle(cornerRadius: Radius.panel, style: .continuous).fill(Color.surface))
        .clipShape(RoundedRectangle(cornerRadius: Radius.panel, style: .continuous))
        .overlay(RoundedRectangle(cornerRadius: Radius.panel, style: .continuous).strokeBorder(Color.hairline.opacity(0.8), lineWidth: 1))
    }

    private func copy() {
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(model.log, forType: .string)
        copied = true
        DispatchQueue.main.asyncAfter(deadline: .now() + 1.6) { copied = false }
    }
}

// Selectable, appending log view. Appends only the new suffix so long runs stay
// cheap, and follows the end of the log unless the person has scrolled up.
struct LogTextView: NSViewRepresentable {
    let text: String
    let generation: Int

    final class Coordinator {
        var generation = -1
        var length = 0
    }

    func makeCoordinator() -> Coordinator { Coordinator() }

    private static var attributes: [NSAttributedString.Key: Any] {
        let paragraph = NSMutableParagraphStyle()
        paragraph.lineHeightMultiple = 1.18
        return [.font: BrandFont.mono(11.5), .foregroundColor: Palette.text, .paragraphStyle: paragraph]
    }

    func makeNSView(context: Context) -> NSScrollView {
        let scroll = NSTextView.scrollableTextView()
        scroll.drawsBackground = false
        scroll.borderType = .noBorder
        scroll.hasVerticalScroller = true
        scroll.autohidesScrollers = true
        if let textView = scroll.documentView as? NSTextView {
            textView.isEditable = false
            textView.isSelectable = true
            textView.drawsBackground = false
            textView.isRichText = false
            textView.usesFindBar = true
            textView.isIncrementalSearchingEnabled = true
            textView.textContainerInset = NSSize(width: 10, height: 10)
            textView.font = BrandFont.mono(11.5)
            textView.textColor = Palette.text
            textView.selectedTextAttributes = [.backgroundColor: Palette.primarySoft, .foregroundColor: Palette.text]
            textView.setAccessibilityLabel("Worker log")
        }
        return scroll
    }

    func updateNSView(_ scroll: NSScrollView, context: Context) {
        guard let textView = scroll.documentView as? NSTextView, let storage = textView.textStorage else { return }
        let coordinator = context.coordinator
        let current = text as NSString
        let visible = scroll.contentView.bounds
        let following = visible.maxY >= textView.frame.height - 24
        if coordinator.generation != generation || current.length < coordinator.length {
            storage.setAttributedString(NSAttributedString(string: text, attributes: Self.attributes))
        } else if current.length > coordinator.length {
            storage.append(NSAttributedString(string: current.substring(from: coordinator.length), attributes: Self.attributes))
        } else { return }
        coordinator.generation = generation
        coordinator.length = current.length
        if following { textView.scrollToEndOfDocument(nil) }
    }
}

// MARK: - Buttons

struct BrandButtonStyle: ButtonStyle {
    enum Kind { case primary, secondary, quiet }
    let kind: Kind

    func makeBody(configuration: Configuration) -> some View {
        BrandButton(configuration: configuration, kind: kind)
    }

    private struct BrandButton: View {
        let configuration: ButtonStyleConfiguration
        let kind: Kind
        @Environment(\.isEnabled) private var isEnabled
        @Environment(\.isFocused) private var isFocused
        @State private var hovering = false

        var body: some View {
            let shape = RoundedRectangle(cornerRadius: Radius.control, style: .continuous)
            configuration.label
                .font(.brand(kind == .quiet ? 12 : 13, 650))
                .labelStyle(.titleAndIcon)
                .foregroundColor(foreground)
                .padding(.horizontal, kind == .quiet ? 10 : 16)
                .frame(height: kind == .quiet ? 28 : 34)
                .background(shape.fill(background))
                .overlay(shape.strokeBorder(kind == .secondary ? Color.controlBorder : Color.clear, lineWidth: 1))
                .overlay(
                    RoundedRectangle(cornerRadius: Radius.control + 3, style: .continuous)
                        .strokeBorder(Color.focusRing, lineWidth: 2)
                        .padding(-3)
                        .opacity(isFocused ? 1 : 0)
                )
                .opacity(isEnabled ? 1 : 0.42)
                .contentShape(shape)
                .fixedSize()
                .onHover { hovering = $0 }
        }

        private var foreground: Color {
            switch kind {
            case .primary: return .onBrand
            case .secondary: return .ink
            case .quiet: return .brand
            }
        }

        private var background: Color {
            let pressed = configuration.isPressed
            switch kind {
            case .primary: return Color.brand.opacity(pressed ? 0.82 : (hovering && isEnabled ? 0.92 : 1))
            case .secondary: return pressed ? Color.raised : (hovering && isEnabled ? Color.raised.opacity(0.6) : Color.surface)
            case .quiet: return pressed ? Color.brandSoft : (hovering && isEnabled ? Color.brandSoft.opacity(0.7) : Color.clear)
            }
        }
    }
}
