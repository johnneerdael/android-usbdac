import AppKit
import SwiftUI

final class App: NSObject, NSApplicationDelegate, NSMenuItemValidation, NSWindowDelegate {
    let model = ValidatorModel()
    let appearance = AppearanceStore()
    var window: NSWindow!

    func applicationDidFinishLaunching(_ notification: Notification) {
        BrandFont.register()
        appearance.apply()
        NSApp.mainMenu = makeMainMenu()

        window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 900, height: 720), styleMask: [.titled, .closable, .miniaturizable, .resizable, .fullSizeContentView], backing: .buffered, defer: false)
        window.title = "usbdac Validator"
        window.titleVisibility = .hidden
        window.titlebarAppearsTransparent = true
        window.isMovableByWindowBackground = true
        window.isReleasedWhenClosed = false
        window.contentMinSize = NSSize(width: 820, height: 660)
        window.delegate = self
        let root = RootView(model: model, appearance: appearance) { [weak self] in self?.saveDiagnostics(nil) }
        let host = NSHostingView(rootView: root)
        // Size the window from contentMinSize and the autosaved frame, not from
        // the ideal size of the flexible log panel.
        if #available(macOS 13.0, *) { host.sizingOptions = [.minSize] }
        window.contentView = host
        window.center()
        window.setFrameAutosaveName("ValidatorWindow")
        window.makeKeyAndOrderFront(nil); NSApp.activate(ignoringOtherApps: true)
    }

    // MARK: Actions

    @objc func startCapture(_ sender: Any?) { model.begin() }
    @objc func cancelCapture(_ sender: Any?) { if model.canCancel { model.stop() } }
    @objc func saveDiagnostics(_ sender: Any?) { if model.canSave { model.saveArchive(from: window) } }
    @objc func chooseAppearance(_ sender: NSMenuItem) {
        if let choice = sender.representedObject as? String, let value = AppearanceChoice(rawValue: choice) { appearance.choice = value }
    }
    @objc func showAbout(_ sender: Any?) {
        NSApp.orderFrontStandardAboutPanel(options: [
            .applicationName: "usbdac Validator",
            .credits: NSAttributedString(string: "Direct USB audio. Clear evidence.\nDiagnostics for one USB DAC at a time.", attributes: [.font: NSFont.systemFont(ofSize: 11), .foregroundColor: NSColor.secondaryLabelColor]),
        ])
    }

    func validateMenuItem(_ item: NSMenuItem) -> Bool {
        switch item.action {
        case #selector(startCapture(_:)): return model.canStart
        case #selector(cancelCapture(_:)): return model.canCancel
        case #selector(saveDiagnostics(_:)): return model.canSave
        case #selector(chooseAppearance(_:)):
            item.state = (item.representedObject as? String) == appearance.choice.rawValue ? .on : .off
            return true
        default: return true
        }
    }

    // MARK: Lifecycle

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool { true }
    func applicationShouldTerminate(_ sender: NSApplication) -> NSApplication.TerminateReply { if model.worker != nil { model.stop(); return .terminateCancel }; return .terminateNow }

    // MARK: Menu

    private func makeMainMenu() -> NSMenu {
        let main = NSMenu()
        let name = "USB DAC Validator"

        let appMenu = NSMenu(title: name)
        appMenu.addItem(withTitle: "About \(name)", action: #selector(showAbout(_:)), keyEquivalent: "")
        appMenu.addItem(.separator())
        let services = NSMenu(title: "Services")
        appMenu.addItem(withTitle: "Services", action: nil, keyEquivalent: "").submenu = services
        NSApp.servicesMenu = services
        appMenu.addItem(.separator())
        appMenu.addItem(withTitle: "Hide \(name)", action: #selector(NSApplication.hide(_:)), keyEquivalent: "h")
        appMenu.addItem(withTitle: "Hide Others", action: #selector(NSApplication.hideOtherApplications(_:)), keyEquivalent: "h").keyEquivalentModifierMask = [.command, .option]
        appMenu.addItem(withTitle: "Show All", action: #selector(NSApplication.unhideAllApplications(_:)), keyEquivalent: "")
        appMenu.addItem(.separator())
        appMenu.addItem(withTitle: "Quit \(name)", action: #selector(NSApplication.terminate(_:)), keyEquivalent: "q")
        main.addItem(withTitle: name, action: nil, keyEquivalent: "").submenu = appMenu

        let file = NSMenu(title: "File")
        file.addItem(withTitle: "Start Capture", action: #selector(startCapture(_:)), keyEquivalent: "r")
        file.addItem(withTitle: "Cancel Capture", action: #selector(cancelCapture(_:)), keyEquivalent: ".")
        file.addItem(.separator())
        file.addItem(withTitle: "Save Diagnostics…", action: #selector(saveDiagnostics(_:)), keyEquivalent: "s")
        file.addItem(.separator())
        file.addItem(withTitle: "Close Window", action: #selector(NSWindow.performClose(_:)), keyEquivalent: "w")
        main.addItem(withTitle: "File", action: nil, keyEquivalent: "").submenu = file

        let edit = NSMenu(title: "Edit")
        edit.addItem(withTitle: "Copy", action: #selector(NSText.copy(_:)), keyEquivalent: "c")
        edit.addItem(withTitle: "Select All", action: #selector(NSText.selectAll(_:)), keyEquivalent: "a")
        edit.addItem(.separator())
        let find = NSMenu(title: "Find")
        let findItem = find.addItem(withTitle: "Find…", action: #selector(NSTextView.performFindPanelAction(_:)), keyEquivalent: "f")
        findItem.tag = Int(NSFindPanelAction.showFindPanel.rawValue)
        let nextItem = find.addItem(withTitle: "Find Next", action: #selector(NSTextView.performFindPanelAction(_:)), keyEquivalent: "g")
        nextItem.tag = Int(NSFindPanelAction.next.rawValue)
        edit.addItem(withTitle: "Find", action: nil, keyEquivalent: "").submenu = find
        main.addItem(withTitle: "Edit", action: nil, keyEquivalent: "").submenu = edit

        let view = NSMenu(title: "View")
        let appearanceMenu = NSMenu(title: "Appearance")
        for choice in AppearanceChoice.allCases {
            let item = appearanceMenu.addItem(withTitle: choice.title, action: #selector(chooseAppearance(_:)), keyEquivalent: "")
            item.representedObject = choice.rawValue
        }
        view.addItem(withTitle: "Appearance", action: nil, keyEquivalent: "").submenu = appearanceMenu
        main.addItem(withTitle: "View", action: nil, keyEquivalent: "").submenu = view

        let windowMenu = NSMenu(title: "Window")
        windowMenu.addItem(withTitle: "Minimize", action: #selector(NSWindow.performMiniaturize(_:)), keyEquivalent: "m")
        windowMenu.addItem(withTitle: "Zoom", action: #selector(NSWindow.performZoom(_:)), keyEquivalent: "")
        windowMenu.addItem(.separator())
        windowMenu.addItem(withTitle: "Bring All to Front", action: #selector(NSApplication.arrangeInFront(_:)), keyEquivalent: "")
        main.addItem(withTitle: "Window", action: nil, keyEquivalent: "").submenu = windowMenu
        NSApp.windowsMenu = windowMenu

        return main
    }
}

let app = NSApplication.shared
let delegate = App(); app.delegate = delegate; app.setActivationPolicy(.regular); app.run()
