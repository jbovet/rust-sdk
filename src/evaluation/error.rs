// ============================================================
//  EvaluationError
// ============================================================

use typed_builder::TypedBuilder;

/// Struct representing error
#[derive(Clone, Eq, PartialEq, TypedBuilder, Debug)]
pub struct EvaluationError {
    /// The error code of abnormal evaluation.
    pub code: EvaluationErrorCode,

    /// The custom error message returned by the provider.
    #[builder(default, setter(strip_option, into))]
    pub message: Option<String>,
}

// ============================================================
//  EvaluationErrorCode
// ============================================================

/// An enumerated error code represented idiomatically in the implementation language.
#[derive(Clone, Eq, PartialEq, Debug)]
pub enum EvaluationErrorCode {
    /// The value was resolved before the provider was initialized.
    ProviderNotReady,

    /// The flag could not be found.
    FlagNotFound,

    /// An error was encountered parsing data, such as a flag configuration.
    ParseError,

    /// The type of the flag value does not match the expected type.
    TypeMismatch,

    /// The provider requires a targeting key and one was not provided in the evaluation context.
    TargetingKeyMissing,

    /// The evaluation context does not meet provider requirements.
    InvalidContext,

    /// The error was for a reason not enumerated above.
    General(String),
}

impl ToString for EvaluationErrorCode {
    fn to_string(&self) -> String {
        match self {
            Self::ProviderNotReady => "PROVIDER_NOT_READY".to_string(),
            Self::FlagNotFound => "FLAG_NOT_FOUND".to_string(),
            Self::ParseError => "PARSE_ERROR".to_string(),
            Self::TypeMismatch => "TYPE_MISMATCH".to_string(),
            Self::TargetingKeyMissing => "TARGETING_KEY_MISSING".to_string(),
            Self::InvalidContext => "INVALID_CONTEXT".to_string(),
            Self::General(message) => message.clone(),
        }
    }
}

// ============================================================
//  EvaluationError
// ============================================================
impl EvaluationError {
    /// Create a new evaluation error with default message from `EvaluationErrorCode`
    /// # Arguments
    /// * `code` - The evaluation error code for abnormal evaluation.
    pub fn new(code: EvaluationErrorCode) -> EvaluationError {
        Self::new_with_message(code, None)
    }
    /// Create a new evaluation error with a custom message.
    /// # Arguments
    /// * `code` - The evaluation error code for abnormal evaluation.
    /// * `message` - The custom error message returned by the provider (optional).
    pub fn new_with_message(code: EvaluationErrorCode, message: Option<String>) -> EvaluationError {
        let default_message = match &code {
            EvaluationErrorCode::ProviderNotReady => "The value was resolved before the provider was initialized.".to_string(),
            EvaluationErrorCode::FlagNotFound => "The flag could not be found.".to_string(),
            EvaluationErrorCode::ParseError => "An error was encountered parsing data, such as a flag configuration.".to_string(),
            EvaluationErrorCode::TypeMismatch => "The type of the flag value does not match the expected type.".to_string(),
            EvaluationErrorCode::TargetingKeyMissing => "The provider requires a targeting key and one was not provided in the evaluation context.".to_string(),
            EvaluationErrorCode::InvalidContext => "The evaluation context does not meet provider requirements.".to_string(),
            EvaluationErrorCode::General(message) =>  message.clone(),
        };

        EvaluationError {
            code,
            message: match message {
                Some(message) => Some(message),
                None => Some(default_message),
            },
        }
    }

    /// Get the error code.
    /// Returns the error code of abnormal evaluation.
    pub fn code(&self) -> &EvaluationErrorCode {
        &self.code
    }

