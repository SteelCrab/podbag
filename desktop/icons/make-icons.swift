// Make podbag's app icons from the logo (docs/images/logo.png), on Apple's macOS app icon
// grid: a 1024 px canvas, a rounded square of 824 px in the middle (continuous corners),
// a soft shadow in the margin, a light blue background and the bag in the middle.
//
//   swift desktop/icons/make-icons.swift <logo.png> <out-dir> [preview.png]
//
// Writes <out-dir>/icon.iconset (every size iconutil wants), 32x32.png, 64x64.png,
// 128x128.png, 128x128@2x.png, icon.png (512) and icon.ico. With a third argument, also a
// picture of the icon at 1024, 128, 32 and 16 px on light and dark backgrounds.

import AppKit
import Foundation

let args = CommandLine.arguments
guard args.count >= 3, let logo = NSImage(contentsOfFile: args[1]),
  let logoCG = logo.cgImage(forProposedRect: nil, context: nil, hints: nil)
else {
  FileHandle.standardError.write("usage: make-icons.swift <logo.png> <out-dir> [preview.png]\n".data(using: .utf8)!)
  exit(2)
}
let out = URL(fileURLWithPath: args[2])

/// The rounded square with continuous corners: a superellipse (exponent 5), close to the
/// shape macOS draws.
func squircle(in r: CGRect) -> CGPath {
  let path = CGMutablePath()
  let n = 5.0
  let (a, b) = (r.width / 2, r.height / 2)
  let (cx, cy) = (r.midX, r.midY)
  let steps = 720
  for i in 0...steps {
    let t = Double(i) / Double(steps) * 2 * Double.pi
    let c = cos(t), s = sin(t)
    let x = cx + a * (c >= 0 ? 1 : -1) * pow(abs(c), 2 / n)
    let y = cy + b * (s >= 0 ? 1 : -1) * pow(abs(s), 2 / n)
    if i == 0 { path.move(to: CGPoint(x: x, y: y)) } else { path.addLine(to: CGPoint(x: x, y: y)) }
  }
  path.closeSubpath()
  return path
}

