//! Embeds the argument clamp bytecode that the clamp-bytecode workflow
//! builds with Microsoft's compilers, since Wine cannot compile the shader.

use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};

/// FNV-1a, the same hash `victory-gpu` takes of the source. The asset
/// policy pins its exceptions with it too.
pub(crate) fn fnv(data: &[u8]) -> u64 {
    data.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3)
    })
}

/// An array of bytes laid out as rustfmt lays it out, so that formatting
/// the workspace leaves the written file, and the hash the asset policy
/// pins, alone.
fn array(data: &[u8]) -> String {
    let mut out = String::from("&[\n");
    for row in data.chunks(16) {
        out.push_str("   ");
        for b in row {
            let _ = write!(out, " 0x{b:02x},");
        }
        out.push('\n');
    }
    out.push(']');
    out
}

/// The four-character names of a DXBC container's parts, or why `data` is
/// not such a container. The container's checksum is not verified: the
/// host does that when it takes the shader.
fn parts(data: &[u8]) -> Result<Vec<[u8; 4]>> {
    let word = |at: usize| -> Option<usize> {
        let bytes = data.get(at..at.checked_add(4)?)?;
        usize::try_from(u32::from_le_bytes(bytes.try_into().ok()?)).ok()
    };
    ensure!(data.starts_with(b"DXBC"), "does not start with `DXBC`");
    // Signature, 16 bytes of checksum, a version, the size and a count.
    let (Some(size), Some(count)) = (word(24), word(28)) else {
        bail!("ends inside its header");
    };
    ensure!(
        size == data.len(),
        "says it is {size} bytes long and is {}",
        data.len()
    );
    let mut names = Vec::new();
    for index in 0..count {
        let part = index
            .checked_mul(4)
            .and_then(|o| o.checked_add(32))
            .and_then(word)
            .with_context(|| format!("part {index} has no offset"))?;
        let (Some(name), Some(len)) = (
            part.checked_add(4).and_then(|end| data.get(part..end)),
            part.checked_add(4).and_then(word),
        ) else {
            bail!("part {index} starts outside the file");
        };
        ensure!(
            len <= data.len() - part - 8,
            "part {index} ends outside the file"
        );
        names.push(name.try_into().expect("four bytes"));
    }
    Ok(names)
}

/// Reads one compiled shader and refuses anything that is not a container
/// holding one of the `wanted` parts.
fn bytecode(path: &Path, wanted: &[&[u8; 4]]) -> Result<Vec<u8>> {
    let data = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let names = parts(&data).with_context(|| format!("{} is not bytecode", path.display()))?;
    ensure!(
        names.iter().any(|name| wanted.contains(&name)),
        "{} holds no {} part",
        path.display(),
        wanted
            .iter()
            .map(|name| String::from_utf8_lossy(*name))
            .collect::<Vec<_>>()
            .join(" or ")
    );
    Ok(data)
}

/// The text of `clamp_bytecode.rs`.
fn render(source: &[u8], dxil: &[u8], dxbc: &[u8]) -> String {
    format!(
        "//! The argument clamp shader (`clamp.hlsl`) compiled by Microsoft's\n\
         //! compilers on a Windows runner. Written by `cargo xtask embed-clamp`.\n\
         //! `SOURCE_HASH` names the source it was built from, and other bytes are\n\
         //! never used.\n\n\
         pub(crate) const SOURCE_HASH: u64 = 0x{:016x};\n\
         pub(crate) const DXIL: &[u8] = {};\n\
         pub(crate) const DXBC: &[u8] = {};\n",
        fnv(source),
        array(dxil),
        array(dxbc)
    )
}

