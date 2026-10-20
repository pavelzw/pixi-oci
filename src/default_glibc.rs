// Shared with build.rs through `include!`, which embeds the default version's packages with the
// `bundled-glibc` feature. Bumping the version there also needs the new packages' SHA-1s.

/// Debian `libc6` version used for `glibc = true`: the glibc of the current Debian stable.
pub const DEFAULT_GLIBC_DEBIAN_VERSION: &str = "2.41-12+deb13u4";
