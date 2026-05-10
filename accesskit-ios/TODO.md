# accesskit-ios — implementation punch list

Per the §10.4 Layer 3 plan in the gem repo's `DESIGN.md`. Tracks
where we are in the implementation across sub-commits, with the
full landed set on top and the remaining work near the bottom.

`accesskit_macos` 0.26 is the canonical template. Its source is at
`~/.cargo/registry/src/.../accesskit_macos-0.26.0/src/`. Files
referenced below are within that path.

## Status (2026-05-10)

**Done:** scaffold (1) + state machine (2a) + query helpers (2b) +
PlatformNode + objc deps (2c) + SubclassingAdapter (3) + gpui-mobile
IosWindow integration (4) + headless end-to-end on iOS Simulator (5,
Path A).

**Remaining:** full app bundle + gpui pipeline (5b, Path B) + VoiceOver
validation (6). 5b is the main lift left — most of the work is iOS
app infrastructure (UIApplicationMain, AppDelegate, .app bundle, FFI
bridge), not Rust.

All accesskit-ios crate sub-commits live on
`philipaw/gpui-mobile` branch `gem/accesskit-ios-scaffold`. Gem-side
work lives on the gem repo's `main` branch.

## Sub-commit 1 — scaffold (DONE, `773e9d6`)

Lazy-workspaceified the philipaw/gpui-mobile fork; added
`accesskit-ios` as a sibling crate; stubbed `SubclassingAdapter` API
matching accesskit_macos 0.26. Followed by `c7c6695` adding this
TODO.md.

## Sub-commit 2a — `Adapter` state machine (DONE, `b98c426`)

`Adapter` with `Inactive { view, is_view_focused, action_handler }` →
`Placeholder { ... }` → `Active(Rc<Context>)` state machine. Mirrors
`accesskit_macos::Adapter`'s state shape, slimmed to the parts that
have no UIKit dependency: `Context` holds only `tree` +
`action_handler`, no `view` or `platform_nodes` yet. `update_if_active`,
`update_view_focus_state`, and `ensure_initialized` (manual
`Inactive→Active` driver for tests).

Five host-target unit tests cover the Layer-2 failure modes:
`Inactive::update_if_active` is a no-op (factory not invoked),
activation with full tree → Active, activation returning None →
Placeholder, Placeholder → Active on next full update,
post-activation diff (`tree=None`, `nodes=[]`) doesn't panic.
Validated by temporarily forwarding the factory in the Inactive arm;
no-op test failed as expected.

## Sub-commit 2b — query helpers (DONE, `eb730af`)

Three `Adapter` methods that mirror the iOS UIAccessibilityContainer
informal protocol's selectors but stay in pure Rust types so they're
testable without UIKit linkage:

- `accessibility_element_count` → root's filtered child count.
- `accessibility_element_at_index` → `Option<LocalNodeId>`.
- `index_of_accessibility_element` → `Option<isize>`.

All use `accesskit_consumer::common_filter`. Six new unit tests
cover positive/negative/out-of-range/unknown/synthetic-root cases.
Validated by introducing an off-by-one in `position()` → roundtrip
test failed with expected `Some(0) != Some(1)`.

## Sub-commit 2c — `PlatformNode` + objc deps (DONE, `9b0199c`)

UIAccessibilityElement subclass via `objc2::declare_class!` with
`Weak<Context>` + `accesskit_consumer::NodeId` ivars. Overrides
`accessibilityLabel`, `accessibilityTraits`, `accessibilityFrame`,
`accessibilityActivate` — minimum surface for VoiceOver to announce
and tap a Role::Button. Other roles fall through to defaults until a
later commit handles them.

`Context` gains iOS-gated fields: `WeakId<UIView>`, `MainThreadMarker`,
`RefCell<HashMap<NodeId, Id<PlatformNode>>>`. `Adapter` gains three
`*_objc` shim methods returning `*mut NSObject`. `accessibilityFrame`
returns the AccessKit bounding box as-is (assumes tree coords ==
screen coords for the spike); coordinate conversion deferred.

Validation: `cargo check --target aarch64-apple-ios-sim`. 11 host
unit tests still pass unchanged.

