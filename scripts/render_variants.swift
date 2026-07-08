#!/usr/bin/env swift
// scripts/render_variants.swift
//
// v1.0 Logo: Render the 4 logo variant SVGs (cool/warm/light/transparent) to
// PNG at 7 sizes (32/64/128/256/512/1024/2048) using macOS's native SVG
// renderer (NSImage + NSBitmapImageRep).
//
// Usage:
//   swift scripts/render_variants.swift
//
// Reads:  assets/logo/variants/{cool,warm,light,transparent}.svg
// Writes: assets/logo/variants/png/{variant}-{size}.png

import AppKit
import Foundation

func renderSVG(at svgPath: String, to pngPath: String, pixelSize: Int) {
    guard let svgData = FileManager.default.contents(atPath: svgPath) else {
        fputs("Error: cannot read SVG at \(svgPath)\n", stderr)
        exit(1)
    }
    // NSImage(data:) handles SVG natively on macOS (CGSVGDocument-backed rep).
    guard let svgImage = NSImage(data: svgData) else {
        fputs("Error: cannot create NSImage from SVG at \(svgPath)\n", stderr)
        exit(1)
    }

    // Force the layout size so the SVG rasterizes at the requested pixel size.
    let size = NSSize(width: pixelSize, height: pixelSize)
    svgImage.size = size

    guard let rep = NSBitmapImageRep(
        bitmapDataPlanes: nil,
        pixelsWide: pixelSize,
        pixelsHigh: pixelSize,
        bitsPerSample: 8,
        samplesPerPixel: 4,
        hasAlpha: true,
        isPlanar: false,
        colorSpaceName: .deviceRGB,
        bytesPerRow: 0,
        bitsPerPixel: 0
    ) else {
        fputs("Error: cannot create NSBitmapImageRep for size \(pixelSize)\n", stderr)
        exit(1)
    }
    rep.size = size

    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)

    // Clear to transparent — important for the Transparent variant.
    NSColor.clear.set()
    NSRect(x: 0, y: 0, width: pixelSize, height: pixelSize).fill()

    svgImage.draw(
        in: NSRect(x: 0, y: 0, width: pixelSize, height: pixelSize),
        from: .zero,
        operation: .sourceOver,
        fraction: 1.0
    )

    NSGraphicsContext.restoreGraphicsState()

    guard let pngData = rep.representation(using: .png, properties: [:]) else {
        fputs("Error: cannot get PNG data for \(pngPath)\n", stderr)
        exit(1)
    }

    do {
        try pngData.write(to: URL(fileURLWithPath: pngPath))
        print("  ✓ \(pngPath) (\(pngData.count) bytes)")
    } catch {
        fputs("Error writing PNG to \(pngPath): \(error)\n", stderr)
        exit(1)
    }
}

// Resolve project root: script is in <root>/scripts/, project root is parent.
let scriptDir = URL(fileURLWithPath: CommandLine.arguments[0])
    .deletingLastPathComponent()
    .standardizedFileURL
let projectRoot = scriptDir.deletingLastPathComponent()

let variantsDir = projectRoot.appendingPathComponent("assets/logo/variants")
let pngDir = variantsDir.appendingPathComponent("png")

guard FileManager.default.fileExists(atPath: variantsDir.path) else {
    fputs("Error: variants dir not found at \(variantsDir.path)\n", stderr)
    exit(1)
}

try? FileManager.default.createDirectory(at: pngDir, withIntermediateDirectories: true)

let variants = ["cool", "warm", "light", "transparent"]
// Sizes include 16 (required by iconutil for icon_16x16.png).
let sizes = [16, 32, 64, 128, 256, 512, 1024, 2048]

for variant in variants {
    let svgPath = variantsDir.appendingPathComponent("\(variant).svg").path
    guard FileManager.default.fileExists(atPath: svgPath) else {
        fputs("Error: SVG not found at \(svgPath)\n", stderr)
        exit(1)
    }
    print("→ Rendering \(variant)...")
    for size in sizes {
        let pngPath = pngDir.appendingPathComponent("\(variant)-\(size).png").path
        renderSVG(at: svgPath, to: pngPath, pixelSize: size)
    }
}

print("✓ All variants rendered to \(pngDir.path)")
