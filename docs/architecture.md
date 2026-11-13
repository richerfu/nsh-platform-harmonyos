# The HarmonyOS backend (ArkUI)

`crates/nsh-platform-harmonyos` renders NSH apps on HarmonyOS / OpenHarmony (API 20+, `arm64-v8a`) with ArkUI custom nodes
and lays text out with native `OH_Drawing_Typography`. Each `NshSurface` owns one `NodeContent`, one engine thread and
one QuickJS VM. How apps use it: [the README](../README.md), [embedding.md](embedding.md).

## Building and checking

The backend builds for `aarch64-unknown-linux-ohos` with the OpenHarmony NDK (API 20+); off `target_env = "ohos"` the
crate is empty. The environment cargo needs (the NDK's clang as `CC` and linker, its sysroot for bindgen, its
libclang) is the one `nsh-platform-harmonyos-cli`'s `sdk.rs` sets:

```sh
OHOS_NDK_HOME=… nsh package examples/counter --target harmonyos -v   # the whole path: runner, library, project
cargo check -p nsh-platform-harmonyos --target aarch64-unknown-linux-ohos --features dev   # with the NDK's CC_* / linker set
```

Features:
- Default / `dev`: API 20 baseline (`OH_ArkUI_*` custom nodes, `OH_Drawing_Typography`, `NativeVSync`, `AVPlayer`,
  system `TextInput` / `TextArea`, and `ArkWeb`); `dev` connects to `nsh dev`.
- `api-23`: native accessibility providers, state descriptions and reduced-motion observation.
- `api-26`: includes `api-23` plus named custom accessibility actions.

## On NSH's extension points

- **The environment**: `Environment { os: Os::Android, platform: Some("harmonyos"), .. }` (`app/runtime.rs`). The
  platform is not one of NSH's `Os`; Android's props and behavior (a system back gesture) are the closest, and
  `platform.name` says where the app runs.
- **The linked app**: the runner, a `cdylib` named `nsh_platform_harmonyos` (the N-API module registers under the
  backend's crate name, which the library's name must match), defines `__nsh_platform_harmonyos_app` with the
  backend's `app!` macro (`linked.rs`): the UI sets and, for an app written in Rust, its root from NSH's generated
  `nsh_rust_app`. `NshSurface.runApp()` starts it; the UI sets serve TSX bundles too. A TSX app with Rust crates of
  its own (`nsh.json` `"rust"` without `"root"`) has no `nsh_rust_app`: the runner depends on each crate by path and
  serves `nsh/base`'s UI set, then each crate's `#[native_module]` `registry()` (`app!(sets = [..])`), while the
  bundle is a raw file of the HAP as for any TSX app.
- **Mount items** (`src/render/mount.rs`): `Shader`, `ShaderTransition` and `NavShader` are implemented; `ShaderInput`
  is a no-op because ArkUI's `RenderNode` texture producer reports native content updates directly (including native
  input and scroll changes between engine batches); and the terminal-only projections (`TreeOrder`, `ProjectStates`)
  are ignored.
- **Streaming assets**: the video player and web views read large files through `services::assets::StreamAssets`,
  this backend's trait over `nsh_engine::Assets`.

## Architecture

- **One `NshSurface` per embedded view** (`src/app/`): the host creates `new NshSurface(this.getUIContext())` and places
  `surface.content` (`NodeContent`) inside a standard ArkUI `ContentSlot`. `@nsh/harmonyos` binds each window session
  explicitly from the provided `UIContext` (`UIAbilityContext` and `Window`) in a standard `UIAbility` without a custom
  base class or process-global context guessing.
- **Commits, frames and frame-rate arbitration** (`src/render/`, `src/app/frame_rate.rs`, `src/app/runtime.rs`): the
  engine submits each `MountBatch` through a N-API threadsafe function to the ArkUI UI thread. `OH_ArkUI_PostFrameCallback`
  (falling back to per-surface `NativeVSync` before attachment) drives demanded UI frames on the UI thread while `FrameRate`
  arbitrates UI animation demand (`Uncapped` or reduced-rate `Capped`, e.g. `30` Hz) and shader compositor demand as the
  single owner of `OH_NativeVSync_SetExpectedFrameRateRange`; idle surfaces and native `Scroll` gestures schedule zero
  synthetic keepalive callbacks on the UI thread. Intentional policy: while any GPU shader holds an active vote
  (`shader_votes > 0`), `FrameRate` pins the shared `NativeVSync` range to full panel `hz`, overriding concurrent
  `UiDemand::Capped` caps until the last shader vote drops (per-consumer UI throttling, if ever needed alongside a live
  shader, belongs in `Animator` rather than lowering the shared VSync handle).
