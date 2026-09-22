//! seat 的稳定身份与共享准入归属；不包含管理端或存储细节。

use super::ClientApiKeyId;
use crate::validation::IdentifierError;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SeatId(String);

impl SeatId {
    pub fn new(value: impl Into<String>) -> Result<Self, IdentifierError> {
        let value = value.into();
        let suffix = value
            .strip_prefix("seat_")
            .ok_or(IdentifierError::MissingPrefix { expected: "seat_" })?;
        if suffix.len() != 32
            || !suffix
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(IdentifierError::InvalidFormat);
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ClientConcurrencyId {
    Key(ClientApiKeyId),
    Seat(SeatId),
}

impl ClientConcurrencyId {
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Key(id) => id.as_str(),
            Self::Seat(id) => id.as_str(),
        }
    }
}
