//! Embeds the bytecode of `victory-gpu`'s own HLSL shaders (the argument
//! clamp and the depth conversion) that the clamp-bytecode workflow builds
//! with Microsoft's compilers, since Wine cannot compile them.

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

/// One of `victory-gpu`'s shaders whose bytecode is embedded: `<name>.hlsl`
/// beside `<name>_bytecode.rs`, which is written from `<name>.dxil` and
/// `<name>.dxbc`.
pub(crate) struct Shader {
    pub name: &'static str,
    /// The first lines of `<name>_bytecode.rs`.
    header: &'static str,
}

/// The argument clamp (`cargo xtask embed-clamp`).
pub(crate) const CLAMP: Shader = Shader {
    name: "clamp",
    header: "//! The argument clamp shader (`clamp.hlsl`) compiled by Microsoft's\n\
             //! compilers on a Windows runner. Written by `cargo xtask embed-clamp`.\n\
             //! `SOURCE_HASH` names the source it was built from, and other bytes are\n\
             //! never used.\n",
};

/// The conversion between 16-bit and 24-bit depth (`cargo xtask
/// embed-depth`).
pub(crate) const DEPTH: Shader = Shader {
    name: "depth",
    header: "//! The depth conversion shader (`depth.hlsl`) compiled by Microsoft's\n\
             //! compilers on a Windows runner. Written by `cargo xtask embed-depth`.\n\
             //! `SOURCE_HASH` names the source it was built from, and other bytes are\n\
             //! never used. No bytes and a hash of zero: not built yet.\n",
};

