//! The two platforms a controller is built for.

/// A supported controller platform.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    Amd64,
    Arm64,
}

impl Platform {
    /// `linux/amd64` or `linux/arm64`.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "linux/amd64" => Some(Self::Amd64),
            "linux/arm64" => Some(Self::Arm64),
            _ => None,
        }
    }

    /// The platform an engine's `image inspect` reports as `Os` and
    /// `Architecture`.
    #[must_use]
    pub fn from_inspect(os: &str, architecture: &str) -> Option<Self> {
        match (os, architecture) {
            ("linux", "amd64") => Some(Self::Amd64),
            ("linux", "arm64") => Some(Self::Arm64),
            _ => None,
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Amd64 => "linux/amd64",
            Self::Arm64 => "linux/arm64",
        }
    }

    /// The OCI architecture name.
    #[must_use]
    pub const fn architecture(self) -> &'static str {
        match self {
            Self::Amd64 => "amd64",
            Self::Arm64 => "arm64",
        }
    }

    /// The Rust target triple the controller is compiled for.
    #[must_use]
    pub const fn rust_target(self) -> &'static str {
        match self {
            Self::Amd64 => "x86_64-unknown-linux-gnu",
            Self::Arm64 => "aarch64-unknown-linux-gnu",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platforms_round_trip() {
        for platform in [Platform::Amd64, Platform::Arm64] {
            assert_eq!(Platform::parse(platform.as_str()), Some(platform));
            assert_eq!(
                Platform::from_inspect("linux", platform.architecture()),
                Some(platform)
            );
        }
        assert_eq!(Platform::parse("linux/arm64/v8"), None);
        assert_eq!(Platform::from_inspect("windows", "amd64"), None);
        assert_eq!(Platform::Arm64.rust_target(), "aarch64-unknown-linux-gnu");
    }
}
