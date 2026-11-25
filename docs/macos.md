# macOS port status

This file tracks the work to run VIctory on Apple Silicon Macs. No game files are used or needed for any step that is marked as done.

## Done on 24 November 2026

The whole workspace builds natively on macOS (aarch64, Homebrew Rust 1.100). The Windows-only parts are gated with `cfg(windows)`, and a few small stubs stand in for them. The text entry dialog is cancelled on other hosts, and a relaunch request stops the process. The `translate` command runs natively; only its `--validate` and `--pipelines` checks need Windows, and they say so. On a Mac M4 the test suite gives 162 passed, 0 failed and 10 ignored. The ignored tests read their own test binary as a PE file or use Windows paths, so they only run on Windows.

A native build proves that the platform code (formats, shader recompiler, PE analysis and most of the HLE layer) is portable. It does not run a title, because the loader maps a Windows PE image into the process and calls Windows APIs.

## Done on 24 November 2026: the Windows build under Wine

VIctory now cross builds on the Mac for `x86_64-pc-windows-gnullvm` and runs under Wine 11.14. The `windows-gnu` target cannot be used, because Rust keeps its thread-locals in OS TLS slots there and the loader only saves and restores static TLS, so std aborted when a thread started. Homebrew has no llvm-mingw, so `tools/macos/gnullvm-cc.sh` links with Homebrew clang and lld against the mingw-w64 runtime.

Run the tests under Wine with this command.

```sh
WINE=/path/to/wine tools/macos/test-wine.sh --workspace --no-fail-fast
```

The loader probe passes, so the stand-in title is mapped, bound, delay loaded and run with its thread callbacks. The HLE tests pass (86 of 86), and `victory.exe --help` runs. Wine lacks three network exports (`WSAConnectByList`, `GetAddrInfoExA` and `WinHttpGetProxySettingsEx`). VIctory does the first two itself from functions Wine has, for loopback only, and answers the third without the host. Two kinds of test still fail under Wine.

- Seven shader tests need a D3D12 device with shader model 6.6. The Wine build has no D3D12 that works on Metal, so this is the GPU gap that D3DMetal has to fill.
- The xtask policy test starts git from inside Wine. Run `cargo xtask policy` natively instead.

## D3DMetal results, 24 November 2026

With the GPTK cask (`brew install --cask gcenx/wine/game-porting-toolkit`, Wine 7.7 with the D3DMetal of GPTK 1.1), VIctory gets a D3D12 device on Metal, and all 69 shader tests pass on an M4. Metal's sin and cos return 0 for NaN and infinity, where the console GPU returns NaN, so the translator now passes that NaN through. D3DMetal does not check the DXIL container hash (it does use it: see "D3DMetal's shader cache and the HASH part" below).

Wine 7.7 is too old for the rest of VIctory. The console memory tests (xmem and dstorage) and the display test fail on it, and it also lacks `WSAConnectByNameA`. Wine 11.14 passes all of them, but D3DMetal from GPTK 4.0 beta 3 crashes in libsystem_pthread on Wine 11.14, because D3DMetal needs Apple's or CrossOver's patched Wine. A Wine that is new enough for VIctory and also works with D3DMetal is the open problem.

```sh
WINE="/Applications/Game Porting Toolkit.app/Contents/Resources/wine/bin/wine64" WINEDLLOVERRIDES=winedbg.exe=d tools/macos/test-wine.sh --workspace --no-fail-fast
```

## CrossOver Wine with D3DMetal, 24 November 2026

`tools/macos/build-crossover-wine.sh` builds CodeWeavers' CrossOver 26.3 Wine (Wine 11.0) from its published source into `target/wine-cx`, using x86_64 FreeType and GnuTLS from `DEPS`. Copy the `redist/lib` folder of Apple's "Evaluation environment for Windows games" over `target/wine-cx/lib`, and run the tests with this command.

```sh
WINE="$HOME/Library/Caches/victory-macos/target/wine-cx/bin/wine" WINEDLLOVERRIDES="d3d12,d3d12core,dxgi=b" tools/macos/test-wine.sh --workspace --no-fail-fast
```

