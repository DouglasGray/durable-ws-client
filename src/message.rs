use bytes::Bytes;
use std::{
    borrow::Borrow,
    fmt,
    hash::{Hash, Hasher},
    ops::Deref,
    str::Utf8Error,
};

pub use yawc::close::CloseCode;

/// A [`Bytes`] that is known to hold valid UTF-8.
///
/// Text payloads arrive from the socket as a refcounted view into the
/// connection's read buffer. Keeping them as `Bytes` rather than converting to
/// `String` avoids a copy per message, and this wrapper preserves the "text is
/// valid UTF-8" guarantee that `String` provided: the payload is scanned once,
/// when the value is constructed, and never again.
///
/// It dereferences to `str`, so most code written against `String` keeps
/// working unchanged.
#[derive(Clone, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Utf8Bytes(Bytes);

impl Utf8Bytes {
    /// Wraps `bytes` without checking that it holds valid UTF-8.
    ///
    /// # Safety
    ///
    /// `bytes` must contain valid UTF-8. Violating this makes [`as_str`] —
    /// and therefore `Deref`, `Display` and `Debug` — undefined behaviour.
    ///
    /// [`as_str`]: Self::as_str
    pub unsafe fn from_utf8_unchecked(bytes: Bytes) -> Self {
        Self(bytes)
    }

    /// Borrows the payload as a string slice.
    pub fn as_str(&self) -> &str {
        // SAFETY: every safe constructor validates the payload, and
        // `from_utf8_unchecked` puts that obligation on its caller, so the
        // inner `Bytes` always holds valid UTF-8.
        unsafe { std::str::from_utf8_unchecked(&self.0) }
    }

    /// Borrows the payload as raw bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// Unwraps the underlying [`Bytes`], without copying.
    pub fn into_bytes(self) -> Bytes {
        self.0
    }

    /// The length of the payload in bytes.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the payload is empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

// Hashed as a `str` rather than as bytes so that the `Borrow<str>` impl below
// is sound: a `HashMap<Utf8Bytes, _>` must be able to look up by `&str`.
impl Hash for Utf8Bytes {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_str().hash(state);
    }
}

impl Deref for Utf8Bytes {
    type Target = str;

    fn deref(&self) -> &str {
        self.as_str()
    }
}

impl Borrow<str> for Utf8Bytes {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

impl AsRef<str> for Utf8Bytes {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl AsRef<[u8]> for Utf8Bytes {
    fn as_ref(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl TryFrom<Bytes> for Utf8Bytes {
    type Error = Utf8Error;

    /// Validates `bytes` as UTF-8 without copying it.
    fn try_from(bytes: Bytes) -> Result<Self, Self::Error> {
        std::str::from_utf8(&bytes)?;
        Ok(Self(bytes))
    }
}

impl From<String> for Utf8Bytes {
    /// Takes ownership of the string's buffer; no copy and no validation, as
    /// a `String` is already known to be UTF-8.
    fn from(s: String) -> Self {
        Self(Bytes::from(s.into_bytes()))
    }
}

impl From<&str> for Utf8Bytes {
    fn from(s: &str) -> Self {
        Self(Bytes::copy_from_slice(s.as_bytes()))
    }
}

impl From<Utf8Bytes> for Bytes {
    fn from(bytes: Utf8Bytes) -> Self {
        bytes.into_bytes()
    }
}

impl From<Utf8Bytes> for String {
    fn from(bytes: Utf8Bytes) -> Self {
        bytes.as_str().to_owned()
    }
}

impl PartialEq<str> for Utf8Bytes {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for Utf8Bytes {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl PartialEq<String> for Utf8Bytes {
    fn eq(&self, other: &String) -> bool {
        self.as_str() == other.as_str()
    }
}

impl PartialEq<Utf8Bytes> for str {
    fn eq(&self, other: &Utf8Bytes) -> bool {
        self == other.as_str()
    }
}

impl fmt::Display for Utf8Bytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self.as_str(), f)
    }
}

impl fmt::Debug for Utf8Bytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), f)
    }
}

/// A message exchanged with the application.
///
/// Both variants hold [`Bytes`], so a message read from the socket is handed
/// to the application without copying its payload.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Message {
    Text(Utf8Bytes),
    Binary(Bytes),
}

impl Message {
    /// Builds a text message.
    pub fn text(text: impl Into<Utf8Bytes>) -> Self {
        Self::Text(text.into())
    }

    /// Builds a binary message.
    pub fn binary(data: impl Into<Bytes>) -> Self {
        Self::Binary(data.into())
    }

    /// Borrows the message payload, whatever its type.
    pub fn as_payload(&self) -> &Bytes {
        match self {
            Self::Text(text) => &text.0,
            Self::Binary(data) => data,
        }
    }

    /// Unwraps the message payload, without copying.
    pub fn into_payload(self) -> Bytes {
        match self {
            Self::Text(text) => text.into_bytes(),
            Self::Binary(data) => data,
        }
    }

    /// The length of the payload in bytes.
    pub fn len(&self) -> usize {
        self.as_payload().len()
    }

    /// Whether the payload is empty.
    pub fn is_empty(&self) -> bool {
        self.as_payload().is_empty()
    }
}

impl From<String> for Message {
    fn from(s: String) -> Self {
        Self::Text(s.into())
    }
}

impl From<&str> for Message {
    fn from(s: &str) -> Self {
        Self::Text(s.into())
    }
}

impl From<Vec<u8>> for Message {
    fn from(data: Vec<u8>) -> Self {
        Self::Binary(data.into())
    }
}

impl From<Bytes> for Message {
    fn from(data: Bytes) -> Self {
        Self::Binary(data)
    }
}

/// The close frame received from the peer, if it sent one carrying a status
/// code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloseFrame {
    pub code: CloseCode,
    pub reason: Utf8Bytes,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_bytes_wraps_without_copying() {
        let bytes = Bytes::from_static(b"hello");
        let text = Utf8Bytes::try_from(bytes.clone()).unwrap();

        assert_eq!(text.as_str(), "hello");
        assert_eq!(text.as_bytes().as_ptr(), bytes.as_ptr());
        assert_eq!(text.into_bytes().as_ptr(), bytes.as_ptr());
    }

    #[test]
    fn utf8_bytes_rejects_invalid_utf8() {
        assert!(Utf8Bytes::try_from(Bytes::from_static(&[0xff, 0xfe])).is_err());
    }

    #[test]
    fn utf8_bytes_hashes_as_str() {
        use std::collections::HashMap;

        let mut map = HashMap::new();
        map.insert(Utf8Bytes::from("key".to_owned()), 1);

        assert_eq!(map.get("key"), Some(&1));
    }

    #[test]
    fn message_payload_round_trips() {
        let text = Message::text("hi");
        assert_eq!(text.len(), 2);
        assert_eq!(text.into_payload(), Bytes::from_static(b"hi"));

        let binary = Message::binary(Bytes::from_static(b"\x00\x01"));
        assert_eq!(binary.into_payload(), Bytes::from_static(b"\x00\x01"));
    }
}
