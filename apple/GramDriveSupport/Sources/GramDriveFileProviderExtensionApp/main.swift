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
// service. Package.swift sets the Mach-O entry point directly to Foundation's
// `_NSExtensionMain`, matching Xcode's App Extension product type. A normal
// Swift main must not call NSExtensionMain: doing so recursively re-enters the
// extension runtime and no File Provider callback is ever delivered.

// The principal class is instantiated by name at runtime, so nothing in this
// image references it and the linker would otherwise dead-strip it out of the
// extension binary — leaving `NSExtensionMain` unable to resolve the class.
// Touching the metatype keeps it in the linked image.
_ = GramDriveFileProviderExtension.self