On an M4 with D3DMetal 4.0 beta 3 this gives 260 passed, 4 failed and 4 ignored. Every loader, HLE, console memory and shader test passes on the same Wine. The policy test fails only because it runs git inside Wine. Three GPU tests show real D3DMetal differences that still need work.

- `buffer_data_reaches_the_host_texture`: fixed. The upload was queued as a copy into title memory, and that check uses VirtualQuery, which cannot see D3DMetal's mappings under Wine, so the copy was skipped. It is now queued as a fill of the list's upload memory.
- `argument_clamp_builds`: fixed. Wine's HLSL compiler (vkd3d-shader) cannot compile the clamp shader, so the `clamp bytecode` workflow compiles `clamp.hlsl` with FXC and DXC on a Windows runner, and `cargo xtask embed-clamp` embeds the result with a hash of the source.
- `a_rejected_blob_gives_an_empty_library`: fixed. D3DMetal accepts any pipeline library bytes, so the cache file now carries a mark and a checksum that VIctory checks itself.

After these fixes the whole suite passes under CrossOver Wine with D3DMetal on an M4: 343 passed, 0 failed and 4 ignored.

## What a test run did not check

A test that lacks something it needs passes after printing one line that starts with `skipped:` and names the test. The line is written past the test harness's capture, so `tools/macos/test-wine.sh ... 2>&1 | grep 'skipped: '` lists everything a run left out. A switch per need turns the skip into a failure. These are all the switches the tree reads; `tools/macos/test-wine.sh` lists each one at its top and hands each on to the tests, and `test_wine_lists_and_hands_on_every_require_switch` (`xtask/tests/shell_scripts.rs`) fails when a switch in the sources is missing from the script or from this list.

Three are set by a run that is meant to prove something on a Mac:

- `VICTORY_REQUIRE_GPU=1`: the host's D3D12 device with shader model 6.6. Without it every device test of `victory-gpu`, `victory-shader`, `victory-hle` and `apps/victory` passes after `skipped: no host D3D12 device`.
- `VICTORY_REQUIRE_DXIL=1`: the DXIL validator, described below. Without it each validating shader test prints `skipped: DXIL validation: ...`.
- `VICTORY_REQUIRE_CLAMP=1`: an argument clamp for indirect draws and dispatches. Wine's HLSL compiler cannot build `clamp.hlsl`, so on a Mac the clamp exists only as the bytecode embedded by `cargo xtask embed-clamp`, which is used while its source hash matches `clamp.hlsl`. Where the bytecode is older than the source and the host cannot compile it, `argument_clamp_builds` prints `skipped: the prebuilt clamp bytecode is older than clamp.hlsl; ...`, and a title's indirect draws would all be dropped ("no argument clamp: indirect execution dropped"). With the switch that is a failure.

The other six name something a Mac does not have or cannot promise, and are not part of the proving run:

- `VICTORY_REQUIRE_HLSL=1`: a host HLSL compiler that builds the shaders some device tests of `victory-gpu` write themselves. Wine's compiler is vkd3d-shader's, which does not, so seven tests print `skipped: the host HLSL compiler cannot build this test shader`: `draw::a_depth_texture_written_through_unordered_views_reads_as_depth`, `draw::a_texture_written_in_one_format_reads_in_another_through_a_placed_view`, `draw::root_views_reach_their_copy_and_read_zeros_past_it`, `draw::unordered_1d_and_3d_textures_keep_their_writes`, `bindless::heap_views_are_current`, `bindless::heap_views_follow_writes_through_another_mapping` and `memory::lists_share_a_table_and_copies_keep_their_views`. What they cover (depth twins, unordered 1D and 3D textures, root views, bindless heap views and the shared page table) therefore has no device test under D3DMetal. With the switch they fail on a Mac.
- `VICTORY_REQUIRE_WARP=1`: the WARP software device beside the hardware one. D3DMetal under Wine has no WARP, so on a Mac the device tests run on the hardware adapter only, and each prints `skipped: no SM 6.6 WARP device`.
- `VICTORY_REQUIRE_DIFF=1`: the console compiler (`VICTORY_CONSOLE_DXC`) and a PC `dxc.exe` (`VICTORY_PC_DXC`) for the differential shader tests, which print `skipped: no console compiler (VICTORY_CONSOLE_DXC) or no PC compiler (VICTORY_PC_DXC)`.
- `VICTORY_REQUIRE_NTFS=1`: a temporary folder on NTFS, whose limit of 1024 hard links to a file one save-game test runs into on purpose. The Wine prefix lies on APFS, which has no such limit, so on a Mac `a_kept_blob_past_the_link_limit_is_copied` prints `skipped: no NTFS link limit: ...`; the copy it is about is checked on every file system by `a_kept_blob_that_cannot_be_linked_is_copied`.
- `VICTORY_REQUIRE_IDLE=1`: a machine with nothing else to do. One DirectStorage test (`a_page_list_filled_after_its_request_starts`) needs a thread of its own to run within 8 ms; where the host starts it later and the request therefore fails as designed, it prints `skipped: the host ran the thread that fills the page list only after ... ms`.
- `VICTORY_REQUIRE_PC_GPU=1`: a device that gives the results of the PC hardware the device tests were measured on. Where D3DMetal is known to give another result (see "Open differences under D3DMetal"), the test that shows it checks that the device is D3DMetal and that the result is the measured one, prints `skipped: D3DMetal ...` and passes. With this switch it fails instead. On a Mac the switch therefore fails four shader tests today; it is for finding out whether a new D3DMetal has closed a difference, not for the proving run.