/// The icon at `size` px. Small sizes give the bag more room, so it still reads at 16 px.
func icon(_ size: Int) -> CGImage {
  let s = CGFloat(size)
  let ctx = CGContext(
    data: nil, width: size, height: size, bitsPerComponent: 8, bytesPerRow: 0,
    space: CGColorSpace(name: CGColorSpace.sRGB)!,
    bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
  ctx.interpolationQuality = .high
  let unit = s / 1024
  // The grid: 824 of 1024, centered (a bit more at the smallest sizes, where the margin
  // only wastes pixels).
  let shapeSide = (size <= 32 ? 900 : 824) * unit
  let shape = CGRect(x: (s - shapeSide) / 2, y: (s - shapeSide) / 2, width: shapeSide, height: shapeSide)
  let path = squircle(in: shape)
  // The soft shadow below the shape, in the margin.
  if size > 32 {
    ctx.saveGState()
    ctx.setShadow(offset: CGSize(width: 0, height: -10 * unit), blur: 28 * unit,
                  color: CGColor(gray: 0, alpha: 0.30))
    ctx.addPath(path)
    ctx.setFillColor(CGColor(gray: 1, alpha: 1))
    ctx.fillPath()
    ctx.restoreGState()
  }
  // The background: a calm light blue, lighter at the top.
  ctx.saveGState()
  ctx.addPath(path)
  ctx.clip()
  let colors = [
    CGColor(srgbRed: 0.965, green: 0.976, blue: 1.0, alpha: 1),
    CGColor(srgbRed: 0.808, green: 0.863, blue: 0.976, alpha: 1),
  ] as CFArray
  let gradient = CGGradient(colorsSpace: CGColorSpace(name: CGColorSpace.sRGB)!, colors: colors,
                            locations: [0, 1])!
  ctx.drawLinearGradient(gradient, start: CGPoint(x: 0, y: shape.maxY),
                         end: CGPoint(x: 0, y: shape.minY), options: [])
  // A thin edge, so the light shape keeps its outline on a light desktop.
  ctx.addPath(path)
  ctx.setStrokeColor(CGColor(srgbRed: 0.10, green: 0.25, blue: 0.65, alpha: 0.18))
  ctx.setLineWidth(max(1, 4 * unit))
  ctx.strokePath()
  // The bag, centered, with breathing room (more of the shape at small sizes).
  let share: CGFloat = size <= 16 ? 0.86 : size <= 32 ? 0.80 : 0.70
  let glyph = shapeSide * share
  let rect = CGRect(x: (s - glyph) / 2, y: (s - glyph) / 2 - glyph * 0.01, width: glyph, height: glyph)
  ctx.draw(logoCG, in: rect)
  ctx.restoreGState()
  return ctx.makeImage()!
}

func png(_ image: CGImage) -> Data {
  let rep = NSBitmapImageRep(cgImage: image)
  return rep.representation(using: .png, properties: [:])!
}

func write(_ data: Data, _ url: URL) {
  try! FileManager.default.createDirectory(at: url.deletingLastPathComponent(),
                                           withIntermediateDirectories: true)
  try! data.write(to: url)
}

// The iconset: 16 … 512, each @1x and @2x.
let iconset = out.appendingPathComponent("icon.iconset")
try? FileManager.default.removeItem(at: iconset)
for base in [16, 32, 128, 256, 512] {
  write(png(icon(base)), iconset.appendingPathComponent("icon_\(base)x\(base).png"))
  write(png(icon(base * 2)), iconset.appendingPathComponent("icon_\(base)x\(base)@2x.png"))
}
// What Tauri reads (the window icon on Linux and Windows, the bundle's lists).
for (name, size) in [("32x32.png", 32), ("64x64.png", 64), ("128x128.png", 128),
                     ("128x128@2x.png", 256), ("icon.png", 512)] {
  write(png(icon(size)), out.appendingPathComponent(name))
}
// icon.ico: PNG entries of 16, 24, 32, 48, 64 and 256 px.
var ico = Data()
let sizes = [16, 24, 32, 48, 64, 256]
let images = sizes.map { png(icon($0)) }
func le16(_ v: Int) -> Data { withUnsafeBytes(of: UInt16(v).littleEndian) { Data($0) } }
func le32(_ v: Int) -> Data { withUnsafeBytes(of: UInt32(v).littleEndian) { Data($0) } }
ico.append(le16(0)); ico.append(le16(1)); ico.append(le16(sizes.count))
var offset = 6 + 16 * sizes.count
for (i, size) in sizes.enumerated() {
  ico.append(UInt8(size >= 256 ? 0 : size)); ico.append(UInt8(size >= 256 ? 0 : size))
  ico.append(0); ico.append(0)
  ico.append(le16(1)); ico.append(le16(32))
  ico.append(le32(images[i].count)); ico.append(le32(offset))
  offset += images[i].count
}
for data in images { ico.append(data) }
write(ico, out.appendingPathComponent("icon.ico"))

// The preview: 1024, 128, 32 and 16 px (each shown at its own size) on light and dark.
if args.count >= 4 {
  let shown = [1024, 128, 32, 16]
  let pad = 48, width = shown.reduce(0, +) + pad * (shown.count + 1), height = 1024 + 2 * pad
  let ctx = CGContext(data: nil, width: width, height: height * 2, bitsPerComponent: 8,
                      bytesPerRow: 0, space: CGColorSpace(name: CGColorSpace.sRGB)!,
                      bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
  for (row, gray) in [(1, 0.96), (0, 0.12)] {
    ctx.setFillColor(CGColor(gray: gray, alpha: 1))
    ctx.fill(CGRect(x: 0, y: row * height, width: width, height: height))
    var x = pad
    for size in shown {
      let y = row * height + (height - size) / 2
      ctx.draw(icon(size), in: CGRect(x: x, y: y, width: size, height: size))
      x += size + pad
    }
  }
  write(png(ctx.makeImage()!), URL(fileURLWithPath: args[3]))
}
