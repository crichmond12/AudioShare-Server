use base64;

#[derive(Debug)]
pub enum CustomError {
    AesGcm(aes_gcm::Error),
    Base64(base64::DecodeError),
    Utf8(std::string::FromUtf8Error),
    #[allow(dead_code)]
    Other(String),
}

impl std::fmt::Display for CustomError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CustomError::AesGcm(e) => write!(f, "AES-GCM error: {}", e),
            CustomError::Base64(e) => write!(f, "Base64 error: {}", e),
            CustomError::Utf8(e) => write!(f, "UTF-8 error: {}", e),
            CustomError::Other(e) => write!(f, "Other error: {}", e),
        }
    }
}

impl std::error::Error for CustomError {}

impl From<aes_gcm::Error> for CustomError {
    fn from(err: aes_gcm::Error) -> CustomError {
        CustomError::AesGcm(err)
    }
}

impl From<base64::DecodeError> for CustomError {
    fn from(err: base64::DecodeError) -> CustomError {
        CustomError::Base64(err)
    }
}

impl From<std::string::FromUtf8Error> for CustomError {
    fn from(err: std::string::FromUtf8Error) -> CustomError {
        CustomError::Utf8(err)
    }
}