The proving run on a Mac is this command, once as it stands and once with `--release` after `--workspace`:

```sh
VICTORY_DXIL_UNIX="$HOME/Library/Caches/victory-macos/dxc/v1.9.2609/dxil.dll" VICTORY_REQUIRE_DXIL=1 VICTORY_REQUIRE_GPU=1 VICTORY_REQUIRE_CLAMP=1 tools/macos/test-wine.sh --workspace --no-fail-fast
```

A total reported as "GPU, DXIL validation and the clamp required" is a total of this command. It does not require HLSL, WARP, the differential compilers, NTFS, an idle machine or the PC hardware's results: the `skipped:` lines of its output say which tests those are.

Every device test of `victory-shader` gets its device and its validation through `crate::testing`, upstream's tests included, so these switches cover all of them. D3DMetal is recognised by the process running under Wine and the adapter naming itself "AMD Compatibility Mode", the name D3DMetal presents; a result that differs on any other device fails.

## The DXIL validator

The shader tests can have Microsoft's DXIL validator (`dxil.dll`) check every container the translator emits, and compare the container hash VIctory computes with the one the validator signs. The DLL is not part of this repository and must never be committed. It ships with the Windows SDK and with the releases of Microsoft's open-source DirectXShaderCompiler. Keep it outside the repository, for example in `~/Library/Caches/victory-macos/dxc/`, and name it by its macOS path.

```sh
VICTORY_DXIL_UNIX="$HOME/Library/Caches/victory-macos/dxc/v1.9.2609/dxil.dll" VICTORY_REQUIRE_DXIL=1 VICTORY_REQUIRE_GPU=1 tools/macos/test-wine.sh -p victory-shader --no-fail-fast
```

`test-wine.sh` passes the file to the tests as `VICTORY_DXIL`, the `Z:` path Wine knows it by. On Windows, set `VICTORY_DXIL` yourself, or leave it unset to use the newest Windows SDK's copy. Without a validator each validating test prints `skipped: DXIL validation: ...`; the native macOS build cannot load the DLL at all and always prints that line, so `VICTORY_REQUIRE_DXIL=1` fails there.

Wine loads the validator and runs it. Measured on 24 November 2026 on an M5 (macOS 27.0 beta) with the CrossOver Wine build and D3DMetal, with the `dxil.dll` of DirectXShaderCompiler release v1.9.2609 (SHA-256 `64be2368211d257a038a7d4ea6bb3e24c878eb8d588c7b8414fccf759b69c2ac`):

