import GramDriveAgentCore
import SwiftUI

/// The first-launch Welcome window: a classic macOS onboarding wizard that
/// walks Welcome → Sign In → Choose Defaults → Success, all over the shared
/// ``OnboardingViewModel``. Presented once on a clean machine and re-runnable
/// from Help ▸ Setup Guide.
public struct OnboardingView: View {
  @Bindable private var model: OnboardingViewModel
  @Environment(\.dismiss) private var dismiss

  public init(model: OnboardingViewModel) {
    self.model = model
  }

  public var body: some View {
    VStack(spacing: 0) {
      stepContent
        .frame(maxWidth: .infinity, maxHeight: .infinity)
      Divider()
      footer
    }
    .frame(minWidth: 560, idealWidth: 560, minHeight: 560, idealHeight: 600)
    // The model owns presentation; when it dismisses (Done/Skip), close
    // the window it lives in.
    .onChange(of: model.isPresented) { _, presented in
      if !presented { dismiss() }
    }
  }

  @ViewBuilder
  private var stepContent: some View {
    switch model.step {
    case .welcome:
      OnboardingWelcomeStep()
    case .signIn:
      OnboardingSignInStep(model: model)
    case .defaults:
      OnboardingDefaultsStep(model: model.settings)
    case .success:
      OnboardingSuccessStep(model: model)
    }
  }

  private var footer: some View {
    HStack(spacing: 12) {
      if !model.isFirstStep {
        Button("Back") { model.back() }
      }
      StepProgressDots(step: model.step)
      Spacer()
      if !model.isLastStep {
        Button("Skip Setup") { model.skip() }
          .buttonStyle(.plain)
          .foregroundStyle(.secondary)
      }
      if !model.canAdvance {
        primaryActionButton
      } else {
        primaryActionButton
          .keyboardShortcut(.defaultAction)
      }
    }
    .padding(16)
  }

  private var primaryActionButton: some View {
    Button(model.primaryActionTitle) { model.advance() }
      .disabled(!model.canAdvance)
  }
}

/// The wizard's step indicator — one dot per step, the current one filled.
private struct StepProgressDots: View {
  let step: OnboardingViewModel.Step

  var body: some View {
    HStack(spacing: 6) {
      ForEach(OnboardingViewModel.Step.allCases) { each in
        Circle()
          .fill(each == step ? Color.accentColor : Color.secondary.opacity(0.35))
          .frame(width: 7, height: 7)
      }
    }
    .accessibilityLabel("Step \(step.rawValue + 1) of \(OnboardingViewModel.Step.allCases.count)")
  }
}

/// A shared header for a step: a large glyph, a title, and a subtitle.
private struct OnboardingStepHeader: View {
  let systemImage: String
  let tint: Color
  let title: String
  let subtitle: String

  init(systemImage: String, tint: Color = .accentColor, title: String, subtitle: String) {
    self.systemImage = systemImage
    self.tint = tint
    self.title = title
    self.subtitle = subtitle
  }

  var body: some View {
    VStack(spacing: 10) {
      Image(systemName: systemImage)
        .font(.system(size: 52))
        .foregroundStyle(tint)
        .accessibilityHidden(true)
      Text(title)
        .font(.title.bold())
        .multilineTextAlignment(.center)
      Text(subtitle)
        .font(.title3)
        .foregroundStyle(.secondary)
        .multilineTextAlignment(.center)
    }
  }
}

// MARK: - Welcome

private struct OnboardingWelcomeStep: View {
  var body: some View {
    VStack(spacing: 24) {
      Spacer(minLength: 0)
      OnboardingStepHeader(
        systemImage: "externaldrive.badge.person.crop",
        title: "Welcome to GramDrive",
        subtitle: "Your Telegram chats as folders in Finder.")
      VStack(alignment: .leading, spacing: 14) {
        WelcomePoint(
          systemImage: "key.fill",
          text: "Sign in to Telegram to connect your account.")
        WelcomePoint(
          systemImage: "slider.horizontal.3",
          text: "Pick your defaults — cache size and startup. Mirror retention starts by default.")
        WelcomePoint(
          systemImage: "folder.fill",
          text: "Open your chats as folders in Finder.")
      }
      .padding(.horizontal, 24)
      Text(
        "Chats stay in the cloud and download when you open them. "
          + "You can switch on Archive Mode later to eagerly keep allowed persistent content offline."
      )
      .font(.callout)
      .foregroundStyle(.secondary)
      .multilineTextAlignment(.center)
      .padding(.horizontal, 32)
      Spacer(minLength: 0)
    }
    .padding(32)
  }
}

private struct WelcomePoint: View {
  let systemImage: String
  let text: String

  var body: some View {
    HStack(alignment: .firstTextBaseline, spacing: 12) {
      Image(systemName: systemImage)
        .foregroundStyle(.tint)
        .frame(width: 22)
        .accessibilityHidden(true)
      Text(text)
      Spacer(minLength: 0)
    }
  }
}

// MARK: - Sign In

private struct OnboardingSignInStep: View {
  @Bindable var model: OnboardingViewModel

  var body: some View {
    VStack(spacing: 16) {
      OnboardingStepHeader(
        systemImage: "key.fill",
        title: "Sign in to Telegram",
        subtitle: "GramDrive signs in through its background agent — "
          + "your credentials never touch Finder."
      )
      .padding(.top, 28)
      AuthorizationView(model: model.authorization)
        .frame(maxHeight: .infinity)
    }
    .task { await model.beginSignInIfNeeded() }
  }
}