    /// Get the error message.
    /// Returns `None` if no error message was provided.
    pub fn message(&self) -> Option<&str> {
        self.message.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use crate::{EvaluationError, EvaluationErrorCode};

    /// nothing change to keep compatibility with builder pattern
    #[test]
    fn test_evaluation_error_builder() {
        let error = EvaluationError::builder()
            .code(EvaluationErrorCode::ProviderNotReady)
            .message("No-op provider is never ready".to_string())
            .build();

        assert_eq!(error.code, EvaluationErrorCode::ProviderNotReady);
        assert_eq!(
            error.message,
            Some("No-op provider is never ready".to_string())
        );
    }

    #[test]
    fn test_new_with_code_evaluation_error() {
        let error = EvaluationError::new(EvaluationErrorCode::ProviderNotReady);

        assert_eq!(error.code, EvaluationErrorCode::ProviderNotReady);
        assert_eq!(
            error.message,
            Some("The value was resolved before the provider was initialized.".to_string())
        );

        let error = EvaluationError::new(EvaluationErrorCode::FlagNotFound);
        assert_eq!(error.code, EvaluationErrorCode::FlagNotFound);
        assert_eq!(
            error.message,
            Some("The flag could not be found.".to_string())
        );

        let error = EvaluationError::new(EvaluationErrorCode::ParseError);
        assert_eq!(error.code, EvaluationErrorCode::ParseError);
        assert_eq!(
            error.message,
            Some(
                "An error was encountered parsing data, such as a flag configuration.".to_string()
            )
        );

        let error = EvaluationError::new(EvaluationErrorCode::TypeMismatch);
        assert_eq!(error.code, EvaluationErrorCode::TypeMismatch);
        assert_eq!(
            error.message,
            Some("The type of the flag value does not match the expected type.".to_string())
        );

        let error = EvaluationError::new(EvaluationErrorCode::TargetingKeyMissing);
        assert_eq!(error.code, EvaluationErrorCode::TargetingKeyMissing);
        assert_eq!(
            error.message,
            Some("The provider requires a targeting key and one was not provided in the evaluation context.".to_string())
        );

        let error = EvaluationError::new(EvaluationErrorCode::InvalidContext);
        assert_eq!(error.code, EvaluationErrorCode::InvalidContext);
        assert_eq!(
            error.message,
            Some("The evaluation context does not meet provider requirements.".to_string())
        );

        let error = EvaluationError::new(EvaluationErrorCode::General("Custom error".to_string()));
        assert_eq!(
            error.code,
            EvaluationErrorCode::General("Custom error".to_string())
        );
        assert_eq!(error.message, Some("Custom error".to_string()));
    }

    #[test]
    fn test_new_with_message_evaluation_error() {
        let error = EvaluationError::new_with_message(
            EvaluationErrorCode::ProviderNotReady,
            Some("No-op provider is never ready".to_string()),
        );

        assert_eq!(error.code, EvaluationErrorCode::ProviderNotReady);
        assert_eq!(
            error.message,
            Some("No-op provider is never ready".to_string())
        );

        let error = EvaluationError::new_with_message(
            EvaluationErrorCode::FlagNotFound,
            Some("Flag not found".to_string()),
        );
        assert_eq!(error.code, EvaluationErrorCode::FlagNotFound);
        assert_eq!(error.message, Some("Flag not found".to_string()));

        let error = EvaluationError::new_with_message(
            EvaluationErrorCode::ParseError,
            Some("Parse error".to_string()),
        );
        assert_eq!(error.code, EvaluationErrorCode::ParseError);
        assert_eq!(error.message, Some("Parse error".to_string()));

        let error = EvaluationError::new_with_message(
            EvaluationErrorCode::TypeMismatch,
            Some("Type mismatch".to_string()),
        );
        assert_eq!(error.code, EvaluationErrorCode::TypeMismatch);
        assert_eq!(error.message, Some("Type mismatch".to_string()));

        let error = EvaluationError::new_with_message(
            EvaluationErrorCode::TargetingKeyMissing,
            Some("Targeting key missing".to_string()),
        );
        assert_eq!(error.code, EvaluationErrorCode::TargetingKeyMissing);
        assert_eq!(error.message, Some("Targeting key missing".to_string()));

        let error = EvaluationError::new_with_message(
            EvaluationErrorCode::InvalidContext,
            Some("Invalid context".to_string()),
        );
        assert_eq!(error.code, EvaluationErrorCode::InvalidContext);
        assert_eq!(error.message, Some("Invalid context".to_string()));

        let error = EvaluationError::new_with_message(
            EvaluationErrorCode::General("Custom error".to_string()),
            Some("Custom error message".to_string()),
        );
        assert_eq!(
            error.code,
            EvaluationErrorCode::General("Custom error".to_string())
        );
        assert_eq!(error.message, Some("Custom error message".to_string()));
    }
}
