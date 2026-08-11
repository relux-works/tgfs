import SwiftUI

/// The authorization flow screen. Renders whatever state the agent reports
/// (phone → code → optional 2FA, or QR → optional 2FA) and forwards the
/// user's actions through the view model. It performs no Telegram operation
/// itself — it only asks the agent and shows the result, including the honest
/// "control channel unavailable" state when no agent channel exists yet.
public struct AuthorizationView: View {
    enum InputField: Hashable {
        case phoneNumber
        case code
        case password

        static func preferred(for state: CompanionAuthState) -> InputField? {
            switch state {
            case .waitPhoneNumber: return .phoneNumber
            case .waitCode: return .code
            case .waitPassword: return .password
            default: return nil
            }
        }
    }

    @Bindable private var model: AuthorizationViewModel
    @State private var phoneNumber = ""
    @State private var code = ""
    @State private var password = ""
    @FocusState private var focusedField: InputField?

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
                    Button(model.isCancelling ? "Cancelling…" : "Cancel", role: .cancel) {
                        Task { await model.cancel() }
                    }
                    .disabled(model.isCancelling)
                }
            }
        }
        .formStyle(.grouped)
        .navigationTitle("Sign In")
        .task(id: model.state.kind) {
            // Auth state branches are inserted after the parent view appears,
            // so `defaultFocus` alone is not sufficient on macOS. Yield once
            // for the native control to join the responder chain, then focus
            // the input for the reported state without a redundant write.
            await Task.yield()
            let preferred = InputField.preferred(for: model.state)
            if focusedField != preferred { focusedField = preferred }
        }
        .onChange(of: model.state.kind) { previous, current in
            if previous == "wait-password", current != "wait-password" {
                password.removeAll(keepingCapacity: false)
            }
        }
    }

    @ViewBuilder
    private var stateSection: some View {
        switch model.state {
        case .idle:
            Section { Text("Not signed in. Start to authorize a Telegram account.") }
        case .starting, .configuring:
            Section { ProgressView(model.isCancelling ? "Cancelling…" : "Connecting…") }
        case .waitPhoneNumber:
            Section("Phone number") {
                TextField("International format, e.g. +1 555 0100", text: $phoneNumber)
                    .focused($focusedField, equals: .phoneNumber)
                    .onSubmit(submitPhoneNumber)
                    .accessibilityLabel("Telegram phone number")
                Button("Send Code") {
                    submitPhoneNumber()
                }
                .disabled(phoneNumber.isEmpty || model.isSubmitting)
                Button("Use QR code instead") {
                    Task { await model.submit(.requestQrCode) }
                }
                .disabled(model.isSubmitting)
            }
            .defaultFocus($focusedField, .phoneNumber)
        case .waitCode(let info):
            Section("Enter code") {
                Text("Sent to \(info.phoneNumber).")
                    .foregroundStyle(.secondary)
                TextField("Login code", text: $code)
                    .focused($focusedField, equals: .code)
                    .onSubmit(submitCode)
                    .accessibilityLabel("Telegram login code")
                Button("Submit Code") { submitCode() }
                    .disabled(code.isEmpty || model.isSubmitting)
                Button("Resend code") { Task { await model.submit(.resendCode) } }
                    .disabled(model.isSubmitting)
            }
            .defaultFocus($focusedField, .code)
        case .waitQrConfirmation(let link):
            Section("Scan to sign in") {
                Text("Open Telegram on another logged-in device and scan this code.")
                    .foregroundStyle(.secondary)
                if let qrCode = TelegramLoginQRCode.image(for: link) {
                    Image(
                        qrCode,
                        scale: 1,
                        orientation: .up,
                        label: Text("Telegram sign-in QR code")
                    )
                    .interpolation(.none)
                    .resizable()
                    .scaledToFit()
                    .frame(width: 240, height: 240)
                    .accessibilityHint(
                        "Scan with Telegram on another logged-in device to continue signing in.")
                } else {
                    Label("The QR code could not be rendered.", systemImage: "qrcode")
                        .foregroundStyle(.red)
                }
                Text("The code refreshes automatically while this screen is open.")
                    .font(.callout)
                    .foregroundStyle(.secondary)
                Button("Use phone number instead") {
                    Task { await model.begin() }
                }
                .disabled(model.isSubmitting)
                .accessibilityHint("Ends QR sign-in and starts a fresh phone-number sign-in.")
            }
        case .waitPassword(let info):
            Section("Two-step password") {
                if !info.hint.isEmpty {
                    Text("Hint: \(info.hint)").foregroundStyle(.secondary)
                }
                SecureField("Password", text: $password)
                    .focused($focusedField, equals: .password)
                    .onSubmit(submitPassword)
                    .accessibilityLabel("Telegram two-step verification password")
                    .accessibilityHint("Enter the account password, then press Return to submit.")
                Button("Submit Password") {
                    submitPassword()
                }
                .disabled(password.isEmpty || model.isSubmitting)
            }
            .defaultFocus($focusedField, .password)
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

    private func submitPhoneNumber() {
        guard !phoneNumber.isEmpty, !model.isSubmitting else { return }
        Task { await model.submit(.submitPhoneNumber(phoneNumber)) }
    }

    private func submitCode() {
        guard !code.isEmpty, !model.isSubmitting else { return }
        Task { await model.submit(.submitCode(code)) }
    }

    private func submitPassword() {
        guard !password.isEmpty, !model.isSubmitting else { return }
        Task {
            await model.submit(.submitPassword(password))
            // Clicking the button moves key focus away from the field. Keep a
            // rejected password field ready so correction or retry needs no
            // extra click; Return submission already leaves focus in place.
            if model.state.kind == "wait-password", focusedField != .password {
                focusedField = .password
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
