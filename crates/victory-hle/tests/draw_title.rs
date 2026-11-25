//! The stand-in title that draws (tests/fixtures/draw_title.rs) against
//! the console runtime a title is bound to, twice: with the runtime's
//! exported entry points looked up as the loader looks up a title's
//! imports, and as `title_draw.dll`, an image that has them in its import
//! table, mapped and bound by victory-loader. Either way the title
//! creates the device through `D3D12XboxCreateDevice`, draws one triangle
//! with two console shader containers and reads the frame back, and the
//! frame is checked texel by texel.
//!
//! Both need the host's D3D12 device: without one each passes after
//! `skipped: no host D3D12 device`, and fails with `VICTORY_REQUIRE_GPU=1`.
//! No game is needed for either.

#[path = "../../../tests/fixtures/draw_title.rs"]
mod title;
// Without a device nothing is run or checked.
#[cfg_attr(not(windows), allow(dead_code))]
#[path = "../../../tests/fixtures/draw_title_expect.rs"]
mod expect;

#[cfg(windows)]
#[path = "../../victory-loader/tests/common/mod.rs"]
mod common;

#[cfg(windows)]
mod bound {
    use victory_hle::HleResolver;
    use victory_loader::{ImportResolver, Provenance, Resolution};
    use victory_pe::ImportSymbol;

    /// The console's graphics runtime, by the name titles import it by.
    pub const RUNTIME_DLL: &str = "d3d12_xs.dll";
    /// What the title imports from it; the last is data.
    pub const IMPORTS: [&str; 3] = [
        "D3D12XboxCreateDevice",
        "D3D12SerializeRootSignature",
        "IID_ID3D12Device",
    ];

    /// The address the resolver binds `d3d12_xs.dll!name` to, which must
    /// be our runtime's own.
    pub fn export(resolver: &HleResolver, name: &str) -> usize {
        let symbol = ImportSymbol::Name {
            hint: 0,
            name: name.into(),
        };
        match resolver.resolve(RUNTIME_DLL, &symbol) {
            Resolution::Found {
                address,
                provenance,
            } => {
                assert_eq!(provenance, Provenance::Hle, "{name}");
                address
            }
            Resolution::Missing => panic!("{RUNTIME_DLL}!{name} is not served"),
        }
    }
}

/// The title's frame holds its triangle when the title is handed the
/// runtime's exports, bound as a title's imports are: the device comes
/// from `D3D12XboxCreateDevice`.
#[test]
fn a_title_draws_a_triangle_through_the_runtimes_exports() {
    let Some(_gpu) = victory_gpu::test_support::gpu() else {
        return;
    };
    #[cfg(windows)]
    {
        let resolver = victory_hle::HleResolver::default();
        let [create, serialize, iid] = bound::IMPORTS.map(|name| bound::export(&resolver, name));
        // SAFETY: the runtime's exports, with the signatures it gives them.
        let runtime = unsafe {
            title::Runtime {
                create_device: std::mem::transmute::<usize, title::CreateDevice>(create),
                serialize_root_signature: std::mem::transmute::<usize, title::SerializeRootSignature>(
                    serialize,
                ),
                device_iid: iid as *const title::Iid,
            }
        };
        let (vs, ps) = (expect::vertex_shader(), expect::pixel_shader());
        // SAFETY: the runtime's own entry points and interface id.
        let report = unsafe { title::run(&runtime, &vs, &ps, expect::CLEARED) };
        expect::check(&report);
    }
}

/// The same from inside a title image: `title_draw.dll` is mapped by the
/// loader, its imports from `d3d12_xs.dll` are bound to our runtime by
/// the resolver `victory run` uses, and its export runs the title, which
/// reports the frame it read back.
#[test]
fn a_loaded_title_image_draws_a_triangle() {
    let Some(_gpu) = victory_gpu::test_support::gpu() else {
        return;
    };
    #[cfg(windows)]
    {
        use victory_loader::{
            ImportResolver, Provenance, Resolution, UnimplementedPolicy, prepare,
        };
        use victory_pe::ImportSymbol;

        /// Passes every question on and keeps who answered the runtime's.
        struct Recording<'a> {
            inner: &'a dyn ImportResolver,
            answers: std::cell::RefCell<Vec<(String, Option<Provenance>)>>,
        }
        impl ImportResolver for Recording<'_> {
            fn resolve(&self, dll: &str, symbol: &ImportSymbol) -> Resolution {
                let answer = self.inner.resolve(dll, symbol);
                if dll.eq_ignore_ascii_case(bound::RUNTIME_DLL) {
                    let provenance = match answer {
                        Resolution::Found { provenance, .. } => Some(provenance),
                        Resolution::Missing => None,
                    };
                    let mut answers = self.answers.borrow_mut();
                    answers.push((symbol.to_string(), provenance));
                }
                answer
            }
        }

        let file = std::fs::read(common::fixture("title_draw.dll")).unwrap();
        let prepared = prepare(&file).unwrap();
        let cache = tempfile::tempdir().unwrap();
        let image = victory_loader::load(&prepared, cache.path()).unwrap();
        let hle = victory_hle::HleResolver::default();
        let recording = Recording {
            inner: &hle,
            answers: Default::default(),
        };
        // An import nobody serves traps when called: the title must not
        // get past one unnoticed.
        image
            .bind_imports(&recording, UnimplementedPolicy::Trap)
            .unwrap();
        // The image imports exactly these from the runtime, and each is ours.
        let mut answers = recording.answers.into_inner();
        answers.sort_by(|a, b| a.0.cmp(&b.0));
        let mut wanted = bound::IMPORTS.map(|name| (name.to_owned(), Some(Provenance::Hle)));
        wanted.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(answers, wanted);

        type Run = unsafe extern "system" fn(
            *const u8,
            usize,
            *const u8,
            usize,
            *const [f32; 4],
            *mut title::Report,
        ) -> u32;
        let run = image
            .export("title_draw_run")
            .expect("the image exports title_draw_run");
        let (vs, ps) = (expect::vertex_shader(), expect::pixel_shader());
        let mut report = title::Report::new();
        // SAFETY: the image declares the export with this signature, its
        // imports are bound, and the pointers are to what it reads and
        // writes.
        let failed = unsafe {
            std::mem::transmute::<usize, Run>(run)(
                vs.as_ptr(),
                vs.len(),
                ps.as_ptr(),
                ps.len(),
                &expect::CLEARED,
                &mut report,
            )
        };
        expect::check(&report);
        assert_eq!(failed, 0);
    }
}