- **Device tests** (`nsh-platform-harmonyos-cli/src/test/`, `src/app/testing.rs`, `src/render/mount/testing.rs`): the
  CLI runs NSH's public `TestRun` and `Agent`, with a `RemoteScene` over the dev protocol. The device's `Model`
  observes mount batches and provides selectors, storage, network mocks and the manual clock. ArkUI answers native
  geometry queries on its UI thread; the CLI injects system input through hdc (`uitest` / `uinput`) and captures the
  marked surface. A separate `.nshtest` bundle and per-open data directories isolate tests from the normal app.
  Quit retires the engine before removing its files; the CLI stops the host and removes its reverse forward.
- **Shaders and composition** (`src/render/shader/`, `src/render/composition/`, `src/interop/`): a per-surface
  `nsh-compositor` worker thread owns an EGL / GLES 3 (`glow`) context and renders outside the ArkUI UI thread across
  three lanes:
  1. **Native ArkUI path**: standard views, transforms, opacity, single blur / backdrop-blur and native blend modes stay
     entirely on ArkUI `C-API` nodes without texture export or GPU worker involvement. `Attribute::Blur` and
     `Attribute::BackdropBlur` pass through `native_blur_value(css_radius, scale)`, inverting ArkUI/Rosen's
     physical-pixel `ConvertRadiusToSigma(r) = 0.57735 * r + 0.5` mapping so native blur matches CSS `sigma = radius * scale`.
  2. **Direct GPU path** (`fill.rs`, `worker.rs` `Composite`): standalone fill shaders render directly into a child C
     `XComponent` surface, while content shaders, non-blur filter chains, `ShaderTransition` and `NavShader` export the
     wrapped subtree via ArkTS `BuilderNode` texture export (`RENDER_TYPE_TEXTURE` -> `OH_NativeImage` OES external
     texture) into a presentation `XComponent` without CPU readback or synchronous wrapper-preview `PixelMap` snapshots.
     Navigation `transitionShader` capture is demand-driven per active transition—reusing an existing `Composite`'s GPU
     output immediately or exporting only the leaving picture root until its snapshot lands (`100` ms fallback deadline
     to standard motion), causing zero idle texture exports or backdrop-blur `Scene` promotions. Fill shaders retain
     `dirty = true` with bounded backoff retries across transient GL/window errors (failing immediately without retry on
     shader compilation errors and preserving any previous bound window on `Window` bind errors).
  3. **Surface `Scene` path** (`scene.rs`, `draw.rs`): subtrees requiring CSS `mask-image` (`masks[0]`, matching
     upstream mobile backends), multi-filter / backdrop-filter combinations, shader backdrop sampling, or blend modes
     unsupported natively are promoted into a minimal stable `Scene` root (`scene_root_for` keeps non-overflowing
     `mask-image` nodes on their own subtree and stops backdrop walks at the nearest ancestor whose opaque rounded-rect
     background covers the projected sample bounds). `Scene::prepare` builds the composition plan via `paint_plan()` /
     `build_paint_tree()`, commits processed state only on success under an RAII `preparing` guard, marks `Unsupported`
     requirements ineligible with one diagnostic, retries transient errors with bounded backoff (`0` / `100` / `500` ms),
     and unconditionally clears `scene_prepared` / `scene_root` on rollback. `nsh-compositor` reads exported `OES`
     planes into bounded RGBA bitmaps, composites CPU masks / backdrop filters (`TileMode::Clamp`) and blends, runs any
     attached GLES passes, presents onto the `Scene` root wrapper's custom draw canvas, and logs throttled
     `[NSH] scene-stats` counters (active planes, readback bytes, upload bytes).
     Platform limitation: embedded separate hardware surfaces (`Video` / `ArkWeb` `XComponent`s) inside a `BuilderNode`
     texture export depend on system surface composition and are not guaranteed to appear in exported textures.
- **Typography** (`src/text/`): the engine thread shapes and measures paragraphs with `ohos-drawing-binding`
  (`FontCollection::shared()`, `Typography`) and hands recorded native drawing commands and selection geometry to the UI
  thread.
- **Controls and media** (`src/input/`, `src/scroll/`, `src/media/`, `src/interop/`): `Input` uses the system
  `TextInput` / `TextArea` BuilderNode; `Scroll` and `List` use native ArkUI `Scroll` (with on-demand `Refresh`);
  `WebView` uses `ArkWeb`; `Video` uses a C `XComponent` surface and an `AVPlayer` worker.
- **Navigation** (`src/navigation/`): HarmonyOS is a native-header platform, as iOS and Android: each `Screen` draws its own
  bar (back button, title) and places only the `headerLeft` / `headerTitle` / `headerRight` items in it; a screen's
  `header` slot (the bar on platforms without a native header) is never shown, as UIKit's and the Android toolbar's
  bars never place it. Screens and modals report `onInsets` as the other backends do: a screen's top inset is 0 below
  a visible, opaque header, else the safe area's top.
- **Hit areas**: `hitSlop` and overflowing children widen `NODE_RESPONSE_REGION`, whose width and height are
  percentages of the node's size (`100` is the node itself), not ratios.