/// An array of bytes laid out as rustfmt lays it out, so that formatting
/// the workspace leaves the written file, and the hash the asset policy
/// pins, alone.
fn array(data: &[u8]) -> String {
    if data.is_empty() {
        return String::from("&[]");
    }
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

/// The text of `<name>_bytecode.rs` for bytecode built from a source of
/// the hash `source`.
fn text(shader: &Shader, source: u64, dxil: &[u8], dxbc: &[u8]) -> String {
    format!(
        "{}\n\
         pub(crate) const SOURCE_HASH: u64 = 0x{source:016x};\n\
         pub(crate) const DXIL: &[u8] = {};\n\
         pub(crate) const DXBC: &[u8] = {};\n",
        shader.header,
        array(dxil),
        array(dxbc)
    )
}

/// The text of `<name>_bytecode.rs`.
fn render(shader: &Shader, source: &[u8], dxil: &[u8], dxbc: &[u8]) -> String {
    text(shader, fnv(source), dxil, dxbc)
}

/// The text of `<name>_bytecode.rs` before the shader was ever built: no
/// bytes, and a hash that names no source, so `victory-gpu` asks the
/// system's compiler and the test of the embed fails until `run` wrote
/// the real one.
#[cfg(test)]
fn missing(shader: &Shader) -> String {
    text(shader, 0, &[], &[])
}

/// Writes `<name>_bytecode.rs` into `src`, the folder that holds
/// `<name>.hlsl`, from `<name>.dxil` and `<name>.dxbc` in `folder`. Nothing
/// is written unless both are what their names say.
pub fn run(shader: &Shader, src: &Path, folder: &Path) -> Result<()> {
    let name = shader.name;
    let source = std::fs::read(src.join(format!("{name}.hlsl")))
        .with_context(|| format!("reading {name}.hlsl"))?;
    let dxil = bytecode(&folder.join(format!("{name}.dxil")), &[b"DXIL"])?;
    // Shader model 5 code is a SHEX part, and model 4 code a SHDR part.
    let dxbc = bytecode(&folder.join(format!("{name}.dxbc")), &[b"SHEX", b"SHDR"])?;
    let text = render(shader, &source, &dxil, &dxbc);
    std::fs::write(src.join(format!("{name}_bytecode.rs")), &text)
        .with_context(|| format!("writing {name}_bytecode.rs"))?;
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

    use super::{CLAMP, DEPTH, Shader, fnv, missing, parts, render, run};

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
            render(&CLAMP, &source, &dxil, &dxbc) == embedded,
            "the text differs"
        );
    }

    /// As for the clamp: `victory-gpu` uses the prebuilt depth conversion
    /// only when its hash names the current `depth.hlsl`, and Wine cannot
    /// compile that shader either (measured: "E5005: Method 'Load' is not
    /// defined on type 'RWByteAddressBuffer'"), so a title's unordered
    /// writes to a 16-bit depth texture never reach the depth texture
    /// there.
    ///
    /// While this fails the embed is missing or stale: compile `depth.hlsl`
    /// on a Windows runner (`fxc /T cs_5_1 /E main /Fo depth.dxbc` and
    /// `dxc -T cs_6_0 -E main -Fo depth.dxil`, as the clamp bytecode
    /// workflow does for `clamp.hlsl`), run `cargo xtask embed-depth
    /// <folder>` and enter the policy numbers it prints. It must stay red
    /// until then; `victory-gpu`'s device test of the conversion says
    /// "skipped" meanwhile.
    #[test]
    fn embedded_depth_bytecode_matches_the_current_source() {
        let source = std::fs::read(gpu_src().join("depth.hlsl")).unwrap();
        let embedded = std::fs::read_to_string(gpu_src().join("depth_bytecode.rs")).unwrap();
        assert_eq!(
            written_hash(&embedded),
            fnv(&source),
            "no bytecode of this depth.hlsl is embedded: compile it on a Windows runner \
             and run `cargo xtask embed-depth`"
        );
    }

    /// The embedded file is exactly what embed-depth writes from two
    /// containers it accepts, or, before the shader was ever built, the
    /// form without bytes: nothing in it was edited by hand.
    #[test]
    fn embedded_depth_bytecode_is_what_embed_depth_writes() {
        let source = std::fs::read(gpu_src().join("depth.hlsl")).unwrap();
        let embedded = std::fs::read_to_string(gpu_src().join("depth_bytecode.rs")).unwrap();
        let dxil = written_array(&embedded, "DXIL");
        let dxbc = written_array(&embedded, "DXBC");
        if dxil.is_empty() && dxbc.is_empty() {
            assert!(missing(&DEPTH) == embedded, "the text differs");
            assert_eq!(written_hash(&embedded), 0);
            return;
        }
        assert!(parts(&dxil).unwrap().contains(b"DXIL"), "the DXIL array");
        assert!(parts(&dxbc).unwrap().contains(b"SHEX"), "the DXBC array");
        assert!(
            render(&DEPTH, &source, &dxil, &dxbc) == embedded,
            "the text differs"
        );
    }

    /// The form without bytes is laid out as rustfmt leaves it, names no
    /// source, and reads back as two empty arrays.
    #[test]
    fn the_form_without_bytes_names_no_source() {
        let text = missing(&DEPTH);
        assert!(text.ends_with(
            "pub(crate) const SOURCE_HASH: u64 = 0x0000000000000000;\n\
             pub(crate) const DXIL: &[u8] = &[];\n\
             pub(crate) const DXBC: &[u8] = &[];\n"
        ));
        assert_eq!(written_hash(&text), 0);
        assert_eq!(written_array(&text, "DXIL"), Vec::<u8>::new());
        assert_eq!(written_array(&text, "DXBC"), Vec::<u8>::new());
        assert_ne!(fnv(b""), 0, "no source hashes to the value that names none");
    }

    /// The same FNV-1a as `victory-gpu`'s `indirect::fnv`. That crate's
    /// `indirect::tests::fnv_matches_embed_clamp` holds these vectors too
    /// (the published ones for the 64-bit hash), so the two cannot drift
    /// apart without one of the tests failing.
    #[test]
    fn fnv_matches_the_gpu_crates_source_hash() {
        assert_eq!(fnv(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv(b"foobar"), 0x8594_4171_f739_67e8);
    }

    /// Runs embed-clamp on the two given files and says whether it wrote.
    fn embed(name: &str, dxil: Option<&[u8]>, dxbc: Option<&[u8]>) -> (bool, Option<String>) {
        embed_as(&CLAMP, name, dxil, dxbc)
    }

    /// Runs the embed of `shader` on the two given files and says whether
    /// it wrote.
    fn embed_as(
        shader: &Shader,
        name: &str,
        dxil: Option<&[u8]>,
        dxbc: Option<&[u8]>,
    ) -> (bool, Option<String>) {
        let file = shader.name;
        let dir = scratch(name);
        let (src, folder) = (dir.join("src"), dir.join("out"));
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(src.join(format!("{file}.hlsl")), b"void main() {}\n").unwrap();
        let put = |file: &Path, data: Option<&[u8]>| {
            if let Some(data) = data {
                std::fs::write(file, data).unwrap();
            }
        };
        put(&folder.join(format!("{file}.dxil")), dxil);
        put(&folder.join(format!("{file}.dxbc")), dxbc);
        let result = run(shader, &src, &folder);
        let written = std::fs::read_to_string(src.join(format!("{file}_bytecode.rs"))).ok();
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

    /// Each shader is embedded from files of its own name into a module of
    /// its own name, under its own first lines: the clamp's files are not
    /// taken for the depth conversion.
    #[test]
    fn each_shader_is_embedded_under_its_own_name() {
        let dxil = container(&[(b"SFI0", &[0; 8]), (b"DXIL", &[1, 2, 3, 4])]);
        let dxbc = container(&[(b"RDEF", &[]), (b"SHEX", &[5, 6, 7, 8])]);
        let (ok, written) = embed_as(&DEPTH, "depth-good", Some(&dxil), Some(&dxbc));
        assert!(ok, "two containers of the right kinds");
        let written = written.expect("depth_bytecode.rs");
        assert!(written.starts_with("//! The depth conversion shader (`depth.hlsl`)"));
        assert!(written.contains("`cargo xtask embed-depth`"));
        assert_eq!(written_hash(&written), fnv(b"void main() {}\n"));
        assert_eq!(written_array(&written, "DXIL"), dxil);
        assert_eq!(written_array(&written, "DXBC"), dxbc);
        assert_eq!(
            embed_as(&DEPTH, "depth-missing", None, Some(&dxbc)),
            (false, None)
        );

        // The clamp's inputs in the folder do not serve the other shader.
        let dir = scratch("depth-from-clamp");
        std::fs::write(dir.join("depth.hlsl"), b"void main() {}\n").unwrap();
        std::fs::write(dir.join("clamp.dxil"), &dxil).unwrap();
        std::fs::write(dir.join("clamp.dxbc"), &dxbc).unwrap();
        assert!(run(&DEPTH, &dir, &dir).is_err());
        assert!(!dir.join("depth_bytecode.rs").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