- The first run of the command above gave 149 passed, 1 failed and 1 ignored. The failure was `emitted_containers_carry_the_parts_each_stage_needs`, with the validator's message "Not all elements of output SV_Position were written." The test's mesh program declared a position and exported none.
- That was also a defect of the translator. The validator wants every channel of each system-value output of a vertex, pixel or mesh-vertex signature written by some store. That covers `SV_Position`, the clip and cull distances, the render-target and viewport indices, the depths, `SV_Coverage` and `SV_StencilRef`. It makes exceptions for `SV_Target` and for user semantics, and it does not check per-primitive mesh outputs this way. The translator now refuses a program that leaves such an output unwritten or half written ("a system-value output the program does not write in full"), because what the console does with it is not known.
- After the fix the same command gives 152 passed, 0 failed and 1 ignored, with no `skipped: DXIL validation` line. The lines that remain are 19 of `skipped: no SM 6.6 WARP device` and 4 of `skipped: no console compiler ...`. So every container these tests emit was validated and carries the hash the validator signs, but the differential tests did not run.
- A container with one flipped bitcode byte makes the validator's `Validate` call fail (0x80004005). One with a wrong feature bit in `SFI0` gets a verdict ("Container part 'Feature Info' does not match expected for module."). `validator_accepts_an_emitted_module_and_rejects_a_corrupted_one` checks both.

The validator checks nothing in a run without `VICTORY_REQUIRE_DXIL=1` and a `dxil.dll`, and the device tests compare nothing where there is no device. A run that is meant to prove something sets `VICTORY_REQUIRE_DXIL=1` and `VICTORY_REQUIRE_GPU=1`.

## Route A: next steps

The first playable route is the upstream Windows build running under Wine, with D3D12 going to Metal through Apple's D3DMetal. DXMT cannot be used because it covers D3D11 only. Rosetta 2 runs the title's x86-64 code and has supported AVX2 since macOS 15.

The steps are as follows.

1. Install rustup and add the `x86_64-pc-windows-gnu` or `x86_64-pc-windows-msvc` target. The MSVC route through `cargo-xwin` downloads the Microsoft CRT and SDK under Microsoft's licence.
2. Build `apps/victory` and the `tests/fixtures/hle-probe` stand-in title for Windows.
3. Run the probe under Wine to test the loader and HLE layer without any game files.
4. Add D3DMetal from Apple's Game Porting Toolkit and check that VIctory creates a D3D12 device with mesh shaders and shader model 6.6.

## Route B: a native macOS host

Later work replaces the Windows host underneath the title. That needs a Rosetta x86-64 process that maps the PE image, a macOS version of the host pass-through in `victory-hle`, a Metal backend for `victory-gpu`, and DXIL to Metal conversion with Apple's Metal shader converter. This only makes sense after Route A shows where things break.

## Running VIctory on a Mac

Run `./setup.sh` from the top of the repository. It checks the Mac, installs the Homebrew tools, builds checksum-verified x86_64 GMP, Nettle, GnuTLS and FreeType, builds CrossOver Wine, adds D3DMetal from the Game Porting Toolkit in Downloads, and builds VIctory. The only manual step is downloading the toolkit from Apple. A clean run on an M4 on 24 November 2026 finished without help, and the full suite then passed (343 passed, 0 failed).

The setup's last step is `tools/macos/check-setup.sh`, which can be run at any time. It needs no game: `victory --version` under Wine is what proves that VIctory starts, and the step fails only if that does. It then shows `victory config`. The `victory.toml` the setup copies from `victory.example.toml` names no game, and `config` says `NOTE: game_dir is not set` for it and succeeds. A `game_dir` that is set and is not there, or a folder inside the repository, makes `config` end with status 1 (`MISSING: game_dir ... is not a directory`, `POLICY VIOLATION: ...`); the step shows that with what to change and still passes, since it is the user's file that needs correcting and not the setup. Until 24 November 2026 the example named `D:\victory-data\game` and the setup required `config` to succeed, so the upstream change that makes a missing `game_dir` an error stopped a fresh setup at its last step with "VIctory did not start under Wine". A `victory.toml` copied from the old example still holds that line: remove it or name the game's folder in the `Z:\` form.

### The directory VIctory runs in

`tools/macos/victory.sh` starts `victory.exe` in the directory the launcher itself was started in, which is what happens on Windows, where `victory` is started in the user's directory and nothing changes it. Two things follow.

