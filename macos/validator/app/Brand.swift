import AppKit
import CoreText
import SwiftUI

// Semantic colors from branding/tokens/usbdac.json. Each resolves against the
// effective appearance of the view drawing it, so window chrome, AppKit views
// and SwiftUI stay in agreement when the theme changes.
enum Palette {
    private static func dynamic(_ name: String, dark: UInt32, light: UInt32) -> NSColor {
        NSColor(name: NSColor.Name("usbdac." + name)) { appearance in
            let hex = appearance.bestMatch(from: [.darkAqua, .aqua]) == .darkAqua ? dark : light
            return NSColor(srgbRed: CGFloat((hex >> 16) & 0xFF) / 255, green: CGFloat((hex >> 8) & 0xFF) / 255, blue: CGFloat(hex & 0xFF) / 255, alpha: 1)
        }
    }
    static let canvas = dynamic("canvas", dark: 0x11151C, light: 0xF2F5FA)
    static let surface = dynamic("surface", dark: 0x1A212C, light: 0xFFFFFF)
    static let raised = dynamic("raised", dark: 0x252F3D, light: 0xE7EDF5)
    static let text = dynamic("text", dark: 0xF3F5F7, light: 0x152237)
    static let muted = dynamic("muted", dark: 0xB2C0D2, light: 0x4E6078)
    static let primary = dynamic("primary", dark: 0x78B5FF, light: 0x2367C9)
    static let onPrimary = dynamic("onPrimary", dark: 0x101E32, light: 0xFFFFFF)
    static let primarySoft = dynamic("primarySoft", dark: 0x213753, light: 0xE1EDFF)
    static let border = dynamic("border", dark: 0x415169, light: 0xACB9CA)
    static let controlBorder = dynamic("controlBorder", dark: 0x7386A1, light: 0x6C809B)
    static let success = dynamic("success", dark: 0x87DFC2, light: 0x17634D)
    static let successSoft = dynamic("successSoft", dark: 0x203D34, light: 0xDDF4EA)
    static let warning = dynamic("warning", dark: 0xF1CA83, light: 0x76500B)
    static let warningSoft = dynamic("warningSoft", dark: 0x413723, light: 0xFFF0D2)
    static let danger = dynamic("danger", dark: 0xFFABB8, light: 0xA02945)
    static let dangerSoft = dynamic("dangerSoft", dark: 0x442C36, light: 0xFCE5EC)
    static let focus = dynamic("focus", dark: 0xA3CDFF, light: 0x1358B7)
    // Mark colors from branding/logos/usbdac-mark-{dark,light}.svg.
    static let markRibbon = dynamic("markRibbon", dark: 0xF3F5F7, light: 0x152237)
    static let markBlue = dynamic("markBlue", dark: 0x65A9FF, light: 0x2367C9)
}

extension Color {
    static let canvas = Color(nsColor: Palette.canvas)
    static let surface = Color(nsColor: Palette.surface)
    static let raised = Color(nsColor: Palette.raised)
    static let ink = Color(nsColor: Palette.text)
    static let muted = Color(nsColor: Palette.muted)
    static let brand = Color(nsColor: Palette.primary)
    static let onBrand = Color(nsColor: Palette.onPrimary)
    static let brandSoft = Color(nsColor: Palette.primarySoft)
    static let hairline = Color(nsColor: Palette.border)
    static let controlBorder = Color(nsColor: Palette.controlBorder)
    static let success = Color(nsColor: Palette.success)
    static let successSoft = Color(nsColor: Palette.successSoft)
    static let caution = Color(nsColor: Palette.warning)
    static let cautionSoft = Color(nsColor: Palette.warningSoft)
    static let danger = Color(nsColor: Palette.danger)
    static let dangerSoft = Color(nsColor: Palette.dangerSoft)
    static let focusRing = Color(nsColor: Palette.focus)
}

enum Radius {
    static let control: CGFloat = 8
    static let panel: CGFloat = 16
    static let sheet: CGFloat = 24
}

enum Motion {
    static let feedback = 0.12
    static let transition = 0.18
}

// Manrope (variable, 200-800) and IBM Plex Mono ship in Contents/Resources.
// When registration fails the app falls back to the system faces.
enum BrandFont {
    private static let weightAxis = NSNumber(value: 0x77676874) // 'wght'
    private(set) static var manropeAvailable = false
    private(set) static var plexAvailable = false

    static func register() {
        for name in ["Manrope", "IBMPlexMono-Regular"] {
            guard let url = Bundle.main.url(forResource: name, withExtension: "ttf") else { continue }
            var error: Unmanaged<CFError>?
            if !CTFontManagerRegisterFontsForURL(url as CFURL, .process, &error) { error?.release() }
        }
        manropeAvailable = NSFont(name: "Manrope-Regular", size: 12) != nil
        plexAvailable = NSFont(name: "IBMPlexMono-Regular", size: 12) != nil
    }