/// Writes `clamp_bytecode.rs` into `src`, the folder that holds
/// `clamp.hlsl`, from `clamp.dxil` and `clamp.dxbc` in `folder`. Nothing is
/// written unless both are what their names say.
pub fn run(src: &Path, folder: &Path) -> Result<()> {
    let source = std::fs::read(src.join("clamp.hlsl")).context("reading clamp.hlsl")?;
    let dxil = bytecode(&folder.join("clamp.dxil"), &[b"DXIL"])?;
    // Shader model 5 code is a SHEX part, and model 4 code a SHDR part.
    let dxbc = bytecode(&folder.join("clamp.dxbc"), &[b"SHEX", b"SHDR"])?;
    let text = render(&source, &dxil, &dxbc);
    std::fs::write(src.join("clamp_bytecode.rs"), &text).context("writing clamp_bytecode.rs")?;
    println!(
        "embedded {} bytes of DXIL and {} of DXBC",
        dxil.len(),
        dxbc.len()
    );
    // The file is an exception to the asset policy, pinned as written.
    println!(
        "its entry in xtask/src/policy.rs is now: len: {}, fnv: 0x{:016x},",
        text.len(),
        fnv(text.as_bytes())
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{fnv, parts, render, run};

    /// The folder of `victory-gpu` that holds the shader and its bytecode.
    fn gpu_src() -> PathBuf {
        crate::workspace_root().join("crates/victory-gpu/src")
    }

    /// An empty folder of this test's own, outside the repository.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("victory-embed-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A DXBC container of our own making with the given parts. Its
    /// checksum is left zero.
    fn container(parts: &[(&[u8; 4], &[u8])]) -> Vec<u8> {
        let word = |n: usize| u32::try_from(n).unwrap().to_le_bytes();
        let mut offset = 32 + 4 * parts.len();
        let total = offset + parts.iter().map(|(_, d)| 8 + d.len()).sum::<usize>();
        let mut out = b"DXBC".to_vec();
        out.extend_from_slice(&[0; 16]);
        out.extend_from_slice(&word(1));
        out.extend_from_slice(&word(total));
        out.extend_from_slice(&word(parts.len()));
        for (_, data) in parts {
            out.extend_from_slice(&word(offset));
            offset += 8 + data.len();
        }
        for (name, data) in parts {
            out.extend_from_slice(*name);
            out.extend_from_slice(&word(data.len()));
            out.extend_from_slice(data);
        }
        assert_eq!(out.len(), total);
        out
    }

    /// The value of `pub(crate) const SOURCE_HASH` in the written text.
    fn written_hash(text: &str) -> u64 {
        let rest = text
            .split_once("const SOURCE_HASH: u64 = 0x")
            .expect("a SOURCE_HASH line")
            .1;
        let digits = rest.split_once(';').expect("the line's end").0;
        u64::from_str_radix(digits, 16).expect("a hexadecimal hash")
    }

    /// The bytes of the array `name` in the written text.
    fn written_array(text: &str, name: &str) -> Vec<u8> {
        let rest = text
            .split_once(&format!("const {name}: &[u8] = &["))
            .unwrap_or_else(|| panic!("no {name} array"))
            .1;
        rest.split_once(']')
            .expect("the array's end")
            .0
            .split(',')
            .map(str::trim)
            .filter(|literal| !literal.is_empty())
            .map(|literal| {
                let digits = literal.strip_prefix("0x").expect("a hexadecimal byte");
                u8::from_str_radix(digits, 16).expect("a byte")
            })
            .collect()
    }

    /// `victory-gpu` uses the prebuilt bytecode only when its hash names the
    /// current source, and otherwise asks the system to compile the shader,
    /// which Wine cannot do. So a stale embed must fail on every host.
    #[test]
    fn embedded_clamp_bytecode_matches_the_current_source() {
        let source = std::fs::read(gpu_src().join("clamp.hlsl")).unwrap();
        let embedded = std::fs::read_to_string(gpu_src().join("clamp_bytecode.rs")).unwrap();
        assert_eq!(
            written_hash(&embedded),
            fnv(&source),
            "clamp.hlsl changed: run the clamp-bytecode workflow and `cargo xtask embed-clamp`"
        );
    }

    /// The embedded file is exactly what embed-clamp writes from two
    /// containers it accepts, so nothing in it was edited by hand.
    #[test]
    fn embedded_clamp_bytecode_is_what_embed_clamp_writes() {
        let source = std::fs::read(gpu_src().join("clamp.hlsl")).unwrap();
        let embedded = std::fs::read_to_string(gpu_src().join("clamp_bytecode.rs")).unwrap();
        let dxil = written_array(&embedded, "DXIL");
        let dxbc = written_array(&embedded, "DXBC");
        assert!(parts(&dxil).unwrap().contains(b"DXIL"), "the DXIL array");
        assert!(parts(&dxbc).unwrap().contains(b"SHEX"), "the DXBC array");
        assert!(
            render(&source, &dxil, &dxbc) == embedded,
            "the text differs"
        );
    }

    /// The same FNV-1a as `victory-gpu`'s `indirect::fnv`, whose test holds
    /// these vectors too (the published ones for the 64-bit hash).
    #[test]
    fn fnv_matches_the_gpu_crates_source_hash() {
        assert_eq!(fnv(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv(b"foobar"), 0x8594_4171_f739_67e8);
    }

    /// Runs embed-clamp on the two given files and says whether it wrote.
    fn embed(name: &str, dxil: Option<&[u8]>, dxbc: Option<&[u8]>) -> (bool, Option<String>) {
        let dir = scratch(name);
        let (src, folder) = (dir.join("src"), dir.join("out"));
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(src.join("clamp.hlsl"), b"void main() {}\n").unwrap();
        let put = |file: &Path, data: Option<&[u8]>| {
            if let Some(data) = data {
                std::fs::write(file, data).unwrap();
            }
        };
        put(&folder.join("clamp.dxil"), dxil);
        put(&folder.join("clamp.dxbc"), dxbc);
        let result = run(&src, &folder);
        let written = std::fs::read_to_string(src.join("clamp_bytecode.rs")).ok();
        let _ = std::fs::remove_dir_all(&dir);
        (result.is_ok(), written)
    }

    #[test]
    fn inputs_that_are_not_containers_are_refused() {
        let dxil = container(&[(b"SFI0", &[0; 8]), (b"DXIL", &[1, 2, 3, 4])]);
        let dxbc = container(&[(b"RDEF", &[]), (b"SHEX", &[5, 6, 7, 8])]);
        type Input<'a> = Option<&'a [u8]>;
        let refused: [(&str, Input, Input); 9] = [
            ("text", Some(b"not bytecode\n"), Some(&dxbc)),
            ("text-dxbc", Some(&dxil), Some(b"not bytecode\n")),
            ("empty", Some(b""), Some(&dxbc)),
            ("missing", None, Some(&dxbc)),
            ("missing-dxbc", Some(&dxil), None),
            // Each is a container, but not the kind its name gives.
            ("swapped", Some(&dxbc), Some(&dxil)),
            ("twice", Some(&dxil), Some(&dxil)),
            ("cut", Some(&dxil[..dxil.len() - 2]), Some(&dxbc)),
            ("header", Some(b"DXBC\0\0\0\0"), Some(&dxbc)),
        ];
        for (name, dxil, dxbc) in refused {
            assert_eq!(embed(name, dxil, dxbc), (false, None), "{name}");
        }
        // A part that says it runs past the end of the file.
        let mut long = dxil.clone();
        let at = long.len() - 8;
        long[at..at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(embed("long", Some(&long), Some(&dxbc)), (false, None));
        // A part offset outside the file.
        let mut far = dxil.clone();
        far[36..40].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(embed("far", Some(&far), Some(&dxbc)), (false, None));
        // Whole containers but for the four bytes that say so. Nothing else
        // about them is wrong, so only the signature check refuses them.
        for (name, signature) in [("lower", b"dxbc"), ("zero", &[0; 4]), ("pe", b"MZ\x90\0")] {
            let mut unsigned = dxil.clone();
            unsigned[..4].copy_from_slice(signature);
            assert_eq!(
                parts(&unsigned).unwrap_err().to_string(),
                "does not start with `DXBC`",
                "{name}"
            );
            assert_eq!(
                embed(name, Some(&unsigned), Some(&dxbc)),
                (false, None),
                "{name}"
            );
            let mut unsigned = dxbc.clone();
            unsigned[..4].copy_from_slice(signature);
            assert_eq!(
                embed(name, Some(&dxil), Some(&unsigned)),
                (false, None),
                "{name} as clamp.dxbc"
            );
        }

        let (ok, written) = embed("good", Some(&dxil), Some(&dxbc));
        assert!(ok, "two containers of the right kinds");
        let written = written.expect("clamp_bytecode.rs");
        assert_eq!(written_hash(&written), fnv(b"void main() {}\n"));
        assert_eq!(written_array(&written, "DXIL"), dxil);
        assert_eq!(written_array(&written, "DXBC"), dxbc);
    }
}
