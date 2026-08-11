import CoreGraphics
import CoreImage
import CoreImage.CIFilterBuiltins
import Foundation

/// Generates the QR image TDLib asks another logged-in Telegram client to
/// scan. The payload remains transient: callers supply the current rotating
/// link and receive pixels; this type does not log or persist either one.
enum TelegramLoginQRCode {
    private static let quietZoneModules = 4
    private static let pixelsPerModule = 8

    /// A sharp, square QR code with the ISO-recommended four-module quiet zone.
    /// Integer scaling is deliberate: interpolation can make a valid QR image
    /// difficult for a camera to decode.
    static func image(for link: String) -> CGImage? {
        guard !link.isEmpty, let message = link.data(using: .utf8) else { return nil }

        let filter = CIFilter.qrCodeGenerator()
        filter.message = message
        filter.correctionLevel = "M"
        guard let code = filter.outputImage else { return nil }

        let codeExtent = code.extent.integral
        let paddedExtent = codeExtent.insetBy(
            dx: -CGFloat(quietZoneModules),
            dy: -CGFloat(quietZoneModules))
        let background = CIImage(color: .white).cropped(to: paddedExtent)
        let paddedCode = code.composited(over: background)
        let scaledCode = paddedCode.transformed(
            by: CGAffineTransform(
                scaleX: CGFloat(pixelsPerModule),
                y: CGFloat(pixelsPerModule)))

        return CIContext(options: [.useSoftwareRenderer: false]).createCGImage(
            scaledCode,
            from: scaledCode.extent.integral)
    }
}
