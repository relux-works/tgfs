import Foundation
import GramDriveAgentCore
import Testing

@testable import GramDriveCompanion

/// Hand-driven login-item service for the launch-at-login reconcile.
private final class FakeLoginItemService: LoginItemService {
    var status: LoginItemStatus
    private(set) var registerCalls = 0

    init(status: LoginItemStatus) { self.status = status }

    func register() throws {
        registerCalls += 1
        status = .requiresApproval
    }

    func unregister() throws { status = .notRegistered }
}

private struct SaveFailure: Error {}

@MainActor
@Suite struct SettingsViewModelTests {
    private func model(
        backend: InMemoryCompanionBackend,
        login: (any LoginItemService)? = nil,
        available: UInt64? = 500_000_000_000
    ) -> CompanionSettingsViewModel {
        CompanionSettingsViewModel(
            backend: backend,
            loginItemService: login,
            diskProbe: FixedDiskSpaceProbe(available: available))
    }

    @Test func loadSeedsEditableFieldsFromTheBackend() {
        let backend = InMemoryCompanionBackend(
            settings: AgentSettings(
                launchAtLogin: true, cacheQuotaBytes: 25_000_000_000, archiveModeEnabled: true))
        let vm = model(backend: backend)
        vm.load()
        #expect(vm.launchAtLogin == true)
        #expect(vm.cacheQuotaBytes == 25_000_000_000)
        #expect(vm.archiveModeEnabled == true)
    }

    @Test func saveWritesEditedSettingsThroughTheSeam() {
        let backend = InMemoryCompanionBackend()
        let vm = model(backend: backend)
        vm.cacheQuotaBytes = 50_000_000_000
        vm.archiveModeEnabled = true
        #expect(vm.save())
        #expect(backend.storedSettings.cacheQuotaBytes == 50_000_000_000)
        #expect(backend.storedSettings.archiveModeEnabled == true)
    }

    @Test func aSaveFailureIsSurfacedNotSwallowed() {
        let backend = InMemoryCompanionBackend(saveError: SaveFailure())
        let vm = model(backend: backend)
        #expect(!vm.save())
        #expect(vm.lastError != nil)
    }

    @Test func gigabyteBindingIsBaseTen() {
        let backend = InMemoryCompanionBackend()
        let vm = model(backend: backend)
        vm.cacheQuotaGigabytes = 10
        #expect(vm.cacheQuotaBytes == 10_000_000_000)
        #expect(vm.cacheQuotaGigabytes == 10)
    }

    @Test func archiveModePreflightFitsWhenDiskIsAmple() {
        let vm = model(backend: InMemoryCompanionBackend(), available: 500_000_000_000)
        let preflight = vm.archiveModePreflight(estimatedArchiveBytes: 25_000_000_000)
        #expect(preflight == .ok(projectedBytes: 25_000_000_000, availableBytes: 500_000_000_000))
        #expect(!preflight.isLowDisk)
    }

    @Test func archiveModePreflightWarnsWhenDiskIsTight() {
        // 25 GB scope + 2 GB buffer against 20 GB free — will not fit.
        let vm = model(backend: InMemoryCompanionBackend(), available: 20_000_000_000)
        let preflight = vm.archiveModePreflight(estimatedArchiveBytes: 25_000_000_000)
        #expect(preflight.isLowDisk)
        #expect(preflight.projectedBytes == 25_000_000_000)
    }

    @Test func archiveModePreflightIsHonestWhenCapacityIsUnknown() {
        let vm = model(backend: InMemoryCompanionBackend(), available: nil)
        let preflight = vm.archiveModePreflight(estimatedArchiveBytes: 25_000_000_000)
        #expect(preflight == .ok(projectedBytes: 25_000_000_000, availableBytes: nil))
    }

    @Test func launchAtLoginReconcileSurfacesAwaitingApproval() {
        let login = FakeLoginItemService(status: .notRegistered)
        let vm = model(backend: InMemoryCompanionBackend(), login: login)
        let action = vm.applyLaunchAtLogin(true)
        #expect(action == .awaitingApproval)
        #expect(vm.launchAtLogin == true)
        #expect(login.registerCalls == 1)
    }

    @Test func launchAtLoginWithoutAServiceStillUpdatesThePreference() {
        let vm = model(backend: InMemoryCompanionBackend(), login: nil)
        let action = vm.applyLaunchAtLogin(true)
        #expect(action == nil)
        #expect(vm.launchAtLogin == true)
    }
}
