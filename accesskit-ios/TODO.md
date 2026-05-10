# accesskit-ios — implementation punch list

Per the §10.4 Layer 3 plan in the gem repo's `DESIGN.md`. This crate
scaffolds the API surface; method bodies are `todo!()`. Subsequent
sub-commits land the implementation. Each item below is a session-
sized commit.

`accesskit_macos` 0.26 is the canonical template. Its source is at
`~/.cargo/registry/src/.../accesskit_macos-0.26.0/src/`. Files
referenced below are within that path.

## Sub-commit 1 — scaffold (DONE, `773e9d6`)

Lazy-workspaceified the philipaw/gpui-mobile fork; added `accesskit-ios`
as a sibling crate; stubbed `SubclassingAdapter` API matching
accesskit_macos 0.26.

## Sub-commit 2 — `Adapter` (manual integration path)

Mirror `accesskit_macos::Adapter` (in `adapter.rs`, ~400 lines). The
non-subclassing path: caller manually overrides the UIAccessibility
selectors and delegates to the adapter.

- New `Adapter` struct holding the state machine: `Inactive {
  view: WeakId<UIView>, ... }` → `Placeholder { ... }` →
  `Active(Rc<Context>)`. Same shape as macOS.
- `pub unsafe fn new(view: *mut c_void, is_view_focused: bool,
  action_handler: impl ActionHandler + 'static) -> Self`.
- `pub fn update_if_active(&mut self, factory: impl FnOnce() ->
  TreeUpdate) -> Option<QueuedEvents>`. Forwards to
  `accesskit_consumer::Tree::update`; collects events.
- UIAccessibility query helpers (parallel to macOS's
  `view_children` / `focus` / `hit_test`):
    - `pub fn accessibility_element_count<H: ActivationHandler>(
        &mut self, h: &mut H) -> isize`.
    - `pub fn accessibility_element_at_index<H: ActivationHandler>(
        &mut self, idx: isize, h: &mut H) -> *mut NSObject`.
    - `pub fn index_of_accessibility_element<H: ActivationHandler>(
        &mut self, element: *mut NSObject, h: &mut H) -> isize`.
- New deps: `accesskit_consumer = "0.35"`, `objc2 = "0.6"`,
  `objc2-foundation = "0.3"`, `objc2-uikit = "?"` (version-pin TBD —
  open research item from §10.4).

Validation: `cargo check`. Real validation comes in sub-commit 3
(SubclassingAdapter) where we can actually exercise the UIAccessibility
path, and sub-commit 5 (spike harness) where we run on iOS Simulator.

## Sub-commit 3 — `SubclassingAdapter` (dynamic UIView subclass)

Mirror `accesskit_macos::SubclassingAdapter` (in `subclass.rs`,
~270 lines).

- `declare_class!` an `AccessKitSubclassAssociatedObject` holding
  `Adapter` + activation handler. State stored in associated-object
  ivars.
- A static `Mutex<Vec<(prev_class, subclass)>>` cache for created
  subclasses (one per parent UIView class encountered).
- `unsafe fn new(view: *mut c_void, activation: impl ActivationHandler,
  action: impl ActionHandler) -> Self`:
    1. Create `AssociatedObject` instance.
    2. `objc_setAssociatedObject(view, ASSOCIATED_OBJECT_KEY, ...)`.
    3. Look up or build a subclass that adds:
        - `accessibilityElementCount` → fetches associated object,
          delegates to `Adapter::accessibility_element_count`.
        - `accessibilityElementAtIndex:` → ditto.
        - `indexOfAccessibilityElement:` → ditto.
        - `superclass` → returns `prev_class` (so super-method
          lookups still work).
    4. `objc_setClass(view, subclass)`.
- `Drop`: revert the class swap (`object_setClass(view, prev_class)`).
- `pub fn update_if_active(...)` and `pub fn
  update_view_focus_state(...)` delegate through the associated
  object's `Adapter`.

