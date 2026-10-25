// Draws the Merpati app icon (a stylised pigeon in flight) as a 1024×1024 PNG.
//
//   swift packaging/macos/make-icon.swift out.png

import AppKit

let size: CGFloat = 1024
let out = CommandLine.arguments.dropFirst().first ?? "icon.png"

let rep = NSBitmapImageRep(
    bitmapDataPlanes: nil, pixelsWide: Int(size), pixelsHigh: Int(size), bitsPerSample: 8, samplesPerPixel: 4,
    hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
let ctx = NSGraphicsContext.current!.cgContext

// Rounded-square background with the app's accent gradient (macOS grid: 824 px tile).
let inset: CGFloat = 100
let tile = CGRect(x: inset, y: inset, width: size - 2 * inset, height: size - 2 * inset)
let tilePath = CGPath(roundedRect: tile, cornerWidth: 185, cornerHeight: 185, transform: nil)
ctx.saveGState()
ctx.setShadow(offset: CGSize(width: 0, height: -12), blur: 28, color: NSColor(white: 0, alpha: 0.28).cgColor)
ctx.addPath(tilePath)
ctx.setFillColor(NSColor.black.cgColor)
ctx.fillPath()
ctx.restoreGState()
ctx.saveGState()
ctx.addPath(tilePath)
ctx.clip()
let colors = [
    NSColor(srgbRed: 0.97, green: 0.47, blue: 0.27, alpha: 1).cgColor,
    NSColor(srgbRed: 0.84, green: 0.25, blue: 0.20, alpha: 1).cgColor,
] as CFArray
let gradient = CGGradient(colorsSpace: CGColorSpace(name: CGColorSpace.sRGB), colors: colors, locations: [0, 1])!
ctx.drawLinearGradient(gradient, start: CGPoint(x: 0, y: size - inset), end: CGPoint(x: 0, y: inset), options: [])
ctx.restoreGState()

// Bird: body, raised wing, tail and head, in white.
ctx.setFillColor(NSColor.white.cgColor)
let body = CGMutablePath()
body.move(to: CGPoint(x: 250, y: 440))
body.addCurve(to: CGPoint(x: 700, y: 470), control1: CGPoint(x: 380, y: 360), control2: CGPoint(x: 600, y: 360))
body.addCurve(to: CGPoint(x: 780, y: 560), control1: CGPoint(x: 745, y: 495), control2: CGPoint(x: 770, y: 525))
body.addCurve(to: CGPoint(x: 690, y: 560), control1: CGPoint(x: 750, y: 575), control2: CGPoint(x: 715, y: 575))
body.addCurve(to: CGPoint(x: 250, y: 440), control1: CGPoint(x: 560, y: 500), control2: CGPoint(x: 380, y: 470))
body.closeSubpath()
ctx.addPath(body)
ctx.fillPath()

let wing = CGMutablePath()
wing.move(to: CGPoint(x: 420, y: 470))
wing.addCurve(to: CGPoint(x: 560, y: 790), control1: CGPoint(x: 430, y: 600), control2: CGPoint(x: 480, y: 720))
wing.addCurve(to: CGPoint(x: 600, y: 480), control1: CGPoint(x: 640, y: 690), control2: CGPoint(x: 650, y: 560))
wing.closeSubpath()
ctx.addPath(wing)
ctx.fillPath()

let tail = CGMutablePath()
tail.move(to: CGPoint(x: 290, y: 452))
tail.addLine(to: CGPoint(x: 185, y: 520))
tail.addLine(to: CGPoint(x: 215, y: 420))
tail.addLine(to: CGPoint(x: 175, y: 360))
tail.closeSubpath()
ctx.addPath(tail)
ctx.fillPath()

ctx.fillEllipse(in: CGRect(x: 690, y: 515, width: 105, height: 105))
// Eye and beak.
ctx.setFillColor(NSColor(srgbRed: 0.84, green: 0.25, blue: 0.20, alpha: 1).cgColor)
ctx.fillEllipse(in: CGRect(x: 748, y: 568, width: 18, height: 18))
ctx.setFillColor(NSColor(srgbRed: 1.0, green: 0.83, blue: 0.35, alpha: 1).cgColor)
let beak = CGMutablePath()
beak.move(to: CGPoint(x: 790, y: 585))
beak.addLine(to: CGPoint(x: 845, y: 565))
beak.addLine(to: CGPoint(x: 788, y: 553))
beak.closeSubpath()
ctx.addPath(beak)
ctx.fillPath()

NSGraphicsContext.current = nil
let png = rep.representation(using: .png, properties: [:])!
try! png.write(to: URL(fileURLWithPath: out))
