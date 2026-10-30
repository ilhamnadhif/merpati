# Merpati UI design guide

The app is drawn with egui. Every screen uses the same tokens and components so
it looks like one product. Read this before changing UI code.

## Tokens (`crates/merpati-app/src/theme.rs`)

- Colors: `theme::pal(ui)` returns the `Palette` of the current theme:
  `canvas` (content background), `chrome` (top bar, sidebar, tab strip, status
  bar, table headers), `surface` (inputs, cards, dialogs, menus), `hover`,
  `subtle` (pressed, segmented track), `border`, `border_strong`, `text`,
  `text_muted`, `text_faint`, `accent` (coral red of the icon), `accent_hover`,
  `accent_soft` (selected rows, accent pills), `accent_text`, `ok`, `warn`,
  `err`, `info`. `theme::soft(color, alpha)` gives a soft fill behind colored
  text. Never hard-code RGB values in screens; use the palette.
- Methods and statuses: `theme::method_color`, `theme::status_color`.
- Type: Inter for UI text, JetBrains Mono for code. `theme::medium(size)` for
  labels that need weight (tabs, buttons, table headers), `theme::semibold(size)`
  for titles (15–17 px for view titles, 13–14 px for section titles),
  `theme::mono(size)`. `RichText::strong()` only changes color; use the font
  helpers for real weight.
- Radius: `theme::RADIUS` (6) for controls, 8 for cards and grouped inputs, 10
  for dialogs and toasts.
- Spacing: 8 px between controls in a row, 12–16 px between groups, 18 px page
  margin (set by the shell).

## Icons (`crates/merpati-app/src/icons.rs`)

Phosphor icons, usable inside any text: `format!("{} {}", icons::PLUS, tr("New"))`
or `kit::with_icon(icons::PLUS, tr("New"))` for menu items. Do not use emoji or
symbol characters as icons (🗀 ⚙ ▶ ⋯ ✕ …). To add an icon, add its Phosphor name
to `ICONS` in `scripts/build-fonts.py` and rerun the script (see its header).

## Components (`crates/merpati-app/src/kit.rs`)

| Need | Use |
| --- | --- |
| Main action of a view (Send, Run, Save in a dialog) | `kit::primary` / `kit::primary_icon` |
| Ordinary button | `kit::secondary(ui, Some(icon), text)` |
| Toolbar / quiet action | `kit::ghost(ui, Some(icon), text)` |
| Destructive | `kit::button_ex(ui, Kind::Danger, …)` |
| Icon-only button (always with a tooltip = accessible name) | `kit::icon_button` / `kit::icon_toggle` |
| Tabs inside a view | `widgets::sub_tabs` (= `kit::tab_strip`; ` (n)` suffix becomes a badge) |
| Choice between 2–5 views | `kit::segmented` |
| Status / method / count label | `kit::pill`, `kit::status_pill`, `widgets::method_badge` |
| Keyboard hint | `kit::kbd(ui, "⌘↩")` |
| Section title | `kit::section_title(text)` |
| Help text | `kit::hint(ui, text)` |
| Bordered group | `kit::card(ui).show(ui, …)` |
| Nothing to show yet | `kit::empty_state(ui, icon, title, body, actions)` |
| Search box | `kit::search_field` |
| Key/value editing | `widgets::kv_table` (set `KvOptions::title`) |
| Menus | `let r = kit::…button…; egui::Popup::menu(&r).show(|ui| …)` |
| Drop-down (`ComboBox`) | always `.icon(kit::combo_caret)` (chevron instead of egui's triangle) |
| Focus of a hand-drawn input or input group | `kit::focus_ring(ui, rect, radius)` (1 px accent border + soft halo; framed `TextEdit`s get the accent border from the theme) |
| Masked secret | `widgets::secret_field` / `auth_ui::secret_input` (eye inside the field) |
| Dialog | `dialogs::Modal` with `dialogs::footer` (Cancel secondary, main action primary on the right) |
| Settings-style rows | `dialogs::group` + `Rows::row(label, help, control)`; on/off is `dialogs::toggle` |

Screen-level helpers that several views share live next to their first user
for now: `request_tab` (banners, stat cards, read-only tables, disclosures),
`proto_common` (connection bar, status pill, message log, notice cards),
`flow_tab` (sidebar rows with hover actions, confirm dialog, inspector forms).
Reuse them instead of drawing a new variant; move one into `kit.rs` when a
third screen needs it.

## App shell on macOS

- **Menu bar** (`menu.rs`, muda): the app, File, Edit, View, Window and Help
  menus. An item with a shortcut hands egui the same key press (or
  Copy/Cut/Paste) the shortcut would have, so `MerpatiApp::shortcuts` and text
  editing stay the one place that handles keys on every platform. Add new
  shortcuts there first, then a menu item that sends the key.
- **Scrolling**: trackpad gestures stick to one axis (`scroll_lock.rs`), so text
  that scrolls both ways doesn't shake. A canvas that pans in 2D calls
  `scroll_lock::allow_free_scroll(ctx, rect)` every frame.

## Rules

- One primary (accent) button per view.
- Every icon-only control has a tooltip and an accessible name.
- Empty, loading and error states are designed (empty state with an action,
  spinner with text, error with icon, cause and next step).
- Text that users read goes through `tr()`/`trf()` with an Indonesian entry in
  `crates/merpati-app/i18n/id.tsv`.
- Check both themes (`{"theme": "light"}` / `{"theme": "dark"}` automation
  steps) and a narrow window (`{"window": [900, 640]}`).