Deps added (iOS-target-gated): `objc2 = "0.5"`,
`objc2-foundation = "0.2"` (`NSGeometry` + `NSObject` + `NSString` +
`NSThread`), `objc2-ui-kit = "0.2"` (`UIAccessibility` +
`UIAccessibilityConstants` + `UIAccessibilityContainer` +
`UIAccessibilityElement` + `UIResponder` + `UIView`).

## Sub-commit 3 — `SubclassingAdapter` (DONE, `526abb2`)

Dynamic UIView subclassing that routes UIAccessibility selectors
through the `Adapter` without requiring the host to override the
selectors at compile time. Mirror of
`accesskit_macos::SubclassingAdapter`, ~290 lines.

Per-view-class subclasses cached in a static `Mutex<Vec>`.
`AssociatedObject` NSObject subclass holds the `Adapter` +
`ActivationHandler` + `prev_class` via `declare_class!` ivars.
Selector overrides via `objc2::declare::ClassBuilder`: `superclass`,
`isAccessibilityElement` (forced NO), `accessibilityElementCount`,
`accessibilityElementAtIndex:`, `indexOfAccessibilityElement:`.
`isAccessibilityElement` returns `objc2::runtime::Bool` (Rust `bool`
doesn't impl `Encode` for objc2 method registration).

`objc_setAssociatedObject` + `object_setClass` swap the view's class
in place; `Drop` reverses both.

Validation: `cargo check --target aarch64-apple-ios-sim`. Behavioral
validation deferred to sub-commit 5.

## Sub-commit 4 — gpui-mobile IosWindow integration (DONE, `aae7f7a`)

Repoint gpui-mobile's gpui + gpui_wgpu deps from upstream
`zed-industries/zed @ 5688167` to `philipaw/zed` branch
`gem/a11y-element-trait` so the Layer 1+2 a11y APIs are in scope.
Two unrelated upstream API drifts patched along the way (separate
commit would have been cleaner; bundled with the wiring per
philipaw's call):

- `IosPlatform`: stub `hide_cursor_until_mouse_moves` and
  `is_cursor_visible` (added to the Platform trait upstream).
- `IosDisplay::id`: widen `self.screen as u32` → `u64`
  (DisplayId::new signature widened upstream).

Layer 3 platform integration mirrors `gpui_macos`'s a11y wiring:

- New `ios/a11y.rs`: `A11yState` (initial_update + focus_inverse_map
  + pending_actions), `WindowActivationHandler`, `IosActionHandler`
  (resolves Action::Focus via inverse map → enqueues
  PendingA11yAction::Focus), and a `SendSubclassingAdapter` newtype
  asserting `Send` on the adapter (`Id<UIView>` is `!Send` by design).
- `IosWindow` gains `a11y_adapter:
  Arc<Mutex<SendSubclassingAdapter>>` + `a11y_state:
  Arc<Mutex<A11yState>>` fields. Constructed in `IosWindow::new`
  immediately after the Metal view exists, before the renderer is
  built.
- `impl PlatformWindow for IosWindow` gains
  `take_accessibility_handler` (returns the AccessibilityDrain-
  consuming closure that captures cloned Arcs of state + adapter)
  and `take_pending_a11y_actions` (drains pending_actions on each
  gpui draw).
- `IosWindow::debug_inject_a11y_action` (cfg(debug_assertions))
  bypasses UIKit and synthesizes an ActionRequest into a fresh
  IosActionHandler — Layer 2 lesson carried forward, painful to
  retrofit later.

Validation: `cargo check --target aarch64-apple-ios-sim --workspace`.
accesskit-ios's 11 host tests still pass.

## Sub-commit 5 — headless end-to-end on iOS Simulator (DONE, `2fb6250` gem-side, Path A)

Gem-side spike at `spikes/gpui-ios-a11y-headless/`. Allocates a
bare `UIView` (no UIWindow / UIApplicationMain), constructs
`SubclassingAdapter::new(view, ...)`, feeds in a `TreeUpdate` with
one Role::Button, then calls each iOS UIAccessibility selector
directly via `objc::msg_send!` and asserts the readings match what
we projected.

Output (run via `just spike-gpui-ios-a11y` against booted iPhone
17 simulator):

```
[spike] accessibilityElementCount = 1
[spike] accessibilityElementAtIndex(0) = <non-null PlatformNode pointer>
[spike] PlatformNode.accessibilityLabel = "Hello a11y"
[spike] PlatformNode.accessibilityTraits = 0x1  (UIAccessibilityTraitButton)
[spike] PlatformNode.accessibilityFrame = origin=(0,0) size=(0,0)
[spike] indexOfAccessibilityElement = 0
[spike] action handler fired: action=Click target_node=#2
[spike] accessibilityActivate = true
[spike] SubclassingAdapter dropped (class reverted)
```

Failure-mode validation per CLAUDE.md: temporarily changed the
input label to "OOPS"; the round-trip assertion failed with the
expected mismatch. Reverted before commit.

What this proves: the SubclassingAdapter → AssociatedObject →
Adapter → PlatformNode → AccessKit Tree chain is wired correctly
on real iOS — every selector returns the right value, the action
handler fires synchronously from `accessibilityActivate`, and the
class-swap-and-revert lifecycle is clean.

What this does NOT prove: gpui-mobile's IosWindow a11y wiring (no
IosWindow constructed in the spike); that gpui's element pipeline
actually emits TreeUpdates on iOS; VoiceOver behavior. Those are
sub-commit 5b's job.

## Sub-commit 5b — full iOS app bundle + gpui pipeline (TODO, Path B)

Validate gpui-mobile's IosWindow a11y wiring (sub-commit 4)
end-to-end on iOS Simulator: gpui's element pipeline → IosWindow's
`take_accessibility_handler` closure → `SubclassingAdapter::update_if_active`.

Bigger lift than sub-commit 5 because iOS apps must boot via
`UIApplicationMain` from a C `main()` with an Obj-C `UIApplicationDelegate`
that calls back into Rust through FFI. gpui-mobile's `IosPlatform::run`
is explicit: it stashes the launch callback for the FFI layer to
invoke, instead of running its own loop. So 5b can't be a bare
`fn main()`.

Template exists: `vendor/gpui-mobile/example/ios/` (main.m with
GPUIAppDelegate + UIApplicationMain, Info.plist, LaunchScreen.storyboard,
gpui_ios.h).

### Files to create

1. **`spikes/gpui-ios-a11y-preview/Cargo.toml`** — standalone
   workspace (gpui pulls zed's wgpu fork, conflicts with gem's
   workspace, same as gpui-mac-preview). `[lib] crate-type =
   ["staticlib"]`. Deps: `gpui` + `gpui_platform`
   (`philipaw/zed @ gem/a11y-element-trait`), `gpui-mobile`
   (path = vendor), `accesskit`. Add to gem's workspace `exclude`
   list parallel to `spikes/gpui-mac-preview`.

2. **`spikes/gpui-ios-a11y-preview/src/lib.rs`** (~80 lines).
   Extern-C entry points the AppDelegate calls:
   `gpui_ios_register_app()` instantiates
   `gpui_mobile::ios::current_platform`, builds `gpui::App` with it,
   stashes a `cx.open_window` callback that constructs `HelloWorld`
   (with the `LabeledButton` element from `gpui-mac-preview/main.rs`)
   and installs a `set_accessibility_handler` observer that prints
   TreeUpdates to `os_log` / NSLog. `gpui_ios_run_demo()` calls
   `app.run`.

3. **`spikes/gpui-ios-a11y-preview/ios/main.m`** (~40 lines).
   Lifted from `vendor/gpui-mobile/example/ios/main.m` with the
   USE_GPUI_RUST=undef fallback Metal view stripped and only the
   `USE_GPUI_RUST` path retained.

4. **`spikes/gpui-ios-a11y-preview/ios/gpui_ios.h`** (~10 lines).
   Declares the extern-C symbols main.m calls.

5. **`spikes/gpui-ios-a11y-preview/ios/Info.plist`** — copied from
   the example, bundle id `dev.gem.gpui-ios-a11y-preview`.

6. **`spikes/gpui-ios-a11y-preview/ios/LaunchScreen.storyboard`** —
   verbatim from the example.

7. **`spikes/gpui-ios-a11y-preview/build.sh`** (~50 lines). No
   XcodeGen dependency; hand-rolled bash that:
   - `cargo build --target aarch64-apple-ios-sim --release -p
     gpui-ios-a11y-preview` → `libgpui_ios_a11y_preview.a`.
   - `xcrun --sdk iphonesimulator clang -framework UIKit -framework
     Metal -framework Foundation -isysroot ... -arch arm64
     -mios-simulator-version-min=15 main.m libgpui_ios_a11y_preview.a
     -o GpuiIosA11yPreview.app/GpuiIosA11yPreview`.
   - Lay out the `.app` directory with `Info.plist` + (compiled)
     `LaunchScreen.storyboardc` (compile via `ibtool` from the
     iphonesimulator SDK).
   - `xcrun simctl install booted GpuiIosA11yPreview.app`.
   - `xcrun simctl launch --console booted dev.gem.gpui-ios-a11y-preview`.

8. **`justfile` recipe** `spike-gpui-ios-a11y-preview` boots the
   iPhone 17 simulator and runs `build.sh`.

### What 5b proves

Stderr handler shows TreeUpdates with the `LabeledButton`'s
Role::Button + label, frame after frame. That confirms:

- gpui's element pipeline collects nodes from `Element::accessibility()`
  overrides on iOS.
- IosWindow's `take_accessibility_handler` was actually called and
  its closure runs.
- The closure wrote to `A11yState.initial_update` and called
  `SubclassingAdapter::update_if_active` (otherwise the AT chain
  wouldn't see anything in sub-commit 6).

### Open question for 5b

Sub-commit 5's LabeledButton returns an AccessKit Node with no
`set_bounds(...)` call. PlatformNode's `accessibilityFrame` returns
ZERO. VoiceOver may not surface elements with zero rects (or may
show them at the screen origin where they're hard to interact with).
If 5b's stderr handler shows the right TreeUpdates but sub-commit
6's VoiceOver doesn't pick up the button, the next move is to teach
LabeledButton's `accessibility()` to call `node.set_bounds(...)` from
the prepaint bounds. gpui-mac-preview probably gets away with no
bounds because macOS VoiceOver is more permissive.

### Estimated effort

A full focused session. Most of the time is debugging linker flags +
the static-lib / main.m wiring + sim launch — not Rust. The Rust
portion (lib.rs) is small.

## Sub-commit 6 — VoiceOver-on-Simulator validation (TODO)

Gated on 5b's installable `.app` bundle.

`xcrun simctl ui booted accessibility_screen_navigation_enabled YES`
(or use the Simulator's Settings → Accessibility → VoiceOver toggle)
to enable VoiceOver in the booted simulator, relaunch the spike
binary via `xcrun simctl launch`, observe VoiceOver announces the
LabeledButton with the correct role + label when focused, and that
a double-tap triggers `accessibilityActivate` (which fires our
Action::Click via the IosActionHandler — same path sub-commit 5
already validated end-to-end at the headless layer).

Xcode's iOS Accessibility Inspector (Xcode → Open Developer Tool →
Accessibility Inspector → switch target picker to Simulator) gives
visual verification of the projected tree without needing VoiceOver
gestures.

## Cost recap

Original §10.4 estimate: 6-8 sittings total for sub-commits 2-6.

Done so far (5 sittings, denser than estimated):
- 1 (scaffold) + 2a/b/c (Adapter + helpers + PlatformNode) +
  3 (SubclassingAdapter) + 4 (IosWindow integration) +
  5 (headless validation, Path A).

Remaining (2 sittings estimated):
- 5b (full app bundle + gpui pipeline, Path B) — full session.
- 6 (VoiceOver validation) — short session, gated on 5b.

## Lessons carried forward

Don't re-discover these in 5b/6 or in any future iOS adapter work:

- **objc2 versioning**: stay on `objc2 = "0.5"` line +
  `objc2-foundation = "0.2"` + `objc2-ui-kit = "0.2.2"` so we
  parallel `accesskit_macos`'s pin (objc2 0.5 + objc2-app-kit 0.2).
  objc2-uikit feature flags needed:
  `[UIAccessibility, UIAccessibilityConstants,
  UIAccessibilityContainer, UIAccessibilityElement, UIResponder,
  UIView]`. objc2-foundation needs `NSThread` for
  `MainThreadMarker::new()` (without it only `new_unchecked` is
  available, which is `unsafe`).

- **`UIAccessibilityElement` is `MainThreadOnly` mutability**, not
  `InteriorMutable` like macOS's `NSAccessibilityElement`.
  `PlatformNode::new` must take a `MainThreadMarker` parameter.
  `Context` carries the `mtm` so cached PlatformNode allocation
  works without re-checking the thread.

- **`SubclassingAdapter` is `!Send`** because it holds `Id<UIView>`
  (objc Id types are !Send by design). gpui's PlatformWindow
  trait demands `Send` on the take_accessibility_handler closure.
  gpui-mobile wraps in a `SendSubclassingAdapter` newtype with
  `unsafe impl Send`. macOS handles this differently — via `unsafe
  impl Send for MacWindowState {}` on the wrapping struct.

- **iOS subclass needs `isAccessibilityElement` → NO** so UIKit
  treats the host UIView as a container of accesskit-managed
  elements rather than a leaf. Override via ClassBuilder. Method
  signature returns `objc2::runtime::Bool`, NOT Rust `bool` (bool
  doesn't impl `Encode` for objc2 method registration).

- **`Action::Default` doesn't exist in accesskit 0.24**. Just use
  `Action::Click`. `ActionRequest` fields are `target_tree` +
  `target_node`, NOT `target`. Use `node.locate()` to get back
  `(LocalNodeId, TreeId)`.

- **`accesskit_consumer::ChangeHandler` is exported as
  `TreeChangeHandler`** (renamed in the public re-export, while the
  trait declaration in `tree.rs` still uses the bare name). Import
  via `use accesskit_consumer::TreeChangeHandler`.

- **`accesskit_consumer::Tree::new` panics if first `TreeUpdate`
  has `tree: None`**. Cache only the FIRST update for activation.
  This is what `WindowActivationHandler.state.initial_update` is
  for — set once on first drain, never overwritten by subsequent
  diffs.

- **`accesskit_consumer::validate_global` panics if
  `TreeUpdate.focus`** points at the synthetic Window-role root.
  gpui core already picks first non-root child via
  `diff_tree_update` — Layer 3 inherits this for free.

- **macOS's a11y daemon queries the view immediately on window open**;
  iOS likely behaves similarly. Don't crash if `initial_update` is
  None on first activation — the Adapter's Inactive→Placeholder
  fallback handles this. Mirror it in any future variant.

- **The action handler runs on the main thread but without `&mut
  Window/App`**. Cross-thread dispatch is a per-frame queue
  (`PendingA11yAction` + `take_pending_a11y_actions` in gpui core)
  drained at `Window::draw`. Don't try to call `window.focus(...)`
  from the action handler directly.

- **`MacWindow::debug_inject_a11y_action` (cfg-gated) was the
  validation unblocker** for Layer 2 sub-4. `IosWindow::debug_inject_a11y_action`
  (added in sub-commit 4) is the iOS equivalent. Always add this
  hook from day one — retrofitting hurts.

- **iOS spike harness can't be a bare `fn main()` for gpui-driven
  workloads** — `IosPlatform::run` requires `UIApplicationMain` to
  be running and the AppDelegate to call back via FFI. Sub-commit 5
  worked around this by going headless (allocate a bare UIView, no
  UIWindow). Sub-commit 5b will need the full app-bundle ceremony.

- **gpui-mobile dep repoint can have unrelated upstream API drift
  to fix.** When repointing to a fresher zed fork commit, expect
  signature changes elsewhere in the Platform trait or DisplayId.
  Keep the repoint commit small and fixup any breakage as part of
  it (or in a separate same-session commit if the user prefers).
