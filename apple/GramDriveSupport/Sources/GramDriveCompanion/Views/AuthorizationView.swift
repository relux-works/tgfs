import SwiftUI

/// The authorization flow screen. Renders whatever state the agent reports
/// (phone → code → optional 2FA, or QR → optional 2FA) and forwards the
/// user's actions through the view model. It performs no Telegram operation
/// itself — it only asks the agent and shows the result, including the honest
/// "control channel unavailable" state when no agent channel exists yet.
public struct AuthorizationView: View {
    @Bindable private var model: AuthorizationViewModel
    @State private var phoneNumber = ""
    @State private var code = ""
    @State private var password = ""

    public init(model: AuthorizationViewModel) {
        self.model = model
    }

    public var body: some View {
        Form {
            if let unavailable = model.unavailable {
                Section {
                    Label(unavailable.message, systemImage: "exclamationmark.triangle")
                        .foregroundStyle(.secondary)
                }
            } else {
                stateSection
                if let rejection = model.lastRejection {
                    rejectionSection(rejection)
                }
                if let invalid = model.lastInvalidInput {
                    Section {
                        Label(
                            "That action isn't available right now (\(invalid.kind)).",
                            systemImage: "hand.raised")
                        .foregroundStyle(.secondary)
                    }
                }
            }
            Section {
                Button(model.state == .idle ? "Start Sign In" : "Restart") {
                    Task { await model.begin() }
                }
                .disabled(model.isSubmitting)
                if model.state.acceptsInput {
                    Button("Cancel", role: .cancel) { Task { await model.cancel() } }
                }
            }
        }
        .formStyle(.grouped)
        .navigationTitle("Sign In")
    }

    @ViewBuilder
    private var stateSection: some View {
        switch model.state {
        case .idle:
            Section { Text("Not signed in. Start to authorize a Telegram account.") }
        case .starting, .configuring:
            Section { ProgressView("Connecting…") }
        case .waitPhoneNumber:
            Section("Phone number") {
                TextField("International format, e.g. +1 555 0100", text: $phoneNumber)
                Button("Send Code") {
                    Task { await model.submit(.submitPhoneNumber(phoneNumber)) }
                }
                .disabled(phoneNumber.isEmpty || model.isSubmitting)
                Button("Use QR code instead") {
                    Task { await model.submit(.requestQrCode) }
                }
            }
        case .waitCode(let info):
            Section("Enter code") {
                Text("Sent to \(info.phoneNumber).")
                    .foregroundStyle(.secondary)
                TextField("Login code", text: $code)
                Button("Submit Code") { Task { await model.submit(.submitCode(code)) } }
                    .disabled(code.isEmpty || model.isSubmitting)
                Button("Resend code") { Task { await model.submit(.resendCode) } }
            }
        case .waitQrConfirmation(let link):
            Section("Scan to sign in") {
                Text("Open Telegram on another device and scan this link:")
                    .foregroundStyle(.secondary)
                Text(link).font(.callout.monospaced()).textSelection(.enabled)
            }
        case .waitPassword(let info):
            Section("Two-step password") {
                if !info.hint.isEmpty {
                    Text("Hint: \(info.hint)").foregroundStyle(.secondary)
                }
                SecureField("Password", text: $password)
                Button("Submit Password") {
                    Task { await model.submit(.submitPassword(password)) }
                }
                .disabled(password.isEmpty || model.isSubmitting)
            }
        case .ready:
            Section {
                Label("Signed in.", systemImage: "checkmark.seal.fill")
                    .foregroundStyle(.green)
            }
        case .loggingOut:
            Section { ProgressView("Logging out…") }
        case .closing, .closed:
            Section { Text("The sign-in session ended.") }
        case .unsupported(let kind):
            Section {
                Label(
                    "This sign-in step isn't supported in this version (\(kind)). "
                        + "You can cancel and try again.",
                    systemImage: "questionmark.circle")
                .foregroundStyle(.secondary)
            }
        case .failed(let detail):
            Section {
                Label(
                    "Signing in succeeded but the account could not be saved "
                        + "(\(detail)). Please try signing in again.",
                    systemImage: "exclamationmark.triangle")
                .foregroundStyle(.orange)
            }
        }
    }

    @ViewBuilder
    private func rejectionSection(_ rejection: CompanionAuthRejection) -> some View {
        Section {
            Label(rejection.message, systemImage: "xmark.octagon")
                .foregroundStyle(.red)
            Text(adviceText(rejection.advice))
                .font(.callout)
                .foregroundStyle(.secondary)
        }
    }

    private func adviceText(_ advice: CompanionRetryAdvice) -> String {
        switch advice {
        case .retrySameInput: return "Try the same thing again."
        case .reviseInput: return "Correct the value and resubmit."
        case .requestNewCode: return "Request a new code, then enter it."
        case .waitThenRetry(let after):
            if let after { return "Wait \(after)s, then try again." }
            return "Wait a moment, then try again."
        case .abort: return "This can't be retried here."
        }
    }
}

#if DEBUG
#Preview("Authorization — wait code") {
    let session = ScriptedAuthorizationSession()
    let backend = InMemoryCompanionBackend(session: { session })
    let model = AuthorizationViewModel(backend: backend)
    session.emit(.waitCode(CompanionCodeInfo(phoneNumber: "+1 555 0100", codeLength: 5)))
    return AuthorizationView(model: model)
        .task { await model.begin() }
}
#endif