// MARK: - Defaults

private struct OnboardingDefaultsStep: View {
  @Bindable var model: CompanionSettingsViewModel

  var body: some View {
    VStack(spacing: 12) {
      OnboardingStepHeader(
        systemImage: "slider.horizontal.3",
        title: "Choose your defaults",
        subtitle: "You can change any of these later in GramDrive settings."
      )
      .padding(.top, 28)
      Form {
        Section("Managed cache") {
          Stepper(
            "Cache up to \(Int(model.cacheQuotaGigabytes.rounded())) GB",
            value: $model.cacheQuotaGigabytes, in: 1...1000, step: 1)
          Text(
            "Opened chats are cached up to this size and evicted "
              + "least-recently-used. Pinned items are always kept."
          )
          .font(.callout)
          .foregroundStyle(.secondary)
        }
        Section("Retention & Archive") {
          Text(
            "Each authorized account starts in Mirror, reflecting current observed Telegram state. "
              + "Audit is prospective and cannot recover past or unseen content. "
              + "Archive Mode remains a separate per-account setting with a disk-space check; "
              + "enable it later in Storage & Offline."
          )
          .font(.callout)
          .foregroundStyle(.secondary)
        }
        Section("Startup") {
          Toggle(
            "Launch GramDrive at login",
            isOn: Binding(
              get: { model.launchAtLogin },
              set: { model.applyLaunchAtLogin($0) }))
          if model.lastLaunchAction == .awaitingApproval {
            Label(
              "Approve GramDrive in System Settings › General › Login Items.",
              systemImage: "hand.raised"
            )
            .font(.callout)
            .foregroundStyle(.secondary)
          }
        }
      }
      .formStyle(.grouped)
    }
    .onAppear { model.load() }
  }
}

// MARK: - Success

private struct OnboardingSuccessStep: View {
  @Bindable var model: OnboardingViewModel

  var body: some View {
    VStack(spacing: 22) {
      Spacer(minLength: 0)
      setupContent
      Spacer(minLength: 0)
    }
    .padding(32)
    // Reconcile again after authorization. The launch pass may have run
    // before the account row existed; this pass is the explicit
    // authorized-account → Finder-domain transition.
    .task {
      await model.prepareFileProviderDomain()
      while !Task.isCancelled {
        await model.refreshStatus()
        try? await Task.sleep(for: .seconds(2))
      }
    }
  }

  @ViewBuilder
  private var setupContent: some View {
    switch model.domainSetupState {
    case .idle, .settingUp:
      OnboardingStepHeader(
        systemImage: "externaldrive.badge.timemachine",
        title: "Setting up GramDrive",
        subtitle: "Creating your Finder location…")
      ProgressView()
        .controlSize(.small)
        .accessibilityLabel("Creating the GramDrive Finder location")
    case .failed:
      OnboardingStepHeader(
        systemImage: "exclamationmark.triangle.fill",
        tint: .orange,
        title: "Finder setup needs attention",
        subtitle: "GramDrive couldn't add its Finder location.")
      Text(
        "Confirm GramDrive is installed in Applications and its File Provider "
          + "extension is enabled in System Settings, then try again."
      )
      .font(.callout)
      .foregroundStyle(.secondary)
      .multilineTextAlignment(.center)
      .padding(.horizontal, 24)
      Button("Retry") {
        Task { await model.prepareFileProviderDomain() }
      }
      .keyboardShortcut(.defaultAction)
    case .ready:
      OnboardingStepHeader(
        systemImage: "checkmark.circle.fill",
        tint: .green,
        title: "You're all set",
        subtitle: "GramDrive is live in Finder under Locations.")
      Button {
        model.openDriveInFinder()
      } label: {
        Label("Open in Finder", systemImage: "folder")
      }
      .controlSize(.large)

      syncStatus
        .padding(.top, 4)
    }
  }

  private var syncStatus: some View {
    let sync = model.initialSync
    return HStack(spacing: 8) {
      Image(systemName: sync.isActive ? "clock" : "checkmark.circle.fill")
        .foregroundStyle(sync.isActive ? Color.secondary : Color.green)
      Text(sync.label)
        .font(.callout)
        .foregroundStyle(.secondary)
    }
  }
}

#if DEBUG
  @MainActor
  private func previewOnboardingModel(
    health: HealthReadout = .notRunning,
    driveURL: URL? = URL(fileURLWithPath: "/Users/preview/Library/CloudStorage/GramDrive")
  ) -> OnboardingViewModel {
    let backend = InMemoryCompanionBackend(health: health)
    let vm = CompanionViewModel(
      backend: backend,
      diskProbe: FixedDiskSpaceProbe(available: 500_000_000_000),
      accountLabel: "Preview account",
      driveLocation: FixedDriveLocation(url: driveURL),
      domainSetup: FixedFileProviderDomainSetup(
        rootURL: driveURL
          ?? URL(fileURLWithPath: "/Users/preview/Library/CloudStorage/GramDrive")),
      onboardingStore: InMemoryOnboardingCompletionStore())
    return vm.onboarding
  }

  #Preview("Onboarding — Welcome") {
    OnboardingView(model: previewOnboardingModel())
  }

  #Preview("Onboarding — Success syncing") {
    let model = previewOnboardingModel(
      health: .running(previewSnapshot(recentEvents: ["started"])))
    model.advance()  // welcome → sign-in
    return OnboardingView(model: model)
  }
#endif
