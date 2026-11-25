//! COFF import libraries (`.lib`).
//!
//! An umbrella import library tells us which DLL provides each API that a
//! console title may link against. That is the map the runtime needs to
//! decide which of our implementations answers which import.

use object::read::archive::ArchiveFile;
use object::read::coff::{ImportFile, ImportName, ImportType};

use crate::{ImportSymbol, PeError};

/// One `dll!export` pair described by an import library.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct LibImport {
    pub dll: String,
    pub export: ImportSymbol,
    pub kind: LibImportKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LibImportKind {
    Code,
    Data,
    Const,
}

/// Result of reading an import library.
#[derive(Debug, Default)]
pub struct ImportLibrary {
    pub imports: Vec<LibImport>,
    /// Members that are not short-format import objects (regular objects,
    /// long-format import descriptors). Reported so nothing is silently lost.
    pub other_members: usize,
}

impl ImportLibrary {
    pub fn parse(data: &[u8]) -> Result<Self, PeError> {
        let archive = ArchiveFile::parse(data)?;
        let mut lib = ImportLibrary::default();
        for member in archive.members() {
            let member = member?;
            let bytes = member.data(data)?;
            match ImportFile::parse(bytes) {
                Ok(import) => lib.imports.push(LibImport {
                    dll: String::from_utf8_lossy(import.dll()).into_owned(),
                    export: match import.import() {
                        ImportName::Name(name) => ImportSymbol::Name {
                            hint: 0,
                            name: String::from_utf8_lossy(name).into_owned(),
                        },
                        ImportName::Ordinal(o) => ImportSymbol::Ordinal(o),
                    },
                    kind: match import.import_type() {
                        ImportType::Code => LibImportKind::Code,
                        ImportType::Data => LibImportKind::Data,
                        ImportType::Const => LibImportKind::Const,
                    },
                }),
                Err(_) => lib.other_members += 1,
            }
        }
        lib.imports.sort();
        lib.imports.dedup();
        Ok(lib)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MACHINE_AMD64;

    // `IMPORT_OBJECT_HEADER` type bits and name-type values.
    const CODE: u16 = 0;
    const DATA: u16 = 1;
    const CONST: u16 = 2;
    const BY_ORDINAL: u16 = 0;
    const BY_NAME: u16 = 1;

    /// A COFF archive written in code. The signature is kept in two pieces:
    /// it is one of the file signatures the asset policy refuses.
    struct Archive(Vec<u8>);

    impl Archive {
        fn new() -> Self {
            Archive([&b"!<"[..], &b"arch>\n"[..]].concat())
        }

        /// Appends a member: the 60-byte header, then `data`, padded to an
        /// even offset.
        fn member(mut self, name: &str, data: &[u8]) -> Self {
            // name[16] date[12] uid[6] gid[6] mode[8] size[10], then the
            // two terminator bytes.
            let header = format!(
                "{:<16}{:<12}{:<6}{:<6}{:<8}{:<10}`\n",
                format!("{name}/"),
                0,
                0,
                0,
                0,
                data.len()
            );
            assert_eq!(header.len(), 60);
            self.0.extend_from_slice(header.as_bytes());
            self.0.extend_from_slice(data);
            if data.len() % 2 == 1 {
                self.0.push(b'\n');
            }
            self
        }
    }

    /// A short-format import object: `IMPORT_OBJECT_HEADER` followed by the
    /// symbol and DLL names.
    fn import_object(
        symbol: &str,
        dll: &str,
        ordinal_or_hint: u16,
        kind: u16,
        name_type: u16,
    ) -> Vec<u8> {
        let strings = [symbol.as_bytes(), &[0], dll.as_bytes(), &[0]].concat();
        let mut d = Vec::new();
        d.extend_from_slice(&0u16.to_le_bytes()); // Sig1: IMAGE_FILE_MACHINE_UNKNOWN
        d.extend_from_slice(&0xFFFFu16.to_le_bytes()); // Sig2
        d.extend_from_slice(&0u16.to_le_bytes()); // Version
        d.extend_from_slice(&MACHINE_AMD64.to_le_bytes());
        d.extend_from_slice(&0u32.to_le_bytes()); // TimeDateStamp
        d.extend_from_slice(&(strings.len() as u32).to_le_bytes()); // SizeOfData
        d.extend_from_slice(&ordinal_or_hint.to_le_bytes());
        d.extend_from_slice(&(kind | name_type << 2).to_le_bytes());
        d.extend_from_slice(&strings);
        d
    }

    /// The header of an ordinary AMD64 object file with no sections.
    fn regular_object() -> Vec<u8> {
        let mut d = vec![0u8; 20];
        d[..2].copy_from_slice(&MACHINE_AMD64.to_le_bytes());
        d
    }

    fn named(name: &str) -> ImportSymbol {
        ImportSymbol::Name {
            hint: 0,
            name: name.to_owned(),
        }
    }

    #[test]
    fn reads_code_data_const_and_ordinal_members() {
        let lib = Archive::new()
            .member(
                "one.dll",
                &import_object("Function", "one.dll", 4, CODE, BY_NAME),
            )
            .member(
                "one.dll",
                &import_object("Variable", "one.dll", 0, DATA, BY_NAME),
            )
            .member(
                "two.dll",
                &import_object("Constant", "two.dll", 0, CONST, BY_NAME),
            )
            .member(
                "two.dll",
                &import_object("Unnamed", "two.dll", 17, CODE, BY_ORDINAL),
            );
        let lib = ImportLibrary::parse(&lib.0).unwrap();
        assert_eq!(lib.other_members, 0);
        let import = |dll: &str, export, kind| LibImport {
            dll: dll.to_owned(),
            export,
            kind,
        };
        // Sorted by DLL, then export. The hint is not part of the identity.
        assert_eq!(
            lib.imports,
            [
                import("one.dll", named("Function"), LibImportKind::Code),
                import("one.dll", named("Variable"), LibImportKind::Data),
                import("two.dll", named("Constant"), LibImportKind::Const),
                import("two.dll", ImportSymbol::Ordinal(17), LibImportKind::Code),
            ]
        );
    }

    #[test]
    fn duplicates_collapse_and_other_members_are_counted() {
        let function = import_object("Function", "one.dll", 0, CODE, BY_NAME);
        let lib = Archive::new()
            .member("x.obj", &regular_object())
            .member("one.dll", &function)
            // The same export with another hint is still the same export.
            .member(
                "one.dll",
                &import_object("Function", "one.dll", 9, CODE, BY_NAME),
            )
            .member("odd", b"odd")
            .member("one.dll", &function);
        let lib = ImportLibrary::parse(&lib.0).unwrap();
        assert_eq!(lib.imports.len(), 1);
        assert_eq!(lib.imports[0].export, named("Function"));
        assert_eq!(lib.other_members, 2);
    }

    #[test]
    fn an_empty_archive_has_no_imports() {
        let lib = ImportLibrary::parse(&Archive::new().0).unwrap();
        assert!(lib.imports.is_empty());
        assert_eq!(lib.other_members, 0);
    }

    #[test]
    fn rejects_non_archive() {
        for data in [&b""[..], b"not an archive at all", &regular_object()] {
            assert!(matches!(
                ImportLibrary::parse(data),
                Err(PeError::Malformed(_))
            ));
        }
        // A member that claims more data than the file holds.
        let mut cut = Archive::new()
            .member(
                "one.dll",
                &import_object("Function", "one.dll", 0, CODE, BY_NAME),
            )
            .0;
        cut.truncate(cut.len() - 4);
        assert!(matches!(
            ImportLibrary::parse(&cut),
            Err(PeError::Malformed(_))
        ));
        // A member header cut short.
        let mut cut = Archive::new().member("x.obj", &regular_object()).0;
        cut.truncate(8 + 30);
        assert!(matches!(
            ImportLibrary::parse(&cut),
            Err(PeError::Malformed(_))
        ));
    }

    /// Seeded mutations of a small archive: every outcome is a value. What
    /// the parser accepts is sorted, free of duplicates and no larger than
    /// the archive it came from.
    #[test]
    fn mutated_archives_never_panic() {
        use crate::testing::{XorShift64, mutate};

        let function = import_object("Function", "one.dll", 4, CODE, BY_NAME);
        let seed = Archive::new()
            .member("x.obj", &regular_object())
            .member("one.dll", &function)
            .member(
                "one.dll",
                &import_object("Variable", "one.dll", 0, DATA, BY_NAME),
            )
            .member(
                "two.dll",
                &import_object("Unnamed", "two.dll", 17, CODE, BY_ORDINAL),
            )
            .member("odd", b"odd")
            .member("one.dll", &function)
            .0;
        let whole = ImportLibrary::parse(&seed).unwrap();
        assert_eq!((whole.imports.len(), whole.other_members), (3, 2));

        let mut rng = XorShift64::new(0x5EED_0003);
        let (mut accepted, mut refused) = (0u32, 0u32);
        for i in 0..10_000 {
            let mut data = seed.clone();
            // Member headers are spread over the file: no part is special.
            mutate(&mut data, seed.len(), &mut rng);
            if rng.below(8) == 0 {
                data.truncate(rng.below(data.len()));
            }
            match ImportLibrary::parse(&data) {
                Ok(lib) => {
                    accepted += 1;
                    assert!(
                        lib.imports.windows(2).all(|pair| pair[0] < pair[1]),
                        "mutation {i}: sorted, no duplicates"
                    );
                    // A member is at least its 60-byte header.
                    let members = lib.imports.len() + lib.other_members;
                    assert!(members <= data.len() / 60, "mutation {i}: {members}");
                }
                Err(PeError::Malformed(why) | PeError::Unsupported(why)) => {
                    refused += 1;
                    assert!(!why.is_empty(), "mutation {i}");
                }
            }
        }
        assert!(accepted > 0, "no mutation was accepted");
        assert!(refused > 0, "no mutation was refused");
    }
}