- A path on the command line is taken from that directory. `cd <the game's folder>; <repository>/tools/macos/victory.sh run --execute game.exe` finds `game.exe` there, and the folder is the title's `G:` drive and its current directory.
- Every path a title opens without a drive or a folder is read and written in that directory.

Until 24 November 2026 the launcher changed into the repository before it started Wine. Measured with the stand-in title under the CrossOver Wine build: a start by file name from the title's folder ended with `Error: reading hle-probe-exe.exe` / `File not found. (os error 2)`, and a title started by its full path was told `Z:\Users\...\<repository>` as its current directory, read `standin-relative.txt` from the repository's root and wrote `standin-relative-out.txt` there, among the sources, where nothing a title writes may be (`asset-policy.md`).

A title never runs in a VIctory source tree now (this repository, or any folder with its `.victory-root` file above it). Started from inside one, `run` starts the title in the title's own folder, which is the first case above, and says `victory.sh: started inside the VIctory source tree; the title runs in its own folder, <folder>`; the executable is still the one the path named from where the launcher was started. If the title's folder is inside a source tree as well, the launcher ends with status 1 and `victory.sh: refusing to run a title in <folder>: it is inside a VIctory source tree ...` before it builds or starts anything. Commands that run no title (`config`, `inspect`, `translate` and the others) stay in the user's directory.

