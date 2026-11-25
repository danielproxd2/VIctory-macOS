# macOS port status

This file tracks the work to run VIctory on Apple Silicon Macs. No game files are used or needed for any step that is marked as done.

## Done on 24 November 2026

The whole workspace builds natively on macOS (aarch64, Homebrew Rust 1.100). The Windows-only parts are gated with `cfg(windows)`, and a few small stubs stand in for them. The text entry dialog is cancelled on other hosts, a relaunch request stops the process, and the `translate` command reports that it needs Windows. On a Mac M4 the test suite gives 162 passed, 0 failed and 10 ignored. The ignored tests read their own test binary as a PE file or use Windows paths, so they only run on Windows.

A native build proves that the platform code (formats, shader recompiler, PE analysis and most of the HLE layer) is portable. It does not run a title, because the loader maps a Windows PE image into the process and calls Windows APIs.

## Done on 24 November 2026: the Windows build under Wine

VIctory now cross builds on the Mac for `x86_64-pc-windows-gnullvm` and runs under Wine 11.14. The `windows-gnu` target cannot be used, because Rust keeps its thread-locals in OS TLS slots there and the loader only saves and restores static TLS, so std aborted when a thread started. Homebrew has no llvm-mingw, so `tools/macos/gnullvm-cc.sh` links with Homebrew clang and lld against the mingw-w64 runtime.

Run the tests under Wine with this command.

```sh
WINE=/path/to/wine tools/macos/test-wine.sh --workspace --no-fail-fast
```

The loader probe passes, so the stand-in title is mapped, bound, delay loaded and run with its thread callbacks. The HLE tests pass (86 of 86), and `victory.exe --help` runs. Wine lacks three network exports (`WSAConnectByList`, `GetAddrInfoExA` and `WinHttpGetProxySettingsEx`), and VIctory now fails those calls cleanly. Two kinds of test still fail under Wine.

- Seven shader tests need a D3D12 device with shader model 6.6. The Wine build has no D3D12 that works on Metal, so this is the GPU gap that D3DMetal has to fill.
- The xtask policy test starts git from inside Wine. Run `cargo xtask policy` natively instead.

## D3DMetal results, 24 November 2026

With the GPTK cask (`brew install --cask gcenx/wine/game-porting-toolkit`, Wine 7.7 with the D3DMetal of GPTK 1.1), VIctory gets a D3D12 device on Metal, and all 69 shader tests pass on an M4. Metal's sin and cos return 0 for NaN and infinity, where the console GPU returns NaN, so the translator now passes that NaN through. D3DMetal does not check the DXIL container hash.

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

A shader test that lacks something it needs passes after printing one line that starts with `skipped:` and names the test. The line is written past the test harness's capture, so `tools/macos/test-wine.sh ... 2>&1 | grep 'skipped: '` lists everything a run left out. A switch per need turns the skip into a failure.

- `VICTORY_REQUIRE_GPU=1`: the host's D3D12 device with shader model 6.6.
- `VICTORY_REQUIRE_WARP=1`: the WARP software device beside it. D3DMetal under Wine has no WARP, so on a Mac the device tests run on the hardware adapter only, and each prints `skipped: no SM 6.6 WARP device`.
- `VICTORY_REQUIRE_DXIL=1`: the DXIL validator, described below.
- `VICTORY_REQUIRE_DIFF=1`: the console compiler (`VICTORY_CONSOLE_DXC`) and a PC `dxc.exe` (`VICTORY_PC_DXC`) for the differential shader tests.

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

## Open differences under D3DMetal

- Signed zero in minimums. VIctory's `max_and_min_order_negative_zero_below_positive_zero` test (added upstream on 24 November 2026) finds that a -0 input reaches the shader as +0 under D3DMetal, so `v_min_f32(-0, +0)` gives +0 where the console gives -0. Selecting on the integer bits did not help, which points at Apple's shader converter or Metal's fast math. The minimum cases of that test are skipped under Wine until a fix is found.
