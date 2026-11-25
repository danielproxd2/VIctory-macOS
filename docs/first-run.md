# The first real run on a Mac

This file is for the first time a real game is started with VIctory on a Mac, and for every time after that when a start fails. It lists the stages of a run in the order they happen, the exact text VIctory prints when a stage refuses to go on, and what to check first. Every quoted message is in the source at the place named beside it; nothing here was measured with a real game, because none has been run on this port yet. The tests cover each stage with stand-in programs written for this repository.

## Before the first start

Keep the game outside the repository, for example in `~/games/<the game>`. Then run the checks, which start no title code:

```sh
tools/macos/first-run.sh ~/games/<the game>/<name>.exe
```

The script stops at the first check that fails and says what to do. It runs three checks:

1. The build: the Wine that `setup.sh` built, Apple's D3DMetal inside it, and `victory --version` starting under it.
2. `victory inspect` on the executable: VIctory reads it as a Windows program.
3. `victory run` without `--execute`: the executable is prepared, mapped and its imports are bound. The report names every DLL of the package that is missing and every import that nothing provides.

When all three pass it prints the command that starts the title, which has this form:

```sh
cd '<the game's folder>' && '<repository>/tools/macos/victory.sh' run --execute '<name>.exe' 2>&1 | tee '<reports>/run.log'
```

What the checks print is kept in a new folder for each run under `~/Library/Logs/victory` (or `VICTORY_FIRST_RUN_DIR`): `version.txt`, `inspect.txt` and `bind.txt` hold the reports, and the `.log` file beside each holds the log lines. The script refuses to write inside a VIctory source tree, and it refuses an executable that lies inside one.

### Where the logs are

- `run.log`, from the command above, gets everything VIctory prints. The report of the run goes to standard output: the lines `build`, `mapped at`, `title`, `console`, `title dll`, `bound` and, at the end, `title exited with <code>`. The log lines and crash reports go to standard error. A log line looks like `<time>  WARN victory_gpu::pipeline: shader does not translate; ...`.
- The crash log is `crashes.log` in the title's data folder, beside its `storage` and `temp` folders: `~/Library/Application Support/victory/data/<title id>/crashes.log` unless `data_dir` or `VICTORY_DATA_DIR` names another folder. `first-run.sh` prints the full path. Every crash report and every call of an import that nothing provides is added to it, so it also holds earlier runs.
- A plain build logs `info` and above, whatever `VICTORY_LOG` asks for. The debugging build logs more, and it is the only one that says why each dropped draw was dropped. Put `VICTORY_FEATURES=diagnostics` in front of the launcher's path to build and start it; `VICTORY_LOG=debug` then adds the debug lines (expect a very large log). The launcher builds VIctory again when the features change.
- Wine's own messages are off (`WINEDEBUG=-all`). Set `WINEDEBUG` yourself to see them, for example `WINEDEBUG=err+all`.

## The stages of a start, and what each refusal says

The stages are in the order of `tools/macos/victory.sh` and `apps/victory/src/run.rs`. A line that starts with `Error:` ends the run with status 1 before the title has run; the stage that printed it is the one to look at.

### 1. The launcher

