import Foundation
import GramDriveFileProvider

// The File Provider extension's executable entry point (TASK-260715-1dk9ik).
//
// SwiftPM cannot emit an `.appex`, so packaging assembles the bundle by hand
// (.scripts/apple-app/build_app_bundle.py) around this binary: it becomes
// `GramDriveFileProvider.appex/Contents/MacOS/GramDriveFileProvider`, wrapped
// in an Info.plist whose `NSExtension` dictionary points
// `NSExtensionPrincipalClass` at `GramDriveFileProvider.GramDriveFileProviderExtension`
// (the Swift-mangled Objective-C runtime name of the class defined in the
// GramDriveFileProvider module).
//
// The system's `fileproviderd` launches this binary as the extension's XPC
// service. `NSExtensionMain` — exported from Foundation but absent from its
// Swift overlay, so declared here — reads that `NSExtension` dictionary,
// instantiates the principal class through its `init(domain:)`, and drives the
// `NSFileProviderReplicatedExtension` callbacks. It never returns.

/// Foundation's C entry point for an `NSExtension`-based host. Declared by its
/// C symbol because the Swift Foundation overlay does not surface it.
@_silgen_name("NSExtensionMain")
func gramDriveNSExtensionMain() -> CInt

// The principal class is instantiated by name at runtime, so nothing in this
// image references it and the linker would otherwise dead-strip it out of the
// extension binary — leaving `NSExtensionMain` unable to resolve the class.
// Touching the metatype keeps it in the linked image.
_ = GramDriveFileProviderExtension.self

exit(gramDriveNSExtensionMain())
