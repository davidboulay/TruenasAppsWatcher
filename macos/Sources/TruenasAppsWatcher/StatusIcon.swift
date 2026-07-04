// SPDX-License-Identifier: GPL-3.0-only
//
// The menu bar badge: the same NAS chassis (two drive bays) as the Linux
// applet, drawn in TrueNAS blue with an up-arrow when updates are pending
// and in muted blue with a checkmark when everything is up to date. The
// pending count is drawn into the same image, so the menu bar label is a
// single NSImage.

import AppKit

enum StatusIcon {
    private static let availableBlue = NSColor(
        calibratedRed: 0x00 / 255, green: 0x95 / 255, blue: 0xD5 / 255, alpha: 1)
    private static let okBlue = NSColor(
        calibratedRed: 0x3E / 255, green: 0x6D / 255, blue: 0x89 / 255, alpha: 1)

    static func image(count: Int, configured: Bool) -> NSImage {
        let pending = count > 0 || !configured
        let color = pending ? availableBlue : okBlue
        let text = count > 0 ? "\(count)" : nil

        let font = NSFont.systemFont(ofSize: 11, weight: .semibold)
        let attributes: [NSAttributedString.Key: Any] = [
            .font: font,
            .foregroundColor: color,
        ]
        let textSize = text.map { NSAttributedString(string: $0, attributes: attributes).size() }
        let iconSize: CGFloat = 16
        let gap: CGFloat = 3
        let width = iconSize + (textSize.map { $0.width + gap } ?? 0)
        let height: CGFloat = 18

        let image = NSImage(size: NSSize(width: width, height: height), flipped: true) { _ in
            // 16×16 NAS glyph, vertically centred, in the source SVG's
            // top-left coordinate space (hence flipped: true).
            let path = nasPath(offsetY: (height - iconSize) / 2)
            color.setFill()
            path.windingRule = .evenOdd
            appendGlyph(path, pending: pending, offsetY: (height - iconSize) / 2)
            path.fill()

            if let text, let textSize {
                // draw(at:) in a flipped context positions by top-left.
                NSAttributedString(string: text, attributes: attributes).draw(
                    at: NSPoint(x: iconSize + gap, y: (height - textSize.height) / 2))
            }
            return true
        }
        // Keep the TrueNAS blue rather than letting the menu bar tint it.
        image.isTemplate = false
        return image
    }

    /// Chassis body and the two drive-bay slots (knocked out via even-odd).
    private static func nasPath(offsetY dy: CGFloat) -> NSBezierPath {
        let path = NSBezierPath(
            roundedRect: NSRect(x: 2.5, y: 1.5 + dy, width: 11, height: 13),
            xRadius: 1.5, yRadius: 1.5)
        path.appendRect(NSRect(x: 4.5, y: 3.5 + dy, width: 7, height: 1.2))
        path.appendRect(NSRect(x: 4.5, y: 6.0 + dy, width: 7, height: 1.2))
        return path
    }

    /// The up-arrow (updates pending) or checkmark (up to date) knockout.
    private static func appendGlyph(_ path: NSBezierPath, pending: Bool, offsetY dy: CGFloat) {
        if pending {
            // M8 8 L10.7 11.4 H9.1 V13.2 H6.9 V11.4 H5.3 Z
            path.move(to: NSPoint(x: 8, y: 8 + dy))
            path.line(to: NSPoint(x: 10.7, y: 11.4 + dy))
            path.line(to: NSPoint(x: 9.1, y: 11.4 + dy))
            path.line(to: NSPoint(x: 9.1, y: 13.2 + dy))
            path.line(to: NSPoint(x: 6.9, y: 13.2 + dy))
            path.line(to: NSPoint(x: 6.9, y: 11.4 + dy))
            path.line(to: NSPoint(x: 5.3, y: 11.4 + dy))
            path.close()
        } else {
            // M10.9 8.3 L11.8 9.2 L7.4 13.6 L4.7 10.9 L5.6 10 L7.4 11.8 Z
            path.move(to: NSPoint(x: 10.9, y: 8.3 + dy))
            path.line(to: NSPoint(x: 11.8, y: 9.2 + dy))
            path.line(to: NSPoint(x: 7.4, y: 13.6 + dy))
            path.line(to: NSPoint(x: 4.7, y: 10.9 + dy))
            path.line(to: NSPoint(x: 5.6, y: 10.0 + dy))
            path.line(to: NSPoint(x: 7.4, y: 11.8 + dy))
            path.close()
        }
    }
}