| What is printed | What it means and what to check |
| --- | --- |
| `victory.sh: no command given, and no game is started by default.` | The launcher was started without arguments. It builds and starts nothing. |
| `no Wine at <path>: run tools/macos/build-crossover-wine.sh first` | The Wine build is missing. Run `./setup.sh` again; finished steps are skipped. |
| `D3DMetal is missing: copy the redist/lib folder ...` | Wine is there without Apple's D3DMetal. Download the Game Porting Toolkit `.dmg` into `~/Downloads` and run `./setup.sh` again. |
| `victory.sh: started inside the VIctory source tree; the title runs in its own folder, <folder>` | Not a failure. The launcher was started from inside the repository, where a title must not run, so the title is started in its own folder. |
| `victory.sh: refusing to run a title in <folder>: it is inside a VIctory source tree ...` | The game's folder is inside the repository (or another copy of it). Move the game out. |
| `victory.sh: cannot read the config file ... from macOS; ...` | `VICTORY_CONFIG` names a file the launcher cannot find. Check the path; in the `Z:\` form `/Users/you/x` is `Z:\Users\you\x`. |
| lines of a compiler, then the launcher ends | The build of `victory.exe` failed. Run `./setup.sh` again. |

### 2. The processor check

`Error: this CPU does not support AVX2, which Xbox Series titles require; set VICTORY_SKIP_CPU_CHECK=1 to bypass this check (for example under CPU emulation such as Intel SDE)` (`run.rs`, `check_cpu_features_with`).

Under Rosetta 2 a process sees AVX2 only when it was started with `ROSETTA_ADVERTISE_AVX=1`. The launcher sets that variable, and `setup.sh` requires macOS 15, the first version whose Rosetta has AVX2. If the message appears, check that the title was started through `tools/macos/victory.sh` and not by starting `victory.exe` under Wine by hand. Do not set `VICTORY_SKIP_CPU_CHECK=1` on a Mac: the title would then run into its first AVX2 instruction instead.

### 3. The configuration and the cache folders

| What is printed | What to check |
| --- | --- |
| `Error: parsing <file>` | `victory.toml` is not valid TOML, or has a key VIctory does not know. The lines after it say where. |
| `Error: VICTORY_CONFIG points at <file>, which does not exist` | The variable names a file that is not there. |
| `Error: refusing to use <path> for game-derived data: it is inside the VIctory source tree (<root>). Keep game files, caches and outputs outside the repository (see docs/asset-policy.md)` | `cache_dir` or `data_dir` (or the variable that overrides it) is inside the repository. `tools/macos/victory.sh config` marks each such folder with `POLICY VIOLATION:`. |
| `Error: creating <folder>` | The folder cannot be made. Check the path and its permissions. |
| ` WARN ... FidelityFX: <why>; the title's own DLLs run` | `fidelityfx_dir` names a folder that cannot be used. The run goes on. With no folder named the line is ` INFO ... no FidelityFX folder (VICTORY_FIDELITYFX_DIR): a title's own FidelityFX DLLs run`. The host's FidelityFX has not been tried under D3DMetal; leave it unset for a first run. |

`victory config` itself is not part of a run. `NOTE: game_dir is not set` is no fault: `run` takes the executable from the command line. `MISSING: game_dir ... is not a directory` makes `config` fail and does not stop `run`.

### 4. The executable

| What is printed | What to check |
| --- | --- |
| `Error: reading <path>` and `File not found. (os error 2)` | The path does not name a file from the directory the launcher was started in. Use the command `first-run.sh` printed. |
| `Error: ... is a DLL, which cannot be started as a title (without --execute it is mapped and bound)` | The file is a DLL of the package and not its executable. |
| an `Error:` from reading the image, before any `build` line | The file is not a Windows program VIctory can prepare. `tools/macos/victory.sh inspect <path>` says more. Check that the copy is whole. |

### 5. The build and its startup archive

The report's first line is `build          <label> (<hash>)` for an executable VIctory knows by its BLAKE3 hash (the table is `KNOWN_BUILDS` in `crates/victory-game/src/lib.rs`; today it holds one entry, `Series 1.0.2.1`), and `build          unknown (<hash>)` for any other.

- `build unknown` is not a refusal. The title is run all the same. The startup-archive check below is not made for it, so a missing archive shows up later, inside the title. If you expected a known build, compare the hash with the table: an executable that was patched, cut short or taken from another version of the game has another hash.
- For a known build, a start with `--execute` checks the archive the title needs first (`appdata0.rpf` for `Series 1.0.2.1`), beside the executable:
  - `Error: Cannot read startup archive <path>. Check that the file from your package is present and readable.`
  - `Error: Invalid RPF9 header in <path>. Restore the startup archive from your package.`

Both end the run before the title is mapped. Copy the file from your own package again.

### 6. The manifest and the drives

- `manifest       not taken as it stands: <what>`: something in `MicrosoftGame.config` was not read. The run goes on. Without a title id the title's storage is kept under the id `00000000`.
- `title          <id>, storage <folder> (<size> MB)`: where the title's saved data goes. Under the launcher that is below `~/Library/Application Support/victory/data` unless a `data_dir` is named.
- The title's folder is its `G:` drive and `<data>/<id>/temp` is its `T:` drive (the log lines `virtual drive drive=G root=...`).

### 7. The title's own DLLs

`missing dlls   <names> not in <folder>, and neither the host nor VIctory provides it: the package is incomplete; copy every file of the package next to the executable`

With `--execute` this is followed by `Error: the title's own DLLs are missing: ... (\`--unimplemented zero\` runs it anyway)` and the run ends. `first-run.sh` stops on the first line already. Copy the whole package. `--unimplemented zero` is for finding out how far a title gets without them, not for playing.

A DLL the host policy allows (the C and C++ runtimes) is never taken from the package. Where Wine's own copy lacks a function, it is listed as an import nothing provides (next stage).

### 8. Imports

`bound          <image>: <n> hle, <n> host, <n> title, <n> missing`, followed by one line `  missing      <dll>!<function>` for each import nothing provides.

Missing imports do not stop the start. The title runs until it first calls one. Then:

