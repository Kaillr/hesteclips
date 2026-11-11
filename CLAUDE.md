# Working on HesteClips

## Platforms

HesteClips is made for Windows, macOS and Linux, and every feature is meant for
all three. Whichever machine you're working on, the other two count as much.

- **Plan all three before you start.** Say how the feature works on each: what
  does it on Windows, on macOS and on Linux, and anything one of them can't
  do. Build it on all three unless the user says otherwise.
- **On fewer than all three, it stays on its own branch.** Don't merge it into
  main until it works on every platform, unless the user, told which
  platforms it's missing on, says to merge it as it is. Where it's missing,
  it's hidden or says why, behind a capability flag in
  `crates/capture/src/lib.rs` (like `OUTPUT_SOURCES`), never a control that
  silently does nothing.
- **Every PR and every report of a feature starts with its platform status**:
  a line each for Windows, macOS and Linux, saying what was written, compiled
  and run, and how. What you can't build or run where you are is said too,
  with where it can be, never left out:

  ```
  Windows: done; built, tests pass, tried in the app
  macOS: written; capture crate compiles (cargo check), not run
  Linux: not done; hidden behind OUTPUT_SOURCES
  ```

- **Asked whether something works on a platform, answer that first**, before
  merging, pushing or anything else the answer could change.
- A fix for a bug that only one platform has is fine on its own: say that it is.

## Inputs

Every control that takes a value (a field, a menu, a number) follows
"Inputs guide" at the top of `crates/app/src/ui_kit.rs`. Read it before adding
or changing one, and copy how the Settings page does the same kind of value
rather than inventing a new kind of input. In short:

- A word ("Original", "Auto", "Off") is only ever picked from a menu
  (`ui_kit::ChoiceField`). It is never shown in a box that can be clicked
  into and typed over.
- A choice, or a number of your own: a `ChoiceField` with `Custom`. Custom…
  puts a number box with its unit beside the menu, as Settings' Resolution
  does.
- Always a number: `ui_kit::ValueField` (digits only, its unit after it, the
  common values under its ∨, kept to its range).
- Free text (a name, a search) is the only place letters are typed.

`clippy.toml` turns away egui's own `DragValue`, `Slider` and bare `TextEdit`
in the app. Run `cargo clippy -p hesteclips` after UI changes; a box that really
is for free text says so with `#[allow(clippy::disallowed_methods)]` and why.