The config file is `VICTORY_CONFIG` (a relative name is taken from the user's directory), else `victory.toml` in the user's directory, else the repository's `victory.toml`, where `setup.sh` puts it.

The stand-in wine of `xtask/tests/shell_scripts.rs` checks the launcher's side of this, and `run_by_file_name_alone_runs_the_title_in_its_own_folder` and `run_leaves_the_titles_relative_paths_in_the_directory_it_was_started_in` (`apps/victory/tests/run.rs`) check under Wine what `victory.exe` and a title make of the directory they are started in. No test starts the launcher itself against the real Wine; that was done by hand, with the results above.

## After merging upstream, 24 November 2026

Upstream VIctory (`940d32f`) was merged into this port. What follows is what the merge changed for a Mac, measured on an M5 (macOS 27.0 beta) with the CrossOver Wine build.

- `victory run --execute` now checks the CPU for AVX2 before it starts a title, and refuses with "this CPU does not support AVX2" where it is missing. Under Rosetta 2 a process sees AVX2 only if it was started with `ROSETTA_ADVERTISE_AVX=1`. Without the variable the check refuses, and nine tests of `apps/victory/tests/run.rs` failed on it; with it the check passes and the stand-in title runs. `tools/macos/victory.sh` already set the variable for the title's own sake, and `tools/macos/test-wine.sh` now sets it too unless the caller has set it, so the tests go through the check as a real run does. `VICTORY_SKIP_CPU_CHECK=1` turns the check off; neither script sets it, and a Mac does not need it.
- A build that `victory run` knows by its hash needs its startup archive beside the executable (`appdata0.rpf` for Series 1.0.2.1). With `--execute` the file must be readable and start with the four bytes of an RPF9 header, or the run ends with "Cannot read startup archive" or "Invalid RPF9 header" before the title is mapped. The stand-in title of the tests is no known build ("build unknown" in its report), so the tests need no such file; the check itself is tested on files of a few bytes that the test writes.
- `victory run --execute` no longer starts a title when a DLL it imports is neither in its folder, nor the host's, nor VIctory's: it names the DLLs and stops, and `--unimplemented zero` runs it anyway. `--strict` takes nothing from the host, so `--strict --execute` now stops there for any title that imports a host DLL. The test of the first call of a missing import therefore puts stand-in DLLs that export nothing into the title's folder.
- `victory translate --cache` fills the shader cache `run` reads, and with `--pipelines` the pipeline library from the title's shader pack. Both need the Windows build, so on a Mac they run under Wine (`tools/macos/victory.sh translate --cache ...`); the native build refuses them by name.
- `victory shader-info --effects` lists the effects of shader databases, and `shader-info` lists shader packs. Both run natively.
- `VICTORY_FIDELITYFX_DIR`, or `fidelityfx_dir` under `[paths]` in the config file, names a folder with the PC FidelityFX DLLs. `victory config` shows it. It has not been tried under D3DMetal.

## Open differences under D3DMetal

Four are known in the shader tests, all measured on 24 November 2026 on an M5, macOS 27.0 beta (26A5406e), D3DMetal 4.0 beta 3, with the proving command of "The DXIL validator" above, each confirmed in a copy of the test program under a name D3DMetal had never seen (an empty shader cache). In each the translator emits what upstream's tests check on PC hardware and the host does something else. The tests keep their assertions for every other host; under D3DMetal they check that the result is the one written here, print a `skipped: D3DMetal ...` line and pass, and fail with `VICTORY_REQUIRE_PC_GPU=1`. None of the four was changed in the translator: a lowering that differs per host does not exist, and one changed for every host could not be checked here against the PC hardware that is the reference.

- 64-bit atomics on a typed view (M5, macOS 27.0 beta (26A5406e), D3DMetal 4.0 beta 3). `image_atomics_of_64_bits`: a compute program with a 64-bit `image_atomic_*` on an `R32G32_UINT` texture (DXIL `atomicBinOp.i64` and `atomicCompareExchange.i64` on a typed resource from the heap) does nothing at all. D3DMetal reports the capability (`AtomicInt64OnTypedResourceSupported` and `AtomicInt64OnDescriptorHeapResourceSupported` are both true), creates the pipeline and completes the dispatch without an error, and the texture is left as it was. A probe that was not kept (the same program with a plain `buffer_store_dword` to a raw buffer beside the atomics) showed that the store is lost as well, with a maximum, an exchange or all three atomics in the program, while the same program without an atomic stores, and the same atomics as 32-bit ones on an `R32_UINT` texture work and return the old values. What a title would see: upstream's test says the title's alpha prepasses use these atomics; under D3DMetal such a pass writes nothing, neither its 64-bit keys nor anything else the same compute program stores, and nothing reports it, since the capability is claimed. VIctory does not detect this at run time yet.
- The bias of a biased sample in a compute shader (M5, macOS 27.0 beta (26A5406e), D3DMetal 4.0 beta 3). `image_addresses_of_16_bits_and_clamps`: `image_sample_b` in a compute program (DXIL `sampleBias` with the implicit derivatives SM 6.6 gives compute shaders) is sampled at the level the derivatives alone give. Biases of 1, -1 and 2 all return level 1 where the hardware returns 2, 0 and 3. A probe that was not kept showed the same with a 32-bit bias and with a constant bias, so it is not the half-float address; and the same instruction in a pixel program does take its bias under D3DMetal (a bias of 2 moved a magnified, point-filtered lookup to the minifying, linear filter; a bias of -2 did not). The clamp of `image_sample_d_cl` and the gathers of that test are right. What a title would see: only compute programs that sample with a bias are affected, and they read one mip level per unit of bias too sharp or too blurred; pixel programs are not affected.
- The texels of the host's own gather (M5, macOS 27.0 beta (26A5406e), D3DMetal 4.0 beta 3). `gathers_at_a_level_match_the_hardware`: the gathers the translator computes itself (`image_gather4_l`, `_l_o`) match the expected texels. `image_gather4_lz` is the host's gather (DXIL `textureGather`), and D3DMetal picks its four texels from the product of coordinate and size rounded to f32, where RDNA2 takes the exact product (and WARP truncates, which the test already allowed for). The two differ only for a coordinate within one f32 rounding step of the threshold between two footprints: 12 of the test's 16 level-0 lanes are built to sit there and come back one texel to the right. What a title would see: nothing, short of a gather aimed at exactly such a coordinate.
- The container hash is not checked. `stub_pipeline_is_created` asserts that the runtime refuses a container with a zero or a wrong digest; D3DMetal creates the pipeline for both. See the next section for what D3DMetal does with the digest instead.

Closed, or never a difference:

- Denormal operands of 32-bit maximums and minimums. Before the merge of upstream this was open: D3DMetal's `FMax`/`FMin` treat a 32-bit denormal as equal to zero and return the second operand (`v_max_f32(1 ulp, -0)` gave -0; 72 cases of a test over the format's edges). Upstream's lowering now reads a 32-bit denormal operand as a zero of its sign on the bits before the maximum or minimum (the console's float mode flushes them), so the host's own denormal mode no longer shows. `min_max_and_clamp_match_the_hardware` runs the two- and three-operand forms over both zeros, the smallest and largest denormals, NaNs and infinities against the model of the hardware, and passes under D3DMetal.
- Signed zero in minimums: closed, it was never a difference. This section used to say that a -0 input reaches the shader as +0 under D3DMetal, so that `v_min_f32(-0, +0)` gives +0, and blamed Apple's shader converter or Metal's fast math. Neither was the cause. Every translated container carried the same all-zero `HASH` part, and D3DMetal then ran the minimum's container with the code of the maximum's, which has the same length and was created just before it (see the next section). The maximum of -0 and +0 is +0, which is the value that was seen.
- `negative_zero_inputs_reach_the_shader` shows it directly: a shader stores the registers its `v_min_f32` or `v_mul_f32` read beside the result, and every -0 comes back as `0x80000000`, with the result's sign right as well. It passes under D3DMetal with the all-zero `HASH` part too, so the inputs never lost their sign.
- `max_and_min_order_negative_zero_below_positive_zero` no longer skips `v_min_f32` and `v_min3_f32` under Wine, and `f16_max_and_min_order_negative_zero_below_positive_zero` checks the half-float forms. The three tests passed in the debug and the release build, 5 times each alone and 5 times each in the whole test program, every run in a copy of the test program under a name D3DMetal had never seen (an empty shader cache), and again under the same name (a filled one).
- The check the other way: the same test programs built with the `HASH` part zeroed again, each run under a new name, fail with `hardware: v_min_f32 [80000000, 0, 80000000, 0]`, left 0, right 2147483648, and with `v_min_f16 [8000, 0, 8000, 0] = 0x0, want 0x8000` (and the same for `v_min3_f16`): 2 runs of each test alone and 1 of the whole program, in the debug and in the release build. Run as the whole program they fail three more device tests that way, while `negative_zero_inputs_reach_the_shader` passes.

