# Working on HasteClip

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
in the app. Run `cargo clippy -p hasteclip` after UI changes; a box that really
is for free text says so with `#[allow(clippy::disallowed_methods)]` and why.
