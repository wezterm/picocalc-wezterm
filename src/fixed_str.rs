use heapless::String;
use sequential_storage::map::{Key, SerializationError, Value};

#[derive(Clone, Eq, PartialEq, Hash)]
pub struct FixedString<const N: usize>(String<N>);

impl<const N: usize> core::ops::Deref for FixedString<N> {
    type Target = String<N>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<const N: usize> core::ops::DerefMut for FixedString<N> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<const N: usize> AsRef<str> for FixedString<N> {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl<const N: usize> core::fmt::Display for FixedString<N> {
    fn fmt(&self, w: &mut core::fmt::Formatter) -> core::fmt::Result {
        self.as_str().fmt(w)
    }
}

impl<const N: usize> core::fmt::Debug for FixedString<N> {
    fn fmt(&self, w: &mut core::fmt::Formatter) -> core::fmt::Result {
        self.as_str().fmt(w)
    }
}

impl<const N: usize> FixedString<N> {
    pub const fn new() -> Self {
        Self(String::new())
    }

    pub fn with_str<T: AsRef<str>>(s: T) -> Result<Self, ()> {
        let s: &str = s.as_ref();
        let mut result = Self::new();
        result.0.push_str(s)?;
        Ok(result)
    }

    pub fn as_str(&self) -> &str {
        self.0.as_ref()
    }
}

impl<const N: usize> Key for FixedString<N> {
    fn serialize_into(&self, buffer: &mut [u8]) -> Result<usize, SerializationError> {
        Key::serialize_into(&self.0, buffer)
    }

    fn deserialize_from(buffer: &[u8]) -> Result<(Self, usize), SerializationError> {
        <String<N> as Key>::deserialize_from(buffer).map(|(k, size)| (Self(k), size))
    }

    fn get_len(buffer: &[u8]) -> Result<usize, SerializationError> {
        <String<N> as Key>::get_len(buffer)
    }
}

impl<'a, const N: usize> Value<'a> for FixedString<N> {
    fn serialize_into(&self, buffer: &mut [u8]) -> Result<usize, SerializationError> {
        <String<N> as Value>::serialize_into(&self.0, buffer)
    }

    fn deserialize_from(buffer: &[u8]) -> Result<Self, SerializationError> {
        <String<N> as Value>::deserialize_from(buffer).map(Self)
    }
}

impl<const N: usize> TryInto<FixedString<N>> for &str {
    type Error = sequential_storage::Error<embassy_rp::flash::Error>;

    fn try_into(self) -> Result<FixedString<N>, Self::Error> {
        let mut result = String::<N>::new();
        result
            .push_str(self)
            .map_err(|()| Self::Error::ItemTooBig)?;
        Ok(FixedString(result))
    }
}