## D3DMetal's shader cache and the HASH part

A DXIL container has a `HASH` part: four bytes of flags and a 16-byte digest of the shader. D3D12 on Windows does not check it. D3DMetal uses it, and that decides whether a translated shader runs as itself. What follows is measured behaviour (M5, macOS 27.0 beta (26A5406e), D3DMetal 4.0 beta 3), not a reading of D3DMetal's code, and the exact key is not known.

- D3DMetal keeps compiled shaders in the process and on disk, in a folder per program name: `$(getconf DARWIN_USER_CACHE_DIR)/d3dm/<name of the exe>/shaders.cache`. A test program's first run under a new name takes 5 to 15 seconds for the three signed-zero tests, and a second run under the same name under 1.2 seconds.
- Two containers with the same digest and the same length in bytes are one shader to it: the second gets the code compiled for the first, in the same run or in a later one. Containers of different lengths were not seen to collide.
- VIctory's translator wrote 20 zero bytes there until 24 November 2026. Programs that differ in one instruction give containers of one length, so one ran as the other: `v_or_b32` and `v_xor_b32` gave the result of `v_and_b32`, `v_min_f32` that of `v_max_f32`, and (found with the fix, not measured again here) a `v_cmp_class_f16` program ran as a `v_max3_f32` one from another test.
- The folder is named after the program, and cargo names a test program after its feature set, so `-p victory-shader`, `-p victory-gpu -p victory-shader` and `--workspace` each had a cache with its own history. That is why the same test passed in one of those runs and failed in another, and why a result could outlive the build that caused it.
- The translator now writes its own digest: flags 0 and the MD5 of every other part's name, length and contents (`container::shader_hash`). It is not claimed to be what Microsoft's compiler writes. `translated_containers_have_distinct_nonzero_shader_hashes` (on every host, macOS included) checks that no translated container has a zero digest, that different containers have different digests and that the same program gets the same one. `shaders_of_one_container_length_keep_their_own_results` checks on the device that such programs keep their own results.
- A build with the digests runs correctly even under a program name whose folder was filled by a build without them: its containers no longer match the old entries.

When a device test's result under D3DMetal looks impossible, run a copy of the test program under a new file name before believing it. The old folders can be deleted; D3DMetal makes them again.
