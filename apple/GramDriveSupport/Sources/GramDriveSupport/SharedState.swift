import Foundation
import GramDriveCore

/// Process-role entry points to the shared durable state.
///
/// Thin by design: the multi-process rules — WAL-only open, role rights,
/// short snapshot reads, migration-on-open, coordinator-only recovery —
/// live in the core (`gramdrive-ffi/src/shared_state.rs`); this type only
/// binds them to the App Group container and `URL`-shaped call sites.
public enum SharedState {
    /// Opens the shared state in the App Group container, for product
    /// processes. `role` follows the process: the engine host opens as
    /// `.coordinator`, the File Provider extension and UI surfaces as
    /// `.provider`.
    public static func openInAppGroupContainer(
        role: StateRole,
        fileManager: FileManager = .default
    ) throws -> SharedStateStore {
        try open(
            dataRoot: AppGroup.dataRootURL(
                containerURL: AppGroup.containerURL(fileManager: fileManager)
            ),
            role: role
        )
    }

    /// Opens the shared state under an explicit data root — tests, tools,
    /// and the smoke harness, which substitute a container of their own.
    public static func open(dataRoot: URL, role: StateRole) throws -> SharedStateStore {
        try SharedStateStore.open(dataRoot: dataRoot.path, role: role)
    }

    /// The canonical layout under an explicit data root, without opening.
    public static func layout(dataRoot: URL) throws -> SharedStateLayout {
        try sharedStateLayout(dataRoot: dataRoot.path)
    }
}