Open issue from §10.4: macOS's docstring warns "must be done before
the view is shown or focused for the first time." iOS likely has
the same constraint plus restrictions around accessibility tree
caching across `becomeFirstResponder`. Test early.

## Sub-commit 4 — gpui-mobile fork integration

Mirror `gpui_macos/src/window.rs`'s a11y blocks (search §10.4 Layer
2 sub-commits 1–4 in the gem repo's git log: `e5ce3ebf13`,
`116058cff9`, `db30c3e66d`). Roughly 150 lines on the gpui-mobile
side:

- `MobileWindowState` (or whatever the struct is called in
  gpui-mobile's iOS Platform impl) gains:
    - `a11y_adapter: accesskit_ios::SubclassingAdapter`.
    - `a11y_state: Arc<Mutex<A11yState>>` with `initial_update`,
      `focus_inverse_map`, `pending_actions` — same shape as
      `gpui_macos`.
- Construct `SubclassingAdapter` at window creation.
- Implement `PlatformWindow::take_accessibility_handler` to return
  the `AccessibilityDrain`-consuming closure.
- Implement `PlatformWindow::take_pending_a11y_actions` to drain
  the queue.
- Add `IosActionHandler` (parallel to macOS's
  `LoggingActionHandler`) — resolve `Action::Focus` via inverse
  map, enqueue `PendingA11yAction::Focus(fid)`.
- Add `MobileWindow::debug_inject_a11y_action` test hook (lesson
  from Layer 2: this is painful to retrofit).

## Sub-commit 5 — `spikes/gpui-ios-a11y-preview`

Mirror of `spikes/gpui-mac-preview` but built for
`aarch64-apple-ios-sim`. LabeledButton renders with
`Element::accessibility()` override, handler logs incoming
`TreeUpdate`s to stderr.

Build via `cargo build --target aarch64-apple-ios-sim
-p gpui-ios-a11y-preview`. Deploy via `xcrun simctl spawn` (similar
to gem's `just spike-ios-sim` recipe). Add a `just
spike-gpui-ios-a11y` recipe.

Initial validation: handler output (mirror of what spike-gpui
produces today on macOS). VoiceOver validation in sub-commit 6.

## Sub-commit 6 — VoiceOver-on-Simulator validation

`xcrun simctl ui` to enable VoiceOver in the booted Simulator,
relaunch the spike binary, observe VoiceOver announces the
LabeledButton with correct role + label.

iOS Accessibility Inspector (Xcode → Open Developer Tool →
Accessibility Inspector → switch target picker to Simulator) gives
visual verification of the projected tree.

## Cost recap

§10.4 estimate: 6-8 sittings total for sub-commits 2-6. Sub-commit
1 is done; remaining: 5-7 sittings.

## Lessons carried forward from Layer 1+2

Don't re-discover these:

- `accesskit_consumer::Tree::new` panics if the cached `TreeUpdate`
  for activation has `tree: None`. Cache only the FIRST update
  (which has `tree: Some(...)` + full nodes list). gpui-mobile's
  platform handler closure follows the same pattern as
  gpui_macos's — see `take_accessibility_handler` impl on
  `MacWindow`.
- `accesskit_consumer::validate_global` panics if `TreeUpdate.focus`
  points at the synthetic Window-role root. gpui core already
  picks first non-root child via `diff_tree_update` — Layer 3 gets
  this for free.
- macOS's a11y daemon queries the view immediately on window open;
  iOS likely behaves similarly. Don't crash if `initial_update` is
  None on first activation — accesskit_macos's `Adapter` handles
  this via the Inactive state. Mirror that.
- The action handler runs on the main thread but without `&mut
  Window/App`; cross-thread dispatch is a per-frame queue
  (`PendingA11yAction` + `take_pending_a11y_actions` in gpui core)
  drained at `Window::draw`. Don't try to call `window.focus(...)`
  from the action handler directly.
- `MacWindow::debug_inject_a11y_action` (cfg-gated) was the
  validation unblocker for Layer 2 sub-4. Add the iOS equivalent
  on the iOS Platform impl from sub-commit 4.