- the log has ` ERROR victory_loader::thunks: unimplemented import called dll=<dll> symbol=<function>`;
- `crashes.log` gets `VIctory: unimplemented import <dll>!<function> called`;
- the process ends at once, without `title exited with`.

That function is the next thing to implement. With `--unimplemented zero` such a call returns 0 and is logged once (` WARN ... unimplemented import called, returning 0`).

` WARN victory_hle::modules::kernel: a host DLL outside the host policy: refused dll="<name>" from=<callers>`: the title asked for a DLL at run time that is neither in its folder nor one the host may provide. The title is told that the DLL is not there (`ERROR_MOD_NOT_FOUND`). It is logged once for each name.

### 9. The title runs: the CPU

From here on the title's code runs. A fault is reported on standard error and in `crashes.log`:

`VIctory: unhandled exception <code> at <where><detail>`, then two lines of registers, then the frames `  #0 <where>`, `  #1 ...`, and a line `  host: <CPU>, Windows <version>`.

| Code and detail | What it means |
| --- | --- |
| `0xc000001d ... (unsupported AMD instruction <name>: code <bytes>)` | An instruction of the console's AMD processor that the host does not have and VIctory does not do itself. `clzero`, `monitorx`, `mwaitx` and `rdpru` are done by VIctory; the report names the others it recognises (`movntss`, `movntsd`). |
| `0xc000001d ... (code <bytes>)` | An illegal instruction VIctory does not recognise. On a Mac the first suspect is an instruction Rosetta 2 does not translate; the bytes say which. |
| `0xc0000005 ... (read <address>: ...)`, `write` or `execute` | An access violation, with what is mapped at the address. |
| `0xe06d7363 ... (C++ <type>: <message> thrown)` | A C++ exception nobody caught. |
| `VIctory: panic on thread ...` | A fault in VIctory itself. |

`<where>` is `<image>+<offset>`. An offset in the title's image is the same from run to run, so two reports can be compared.

### 10. The title runs: the GPU

Each of these is a warning, logged once unless it says otherwise, and the run goes on.

| Log line | What it means |
| --- | --- |
| `no host D3D12 device; command lists will not run` | D3DMetal gave no device. Nothing is drawn. Check that the launcher was used (it sets the DLL overrides) and that D3DMetal is in the Wine build. |
| `the host GPU does not fuse multiply-adds: results of the console's fused v_fma and v_fmac can differ in kind here` | A measured property of the host. Not a failure. |
| `shader does not translate; its pipelines' draws are dropped`, with `stage=`, `hash=` and `error=` | The translator refused one shader. `error=GFX10 code at <offset>: <what> is not supported` names an instruction or construct the translator does not handle yet; `error=malformed shader: <what>` and `error=GFX10 code at <offset>: <reason>` mean the container or its code was not understood; `error=the translator panicked` is a fault in the translator. Every draw that uses the shader is dropped. `tools/macos/victory.sh translate --list <the game's folder>` lists all such shaders without running the title. |
| `no host pipeline: <reason> (logged once)` | VIctory did not ask the host for this pipeline. The reasons: `only vertex, mesh, pixel and compute stages are translated`; `no vertex or mesh shader`; `the host GPU has no mesh shaders (or no shader model 6.6)`; `the host GPU has no 64-bit atomics on typed heap resources`; `graphics state not kept (plain description)`. |
| `host root signature refused`, `host pipeline refused`, `host mesh pipeline refused`, each with `error=` | The host (D3DMetal) refused what VIctory asked for. The shaders are named; the pipeline's draws are dropped. |
| `making a host pipeline panicked; it has none and its draws are dropped` | A fault in VIctory while making one pipeline. |
| `no host pipeline library: pipelines are not kept` | Pipelines are made again on every start. Slower, not wrong. |
| `the prebuilt argument clamp is older than clamp.hlsl and is not used ...`, `prebuilt argument clamp refused`, `no argument clamp: indirect execution dropped` | The first two say that the embedded clamp was not used. The third says that there is none: every indirect draw and dispatch is dropped. Wine cannot compile the shader, so on a Mac the fix is `cargo xtask embed-clamp` with bytecode from the clamp workflow. |
| `no depth conversion: 16-bit depth written through unordered views does not reach the depth texture`, and `depth conversion: <compiler text>` | The one shader VIctory still compiles at run time did not compile under Wine. What a title writes to a 16-bit depth texture through unordered views is lost. This path has no device test under D3DMetal. |
| `a draw or dispatch dropped: no dynamic slots for its tables`, `no dynamic slots for root views: dropped`, `a shader reads a root view the title did not set: dropped`, `a dispatch without a host pipeline dropped`, `indirect mesh dispatches dropped` | One draw or dispatch that could not be run, by cause. |
| `more than D3D12 has; ignored`, with `what=` and `count=` | The title set more render targets, viewports or scissor rectangles than D3D12 allows. The call is ignored. |
| `D3D12: <text>` | A message of the host's debug layer, which is on only in the debugging build with `VICTORY_D3D12_DEBUG` set. |

