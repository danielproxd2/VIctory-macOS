//! Everything specific to Grand Theft Auto VI: its container formats, build
//! identities, and the hooks we place in its code (located by per-build
//! offsets or byte-pattern signatures). What may and may not be stored here
//! is defined in docs/asset-policy.md.

pub mod cityhash;
pub mod pipelines;
pub mod sgd5;
pub mod shaderpack;

/// A specific build of the Grand Theft Auto VI executable we know how to run.
#[derive(Debug, Clone, Copy)]
pub struct KnownBuild {
    /// Our own label, e.g. "1.0.0 retail".
    pub label: &'static str,
    /// BLAKE3 of the original executable, hex.
    pub blake3: &'static str,
    /// RPF9 archive the title needs during startup, relative to its executable.
    pub startup_archive: &'static str,
}

/// Executables identified from a local package.
pub const KNOWN_BUILDS: &[KnownBuild] = &[KnownBuild {
    label: "Series 1.0.2.1",
    blake3: "b99f876b74d4a77f03af5f878223b27bd647b5c35a79d5decd9f716504239e93",
    startup_archive: "appdata0.rpf",
}];

/// Looks up a build by the BLAKE3 hash of its executable.
pub fn identify(blake3_hex: &str) -> Option<&'static KnownBuild> {
    identify_in(KNOWN_BUILDS, blake3_hex)
}

/// The lookup behind [`identify`], over any table: the build whose hash is
/// `blake3_hex`, whole, in either case of hex digits.
pub fn identify_in<'a>(builds: &'a [KnownBuild], blake3_hex: &str) -> Option<&'a KnownBuild> {
    builds
        .iter()
        .find(|b| b.blake3.eq_ignore_ascii_case(blake3_hex))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hashes of our own strings, not of any executable.
    const BUILDS: &[KnownBuild] = &[
        KnownBuild {
            label: "first",
            blake3: "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff",
            startup_archive: "appdata0.rpf",
        },
        KnownBuild {
            label: "second",
            blake3: "FFEEDDCCBBAA99887766554433221100FFEEDDCCBBAA99887766554433221100",
            startup_archive: "appdata0.rpf",
        },
    ];

    #[test]
    fn identify_matches_hex_in_either_case() {
        for build in BUILDS {
            for hex in [
                build.blake3.to_owned(),
                build.blake3.to_ascii_lowercase(),
                build.blake3.to_ascii_uppercase(),
            ] {
                assert_eq!(
                    identify_in(BUILDS, &hex).map(|b| b.label),
                    Some(build.label)
                );
            }
        }
    }

    #[test]
    fn an_unknown_build_is_none() {
        let known = BUILDS[0].blake3;
        let one_off = format!("{}e", &known[..63]);
        for hex in [
            "",
            "00",
            // A prefix, the hash with more after it, one digit changed.
            &known[..32],
            &format!("{known}00"),
            &one_off,
            &format!(" {known}"),
            "not hex at all",
        ] {
            assert!(identify_in(BUILDS, hex).is_none(), "{hex:?}");
            // The shipped table knows no build yet.
            assert!(identify(hex).is_none(), "{hex:?}");
        }
        assert!(identify_in(&[], known).is_none());
        assert!(identify(known).is_none());
    }
}
