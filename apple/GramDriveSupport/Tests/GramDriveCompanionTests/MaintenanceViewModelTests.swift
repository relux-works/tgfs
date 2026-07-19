import Foundation
import Testing

@testable import GramDriveCompanion

@MainActor
@Suite struct RepairViewModelTests {
    @Test func aCompletedRepairSucceeds() async {
        let vm = RepairViewModel(backend: InMemoryCompanionBackend(repairOutcome: .completed))
        await vm.repair()
        #expect(vm.phase == .succeeded)
    }

    @Test func anUnwiredChannelIsSurfacedHonestly() async {
        let vm = RepairViewModel(
            backend: InMemoryCompanionBackend(repairOutcome: .unavailable(.notWired)))
        await vm.repair()
        #expect(vm.phase == .unavailable(.notWired))
    }

    @Test func aFailedRepairIsClassified() async {
        let vm = RepairViewModel(
            backend: InMemoryCompanionBackend(repairOutcome: .failed(.storage)))
        await vm.repair()
        #expect(vm.phase == .failed(.storage))
    }
}

@MainActor
@Suite struct AccountRemovalViewModelTests {
    private func vm(outcome: CommandOutcome = .completed) -> AccountRemovalViewModel {
        AccountRemovalViewModel(
            backend: InMemoryCompanionBackend(removalOutcome: outcome),
            accountLabel: "My Account")
    }

    @Test func removalIsRefusedLocallyWithoutAValidConfirmation() async {
        let model = vm(outcome: .completed)  // backend would succeed if reached
        model.acknowledgedIrreversible = false
        model.typedConfirmation = "My Account"
        await model.remove()
        // The mismatch is caught before any command is issued.
        #expect(model.phase == .invalidConfirmation)
    }

    @Test func removalIsRefusedWhenTheTypedLabelDoesNotMatch() async {
        let model = vm(outcome: .completed)
        model.acknowledgedIrreversible = true
        model.typedConfirmation = "Wrong"
        await model.remove()
        #expect(model.phase == .invalidConfirmation)
    }

    @Test func aValidConfirmationRemovesTheAccount() async {
        let model = vm(outcome: .completed)
        model.acknowledgedIrreversible = true
        model.typedConfirmation = "  my account  "  // trimmed + case-insensitive
        #expect(model.canRemove)
        await model.remove()
        #expect(model.phase == .removed)
    }

    @Test func anUnwiredChannelIsSurfacedHonestly() async {
        let model = vm(outcome: .unavailable(.notWired))
        model.acknowledgedIrreversible = true
        model.typedConfirmation = "My Account"
        await model.remove()
        #expect(model.phase == .unavailable(.notWired))
    }

    @Test func aFailedRemovalIsClassified() async {
        let model = vm(outcome: .failed(.sourceUnavailable))
        model.acknowledgedIrreversible = true
        model.typedConfirmation = "My Account"
        await model.remove()
        #expect(model.phase == .failed(.sourceUnavailable))
    }
}

@Suite struct RemovalConfirmationTests {
    @Test func validRequiresMatchAndAcknowledgement() {
        #expect(
            RemovalConfirmation(
                accountLabel: "Acme", typedConfirmation: "acme",
                acknowledgedIrreversible: true
            ).isValid)
        #expect(
            !RemovalConfirmation(
                accountLabel: "Acme", typedConfirmation: "acme",
                acknowledgedIrreversible: false
            ).isValid)
        #expect(
            !RemovalConfirmation(
                accountLabel: "Acme", typedConfirmation: "nope",
                acknowledgedIrreversible: true
            ).isValid)
        // An empty label cannot be satisfied by an empty typed string.
        #expect(
            !RemovalConfirmation(
                accountLabel: "  ", typedConfirmation: "  ",
                acknowledgedIrreversible: true
            ).isValid)
    }
}
