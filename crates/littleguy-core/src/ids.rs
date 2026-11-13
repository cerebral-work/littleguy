//! Newtypes for every id that crosses a boundary.
//!
//! Each constructor checks its text once, so code that holds an id never
//! checks it again.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// The longest id in bytes. It bounds what a client can make the daemon hold.
pub const MAX_ID_BYTES: usize = 128;

/// The longest pool delivery token in bytes. `JetStream` ack tokens exceed 128 bytes.
pub const MAX_DELIVERY_TOKEN_BYTES: usize = 512;

/// Why a text is not a valid id.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum IdError {
    /// The text is empty.
    #[error("the {kind} is empty")]
    Empty {
        /// The id type.
        kind: &'static str,
    },
    /// The text is longer than [`MAX_ID_BYTES`].
    #[error("the {kind} is longer than {MAX_ID_BYTES} bytes")]
    TooLong {
        /// The id type.
        kind: &'static str,
    },
    /// The delivery token is longer than [`MAX_DELIVERY_TOKEN_BYTES`].
    #[error("the {kind} is longer than {MAX_DELIVERY_TOKEN_BYTES} bytes")]
    TokenTooLong {
        /// The id type.
        kind: &'static str,
    },
    /// The text holds a byte that is not printable ASCII.
    #[error("the {kind} holds a byte that is not printable ASCII")]
    NotPrintable {
        /// The id type.
        kind: &'static str,
    },
}

/// Checks the shape that every id shares: printable ASCII, not empty, bounded.
fn check_with(
    kind: &'static str,
    max: usize,
    too_long: fn(&'static str) -> IdError,
    text: &str,
) -> Result<(), IdError> {
    if text.is_empty() {
        return Err(IdError::Empty { kind });
    }
    if text.len() > max {
        return Err(too_long(kind));
    }
    if !text.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err(IdError::NotPrintable { kind });
    }
    Ok(())
}

macro_rules! id_type {
    ($(#[$doc:meta])* $name:ident, $kind:literal) => {
        id_type!($(#[$doc])* $name, $kind, MAX_ID_BYTES, TooLong);
    };
    ($(#[$doc:meta])* $name:ident, $kind:literal, $max:ident, $err:ident) => {
        $(#[$doc])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            /// Checks `text` and wraps it.
            ///
            /// # Errors
            ///
            /// Returns an error when `text` is empty, over its byte cap, or not
            /// printable ASCII.
            pub fn new(text: impl Into<String>) -> Result<Self, IdError> {
                let text = text.into();
                check_with($kind, $max, |kind| IdError::$err { kind }, &text)?;
                Ok(Self(text))
            }

            /// The id as text.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl TryFrom<String> for $name {
            type Error = IdError;

            fn try_from(text: String) -> Result<Self, Self::Error> {
                Self::new(text)
            }
        }

        impl From<$name> for String {
            fn from(id: $name) -> Self {
                id.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl FromStr for $name {
            type Err = IdError;

            fn from_str(text: &str) -> Result<Self, Self::Err> {
                Self::new(text)
            }
        }
    };
}

id_type!(
    /// A channel, for example `cli` or `discord`.
    ChannelId,
    "channel id"
);
id_type!(
    /// A sender as the channel names it, for example `uid:1000`.
    SenderId,
    "sender id"
);
id_type!(
    /// The id of one inbound message. It is the idempotency key of its op.
    MessageId,
    "message id"
);
id_type!(
    /// The owner of the daemon. It names the agent `assistant/<owner>`.
    OwnerName,
    "owner name"
);
id_type!(
    /// The id of one pool task from `escapement-serve`.
    TaskId,
    "task id"
);
id_type!(
    /// The shell id of one `lg pool` worker instance.
    ShellId,
    "shell id"
);
id_type!(
    /// A Cortex mesh role name, such as `service-escapement-pool`.
    RoleName,
    "role name"
);
id_type!(
    /// The peer id of a Cortex pool member.
    PoolPeerId,
    "pool peer id"
);
id_type!(
    /// An opaque lease token returned by Cortex `/v1/mesh/pool/recv`.
    DeliveryToken,
    "delivery token",
    MAX_DELIVERY_TOKEN_BYTES,
    TokenTooLong
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_printable_id_is_accepted() {
        let id = MessageId::new("m-1").unwrap();
        assert_eq!(id.as_str(), "m-1");
        assert_eq!(id.to_string(), "m-1");
    }

    #[test]
    fn an_empty_long_or_unprintable_id_is_refused() {
        assert_eq!(SenderId::new(""), Err(IdError::Empty { kind: "sender id" }));
        assert_eq!(
            SenderId::new("x".repeat(MAX_ID_BYTES + 1)),
            Err(IdError::TooLong { kind: "sender id" })
        );
        assert_eq!(
            SenderId::new("a b"),
            Err(IdError::NotPrintable { kind: "sender id" })
        );
        assert!(SenderId::new("x".repeat(MAX_ID_BYTES)).is_ok());
    }

    #[test]
    fn an_id_converts_to_and_from_a_string() {
        let id = ChannelId::try_from(String::from("cli")).unwrap();
        assert_eq!(String::from(id), "cli");
        assert!(OwnerName::try_from(String::from("a\nb")).is_err());
    }
}