Every five seconds of presented frames there is a line

`<n> fps, <n> ms/frame (...), <n> draws/frame, dropped <n> without a host pipeline (<n> being made) and <n> indirect; <n> root-view slots/frame`

`dropped ... without a host pipeline` counts draws per frame that did not reach the host, and `being made` is the number of pipelines still being built: a number that falls to 0 while the title loads is normal, one that stays is not. In the debugging build (`VICTORY_FEATURES=diagnostics`) the line is followed by up to forty lines `dropped <n>/frame without a host pipeline: <why>`, most frequent first, and that is where a black or incomplete picture is explained.

### 11. The end

`title exited with <code>` is the last line of a title that returned from its entry point, and the launcher ends with that code (its low eight bits). A run that ends without it ended in a crash report, an unimplemented import or the title's own `exit`.

## What to send with a report

`run.log`, `crashes.log`, `bind.txt`, the `build` line with its hash, the macOS version and the name of the Game Porting Toolkit `.dmg`. None of these may be committed to the repository: what VIctory says about a game is derived from the game.

## Where this port differs from upstream on purpose

A title that works with upstream VIctory on Windows can behave differently here for these reasons. They are listed so that a difference seen in a first run can be matched to its cause.

Host differences, measured under D3DMetal (details in `macos.md`, "Open differences under D3DMetal"):

- 64-bit atomics on typed views do nothing, and the compute pass that uses them writes nothing at all. D3DMetal reports the capability, so no warning is logged. Upstream's tests say the title's alpha prepasses use them.
- A biased sample in a compute shader ignores its bias.
- The host's gather picks other texels for coordinates on a texel boundary.
- D3DMetal does not check a shader container's hash, and it accepts any bytes as a pipeline library.
- Wine's HLSL compiler cannot build VIctory's two run-time shaders. The argument clamp is embedded as bytecode for that reason; the depth conversion is not (stage 10).
- D3DMetal refuses a texture whose castable formats include one it has no Metal pixel format for (`R10G10B10_XR_BIAS_A2_UNORM`, `R8G8_B8G8_UNORM`, `G8R8_G8B8_UNORM`). VIctory leaves those three out of the lists there; a view in one of them over a texture of another format is refused.
- Sixteen device tests of `victory-gpu` compile shaders Wine's HLSL compiler cannot build. They run under D3DMetal only in a test run that names a DXC (`VICTORY_PC_DXC_UNIX`, `macos.md`, "Test shaders built by DXC"), and then on DXIL where upstream's run on shader model 5.1 bytecode.

Differences in VIctory itself:

- The cache folder is not interchangeable with an upstream build's. The pipeline library file is sealed (a mark and a checksum in front), shader cache entries carry a length and a hash, and the cache keys differ. Each side discards the other's files and makes them again.
- Translated shaders carry a digest of their own in the `HASH` part, where upstream writes one fixed value. D3DMetal uses the digest to tell shaders apart.
- XAsync: the final status of a call is visible before the provider's cleanup has returned, where upstream shows it after. A title that polls the status from another thread can see the call end slightly earlier.
- Values of a title that are out of range are refused instead of passed on: a root signature that is not a whole container (`E_INVALIDARG`), a command signature with more arguments than its record can hold, root parameters past 64 values (ignored), more render targets, viewports or scissors than D3D12 has (the call is ignored), upload memory over 1 GiB (null), a page-mapping batch that wraps, a rectangle list that expands past 4 GiB, and shader containers with a fourth thread-id component, an export patch outside the code, a thread-group size outside D3D12's limits or shared memory past 32 bits (the shader does not translate).
- A loose range of title memory is answered by any existing host copy that covers it, where upstream makes a new copy unless start and length match exactly.
- `victory config` ends with status 1 for a folder inside the repository or a `game_dir` that is set and is not there; `run --execute` refuses a DLL; a title's exit code becomes the launcher's.
- The launcher never runs a title inside the source tree (stage 1).

## What has not been tried

- No real game has been started on this port. Everything from stage 5 on is tested with a stand-in executable that creates no D3D12 device.
- No test starts the launcher against the real Wine; that is done by hand.
- `fidelityfx_dir` has not been tried under D3DMetal.
- A pipeline library written under another macOS or D3DMetal version is not refused by the host or by the seal. If pictures are wrong after an update of macOS or the toolkit, delete `host-pipelines.bin` in the cache folder and D3DMetal's own cache (`macos.md`, "D3DMetal's shader cache and the HASH part").
