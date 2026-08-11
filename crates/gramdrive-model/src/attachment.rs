//! Shared attachment representation/fidelity truthfulness contract.
//!
//! Telegram message representation is independent from logical media kind,
//! but it constrains what the archive may truthfully claim about the bytes.
//! This module owns that cross-layer constraint so durable state, renderer
//! inputs, and native metadata boundaries cannot drift into different rules.

/// Why an attachment representation/fidelity claim is not truthful.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachmentContractError {
    /// The representation or fidelity vocabulary token is empty.
    EmptyVocabulary,
    /// A present source filename is empty text.
    EmptySourceName,
    /// A Telegram-processed representation claimed a sender source filename.
    ProcessedRepresentationHasSourceName,
    /// A Telegram-processed representation claimed unsupported fidelity.
    ProcessedRepresentationHasInvalidFidelity,
    /// An original document claimed fidelity that does not describe it.
    OriginalDocumentHasInvalidFidelity,
    /// A legacy representation was paired with non-legacy fidelity.
    LegacyRepresentationHasInvalidFidelity,
}

impl std::fmt::Display for AttachmentContractError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let detail = match self {
            Self::EmptyVocabulary => "attachment representation and fidelity must not be empty",
            Self::EmptySourceName => "attachment source name must not be empty text",
            Self::ProcessedRepresentationHasSourceName => {
                "Telegram-processed media must not claim a sender source filename"
            }
            Self::ProcessedRepresentationHasInvalidFidelity => {
                "Telegram-processed media must be a Telegram variant or metadata-only"
            }
            Self::OriginalDocumentHasInvalidFidelity => {
                "an original document must be original or metadata-only"
            }
            Self::LegacyRepresentationHasInvalidFidelity => {
                "an unknown legacy representation must use unknown legacy fidelity"
            }
        };
        formatter.write_str(detail)
    }
}

impl std::error::Error for AttachmentContractError {}

/// Validates one attachment representation/fidelity/source-name tuple.
///
/// Known Telegram-processed representations never expose a sender filename
/// and may claim only downloaded Telegram-variant bytes or metadata without
/// bytes. Original documents may be exact originals or metadata-only.
/// `unknown_legacy` remains readable for forward-only schema migrations.
/// Unknown future representation tokens are accepted so an older core can
/// preserve new source vocabulary without inventing semantics for it.
pub fn validate_attachment_contract(
    representation: &str,
    fidelity: &str,
    source_name: Option<&str>,
) -> Result<(), AttachmentContractError> {
    if representation.is_empty() || fidelity.is_empty() {
        return Err(AttachmentContractError::EmptyVocabulary);
    }
    if source_name == Some("") {
        return Err(AttachmentContractError::EmptySourceName);
    }

    match representation {
        "message_photo" | "message_video" | "message_animation" | "message_audio"
        | "message_voice" | "message_video_note" | "message_sticker" => {
            if source_name.is_some() {
                return Err(AttachmentContractError::ProcessedRepresentationHasSourceName);
            }
            if !matches!(fidelity, "telegram_variant" | "metadata_only") {
                return Err(AttachmentContractError::ProcessedRepresentationHasInvalidFidelity);
            }
        }
        "original_document" if !matches!(fidelity, "original" | "metadata_only") => {
            return Err(AttachmentContractError::OriginalDocumentHasInvalidFidelity);
        }
        "unknown_legacy" if fidelity != "unknown_legacy" => {
            return Err(AttachmentContractError::LegacyRepresentationHasInvalidFidelity);
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_truthful_known_and_forward_compatible_claims() {
        for (representation, fidelity, source_name) in [
            ("original_document", "original", Some("sender.jpg")),
            ("original_document", "metadata_only", None),
            ("message_photo", "telegram_variant", None),
            ("message_video", "metadata_only", None),
            ("unknown_legacy", "unknown_legacy", Some("legacy.bin")),
            (
                "future_representation",
                "future_fidelity",
                Some("future.bin"),
            ),
        ] {
            assert_eq!(
                validate_attachment_contract(representation, fidelity, source_name),
                Ok(())
            );
        }
    }

    #[test]
    fn rejects_false_processed_and_legacy_claims() {
        assert_eq!(
            validate_attachment_contract("message_photo", "original", None),
            Err(AttachmentContractError::ProcessedRepresentationHasInvalidFidelity)
        );
        assert_eq!(
            validate_attachment_contract("message_video", "telegram_variant", Some("claim.mp4")),
            Err(AttachmentContractError::ProcessedRepresentationHasSourceName)
        );
        assert_eq!(
            validate_attachment_contract("original_document", "telegram_variant", None),
            Err(AttachmentContractError::OriginalDocumentHasInvalidFidelity)
        );
        assert_eq!(
            validate_attachment_contract("unknown_legacy", "original", None),
            Err(AttachmentContractError::LegacyRepresentationHasInvalidFidelity)
        );
    }
}