    static func ui(_ size: CGFloat, _ weight: CGFloat) -> NSFont {
        if manropeAvailable {
            let descriptor = NSFontDescriptor(fontAttributes: [.name: "Manrope-Regular", .variation: [weightAxis: NSNumber(value: Double(weight))]])
            if let font = NSFont(descriptor: descriptor, size: size) { return font }
        }
        let system: NSFont.Weight = weight >= 750 ? .heavy : weight >= 650 ? .bold : weight >= 550 ? .semibold : weight >= 450 ? .medium : .regular
        return .systemFont(ofSize: size, weight: system)
    }

    static func mono(_ size: CGFloat) -> NSFont {
        if plexAvailable, let font = NSFont(name: "IBMPlexMono-Regular", size: size) { return font }
        return .monospacedSystemFont(ofSize: size, weight: .regular)
    }
}

extension Font {
    static func brand(_ size: CGFloat, _ weight: CGFloat = 450) -> Font { Font(BrandFont.ui(size, weight) as CTFont) }
    static func plex(_ size: CGFloat) -> Font { Font(BrandFont.mono(size) as CTFont) }
}

// System follows macOS until the person chooses; the choice is persisted and
// applied to NSApp so native chrome, panels and SwiftUI agree.
enum AppearanceChoice: String, CaseIterable, Identifiable {
    case system, dark, light
    static let defaultsKey = "appearance"
    var id: String { rawValue }
    var title: String {
        switch self {
        case .system: return "System"
        case .dark: return "Dark"
        case .light: return "Light"
        }
    }
    var nsAppearance: NSAppearance? {
        switch self {
        case .system: return nil
        case .dark: return NSAppearance(named: .darkAqua)
        case .light: return NSAppearance(named: .aqua)
        }
    }
}

final class AppearanceStore: ObservableObject {
    @Published var choice: AppearanceChoice {
        didSet { apply(); UserDefaults.standard.set(choice.rawValue, forKey: AppearanceChoice.defaultsKey) }
    }
    init() {
        choice = UserDefaults.standard.string(forKey: AppearanceChoice.defaultsKey).flatMap(AppearanceChoice.init(rawValue:)) ?? .system
    }
    func apply() { NSApp.appearance = choice.nsAppearance }
}

// The usbdac mark, drawn from the three paths in usbdac-mark-*.svg (1000 x 1000).
struct MarkShape: Shape {
    enum Part { case ribbonTop, ribbonBottom, blue }
    let part: Part
    private var source: String {
        switch part {
        case .ribbonTop: return "M920 60L710 183Q700 190 700 202V390Q700 403 686 411L528 503L746 630L878 554Q920 530 920 480Z"
        case .ribbonBottom: return "M80 601L470 826L318 914Q292 929 266 914L126 832Q80 805 80 752Z"
        case .blue: return "M80 60L290 183Q300 190 300 202V390Q300 403 314 411L746 662Q760 670 773 662L920 578V822Q920 846 898 859L800 916Q774 931 752 918L126 556Q80 530 80 480Z"
        }
    }
    func path(in rect: CGRect) -> Path {
        let scale = min(rect.width, rect.height) / 1000
        let origin = CGPoint(x: rect.midX - 500 * scale, y: rect.midY - 500 * scale)
        func point(_ x: CGFloat, _ y: CGFloat) -> CGPoint { CGPoint(x: origin.x + x * scale, y: origin.y + y * scale) }
        // Absolute M, L, Q, H, V and Z commands only, as exported by the brand kit.
        var tokens: [String] = []
        var current = ""
        for character in source {
            if character.isLetter {
                if !current.isEmpty { tokens.append(current); current = "" }
                tokens.append(String(character))
            } else if character == " " || character == "," {
                if !current.isEmpty { tokens.append(current); current = "" }
            } else { current.append(character) }
        }
        if !current.isEmpty { tokens.append(current) }
        var path = Path()
        var index = 0
        var command = ""
        var x: CGFloat = 0, y: CGFloat = 0
        func number() -> CGFloat { defer { index += 1 }; return CGFloat(Double(tokens[index]) ?? 0) }
        while index < tokens.count {
            if tokens[index].first?.isLetter == true { command = tokens[index]; index += 1 }
            switch command {
            case "M": x = number(); y = number(); path.move(to: point(x, y)); command = "L"
            case "L": x = number(); y = number(); path.addLine(to: point(x, y))
            case "H": x = number(); path.addLine(to: point(x, y))
            case "V": y = number(); path.addLine(to: point(x, y))
            case "Q":
                let cx = number(), cy = number()
                x = number(); y = number()
                path.addQuadCurve(to: point(x, y), control: point(cx, cy))
            case "Z": path.closeSubpath(); command = ""
            default: index += 1
            }
        }
        return path
    }
}

struct UsbDacMark: View {
    var body: some View {
        ZStack {
            MarkShape(part: .ribbonTop).fill(Color(nsColor: Palette.markRibbon))
            MarkShape(part: .ribbonBottom).fill(Color(nsColor: Palette.markRibbon))
            MarkShape(part: .blue).fill(Color(nsColor: Palette.markBlue))
        }
        .accessibilityHidden(true)
    }
}
