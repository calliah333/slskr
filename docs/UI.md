# UI design system

The web UI (`crates/slskr-web`) follows the look of shadcn/ui (slate palette, as in the
owner's "teacup" project): a light grey page, white rounded cards with a 1px border,
segmented and underline tabs, outline buttons with one solid blue primary action, small
coloured pill badges, quiet tables and lucide line icons. Light and dark themes share every
rule; only the token values change.

- `crates/slskr-web/assets/static/css/app.css`: tokens, base styles, a few layout
  utilities, the component classes below, and the app shell.
- `crates/slskr-web/assets/static/css/pages/`: page-specific styles, loaded through
  the Trunk stylesheet links in `crates/slskr-web/index.html`.
- `crates/slskr-web/src/ui/`: Leptos components that emit that markup (`Button`, `Card`,
  `Badge`, `Alert`, `Modal`, `Icon`, …) and the form components in `ui/form.rs`.

## Rules

1. **No inline colours, no hex values in components.** Use the classes here, or a token
   (`hsl(var(--primary))`) in `app.css`. Charts may use `currentColor` and the status
   classes (`text-success`, …) or read tokens with `getComputedStyle` if they must.
2. **No `style=` for layout** except computed values (a progress width, a chart size).
3. **Keep reusable styles in `app.css`.** Scope page rules to a page or its components.
   Prefer composing the existing classes first.
4. Use the Rust components where they exist; write the documented HTML where a component
   would add nothing (tables, `input`, `select`).
5. Icon-only buttons need `aria-label` (the `Button` `label` prop sets it and `title`).
   Keep `aria-current`, `aria-selected`, `role="alert"`/`"status"` as the examples show.
6. Everything must work at phone width (360px): no horizontal page scroll; wide tables
   scroll inside `.table-wrap`; toolbars wrap.
7. Keep the hook classes tests select on (`form-field`, `save-bar`, `login-failure`,
   `navigation`, `setup-step`, …) when restyling; add new ones rather than reusing styling
   classes as test hooks.

## Tokens

Colours are HSL triples so alpha can be added: `hsl(var(--destructive) / 0.1)`.

| Token | Use |
| --- | --- |
| `--page` | page background (light grey; same as background in dark) |
| `--background`, `--foreground` | inputs, raised chips; body text |
| `--card`, `--card-foreground` | cards, tables, buttons |
| `--popover`, `--popover-foreground` | menus, modals, toasts, save bar |
| `--primary`, `--primary-foreground` | the one main action, links, active tab underline |
| `--secondary`, `--secondary-foreground` | muted fills: segmented control, table header, secondary button |
| `--muted`, `--muted-foreground` | quiet backgrounds; secondary text |
| `--accent`, `--accent-foreground` | hover background |
| `--destructive`, `--destructive-foreground` | delete buttons, error borders |
| `--border`, `--input`, `--ring` | borders, input borders, focus ring |
| `--success`, `--warning`, `--info`, `--danger`, `--purple` | status text/icons (`.text-success` …), alert accents |
| `--badge-{neutral,blue,green,amber,red,purple}-{bg,fg}` | full colours for badges |

Other tokens: `--radius` (0.5rem; `--radius-md`, `--radius-sm`, `--radius-full`),
`--space-1`…`--space-10` (0.25rem steps: 1, 2, 3, 4, 5, 6, 8, 10), `--shadow-sm|md|lg`,
`--font-sans` (system UI stack), `--font-mono`, `--container` (72rem), `--duration`.

Base text is 14px (`0.875rem`); headings: `h1` 24px bold, `h2` 20px, `h3` 16px, `h4` 14px,
all semibold. Focus is a 2px `--ring` outline (`:focus-visible`). Animations stop under
`prefers-reduced-motion` (spinners keep turning slowly).

## Theme

`<html class="dark">` switches the palette. An inline script in `index.html` applies the
stored choice before the first paint; `ui::theme::install()` (called once in `App`) keeps it
in step and follows `prefers-color-scheme` while the choice is `system`. The choice
(`light`, `dark`, `system`; default `system`) is stored per browser under `slskr-theme`.
`<ThemeToggle/>` is the square icon button that cycles light → dark → system (sun, moon,
monitor). Read the resolved state with `expect_context::<ui::theme::Theme>().dark`.

## Layout

| Class | Purpose |
| --- | --- |
| `container` | centred column, max 72rem, 16px gutters (24px from 640px) |
| `stack`, `stack--xs/sm/lg` | vertical flex with 16px gap (4/8/24px) |
| `row` | horizontal flex, centred, 8px gap, no wrap |
| `cluster` | horizontal flex that wraps (button groups, badges) |
| `spread` | wrapping row with space-between (title left, actions right) |
| `grid grid--2/3/4` | responsive auto-fit grids (min 16/13/10rem columns) |
| `grow`, `ml-auto`, `w-full` | flex helpers |
| `muted`, `text-xs/sm/lg`, `font-medium/semibold`, `mono`, `tabular`, `text-right/center` | text |
| `truncate`, `nowrap`, `break-all` | overflow |
| `text-success/warning/danger/info` | status colour for text and icons |
| `sr-only` (= `visually-hidden`), `hide-sm` (hidden below 640px) | visibility |

### Page pattern

Pages render inside `<main class="container app-main">`. A page is a `stack` of a page
header and cards:

```rust
view! {
    <div class="transfers stack">
        <PageHeader title="Downloads" description="Files you are downloading."
            actions=ViewFn::from(|| view! { <Button icon="trash-2" text="Clear completed"/> })/>
        <Card title="In progress" flush=true>
            <div class="table-wrap">…</div>
        </Card>
    </div>
}
```

```html
<div class="page-header">
  <div>
    <h1 class="page-header__title">Downloads</h1>
    <p class="page-header__description">Files you are downloading.</p>
  </div>
  <div class="page-header__actions"><button class="btn">…</button></div>
</div>
```

## Components (CSS)

### Button `.btn`

Outline by default (card background, 1px border, small shadow), 36px tall.

| Modifier | Look |
| --- | --- |
| `btn--primary` | solid blue (one per view: the main action) |
| `btn--secondary` | muted fill, no border (teacup's "Refresh") |
| `btn--outline` | the default, explicit |
| `btn--ghost` | transparent until hover |
| `btn--destructive` | solid red (confirm deletes) |
| `btn--danger-outline` | outline with red text (a delete that opens a confirmation) |
| `btn--link` | looks like a link |
| `btn--action` | quiet 32px icon button for table rows; add `is-danger` for red hover |
| `btn--xs/sm/lg` | 28/32/44px tall |
| `btn--icon` | square icon-only button |
| `btn--block` | full width |

```html
<button type="button" class="btn btn--primary"><svg class="icon">…</svg>Search</button>
<button type="button" class="btn btn--icon" aria-label="Refresh" title="Refresh"><svg class="icon">…</svg></button>
<td class="actions"><button class="btn btn--action is-danger" aria-label="Delete"><svg class="icon">…</svg></button></td>
```

Groups: `btn-group` (gap) or `btn-joined` (edges touching). Loading: put
`<span class="spinner"></span>` inside, set `disabled` and `aria-busy="true"`.
`link-button` is a borderless text button for inline actions.

### Card `.card`

```html
<section class="card">
  <div class="card__header">
    <div><h2 class="card__title">Admin</h2><p class="card__description">…</p></div>
    <div class="card__actions"><button class="btn btn--sm">Refresh</button></div>
  </div>
  <div class="card__content">…</div>
  <div class="card__footer">…</div>
</section>
```

`card--flush` (content without padding, for a table or list reaching the edges),
`card--compact` (16px padding), `card__title--sm` (14px title, teacup's "Upload with curl").
`panel` is a muted inset box inside a card (bulk-action bar, filters).

### Form controls

```html
<div class="field">
  <label class="label" for="url">URL</label>
  <input class="input" id="url" placeholder="https://example.com">
  <p class="help">3-20 characters.</p>
  <p class="error" role="alert">Required</p>
</div>
<select class="select select--auto">…</select>
<textarea class="textarea textarea--mono" rows="6"></textarea>
<div class="input-icon"><svg class="icon">search</svg><input class="input" type="search"></div>
<div class="input-group"><input class="input"><button class="btn">Go</button></div>
<label class="check"><input type="checkbox" class="checkbox">Keep forever</label>
<label class="switch"><input type="checkbox" role="switch"><span class="switch__track"></span><span class="switch__label">Hide locked</span></label>
<span class="inline-field"><label class="label" for="ttl">Expires after</label><input class="input input--auto" id="ttl"></span>
```

`input--sm`/`select--sm` (32px), `input--mono`, `input--auto`/`select--auto` (natural
width). `aria-invalid="true"` draws a red border. `:disabled` dims the control.

### Tabs

Segmented control (muted pill, active tab a raised chip), for the main nav and view
switches; underline tabs for sub-sections. Active: `aria-selected="true"` (buttons),
`aria-current="page"` or `is-active` (links).

```html
<div class="tabs" role="tablist" aria-label="Mode">
  <button class="tabs__tab" role="tab" aria-selected="true">Files</button>
  <button class="tabs__tab" role="tab" aria-selected="false">Text</button>
</div>
<nav class="tabs-underline" aria-label="System">
  <a class="tabs__tab is-active" href="…" aria-current="page">Info</a>
  <a class="tabs__tab" href="…">Logs</a>
</nav>
```

`tabs--block` stretches tabs to full width. Both scroll horizontally on phones. A vertical
link list (settings sections): `nav-list` with `nav-list__item` (+ `is-active`).

### Badge `.badge`

```html
<span class="badge badge--warning">2h left</span>
<span class="badge badge--primary">Permanent</span>
<span class="badge badge--danger">0/1 views</span>
<span class="badge badge--purple">Text</span>
```

Variants: (none) neutral slate, `badge--primary`/`--blue`, `--success`/`--green`,
`--warning`/`--amber`, `--danger`/`--red`, `--purple`, `--outline`, `--solid`. An icon inside
shrinks to 12px. `count` is a round number bubble (unread counts).

Suggested states: queued → neutral, in progress → primary, succeeded → success, cancelled
or rejected → warning, errored or failed → danger, locked → outline + `lock` icon,
online/away/offline → `StatusDot`.

### Table

```html
<div class="toolbar">
  <p class="muted">Total files: 4</p>
  <div class="input-icon grow">…</div>
  <button class="btn btn--secondary btn--sm">Refresh</button>
</div>
<div class="table-wrap">
  <table class="table">
    <thead><tr><th class="check-col"><input type="checkbox" class="checkbox" aria-label="Select all"></th>
      <th>Filename</th><th class="num">Size</th><th class="actions"><span class="sr-only">Actions</span></th></tr></thead>
    <tbody>
      <tr class="is-selected"><td class="check-col">…</td><td>sunset.webp</td><td class="num">608 KB</td>
        <td class="actions"><button class="btn btn--action" aria-label="Copy">…</button></td></tr>
      <tr class="empty-row"><td colspan="4">No files yet.</td></tr>
    </tbody>
  </table>
</div>
```

Header row is muted, rows have dividers and a hover fill. `table--compact` (8/12px
cells), `table--fixed`, cell classes `num`/`numeric` (right-aligned tabular numbers),
`shrink`/`collapsing` (as narrow as possible), `actions`, `check-col`. A table inside a
`card--flush` drops its own border: put the `table-wrap` there anyway, it scrolls on phones.
For row lists that are not tabular: `<ul class="list"><li>…</li></ul>`.

### Stat tiles

```html
<div class="stats">
  <div class="stat"><div class="stat__label">Files</div><div class="stat__value">3</div></div>
  <div class="stat"><div class="stat__label">Storage used</div><div class="stat__value">623 KB of 1.0 GB</div>
    <div class="progress"><div class="progress__bar" style="width:6%"></div></div></div>
</div>
```

`stat__value--lg` for headline numbers, `stat__hint` for a line below.

### Progress

```html
<div class="progress" role="progressbar" aria-valuenow="40" aria-valuemin="0" aria-valuemax="100">
  <div class="progress__bar" style="width:40%"></div>
</div>
```

`progress--lg`, `progress--success|warning|danger`, `progress--indeterminate`.
`progress-label` is a small row above the bar (name left, percentage right).

### Alert

```html
<div class="alert alert--danger" role="alert">
  <svg class="icon">circle-alert</svg>
  <div class="alert__body"><div class="alert__title">Could not save</div><p>…</p></div>
  <button class="btn btn--ghost btn--icon btn--sm alert__dismiss" aria-label="Dismiss">…</button>
</div>
```

Variants `alert--info|success|warning|danger`; neutral without a modifier.

### Dropdown menu, popover

```html
<div class="dropdown">
  <button class="btn btn--action btn--icon" aria-haspopup="menu" aria-expanded="true" aria-label="More">…</button>
  <div class="menu menu--end" role="menu">
    <div class="menu__label">Sort by</div>
    <button class="menu__item" role="menuitemradio" aria-checked="true">Name</button>
    <div class="menu__separator"></div>
    <button class="menu__item menu__item--danger" role="menuitem">Delete</button>
  </div>
</div>
```

`menu--end` aligns to the right edge, `menu--up` opens upwards. `popover` is a free
positioned panel (details on hover/click). Prefer the `DropdownMenu` component, which
handles open/close.

### Modal

```html
<div class="modal-backdrop">
  <div class="modal modal--sm" role="dialog" aria-modal="true" aria-labelledby="t">
    <div class="modal__header"><div><h2 class="modal__title" id="t">Change expiry</h2>
      <p class="modal__description">README.md</p></div><button class="btn btn--ghost btn--icon btn--sm modal__close">…</button></div>
    <div class="modal__body">…</div>
    <div class="modal__footer"><button class="btn">Cancel</button><button class="btn btn--primary">Save</button></div>
  </div>
</div>
```

Sizes `modal--sm` (24rem), default 32rem, `modal--lg` (48rem), `modal--xl` (64rem). Always
use the `Modal` component (portal, Escape, backdrop click, focus return, body scroll lock).

### Toast

`<div class="toast-region">` fixed bottom right (polite live region) holding
`<div class="toast toast--success">…</div>`; see the `Toast` component.

### Empty state, loading

```html
<div class="empty-state empty-state--dashed">
  <div class="empty-state__icon"><svg class="icon">cloud-upload</svg></div>
  <p class="empty-state__title">Drop files here</p>
  <p class="empty-state__description">Up to 100 MB per file</p>
  <div class="empty-state__actions"><button class="btn btn--primary">Search</button></div>
</div>
<span class="spinner"></span> <span class="spinner spinner--lg"></span>
<div class="loading" role="status" aria-label="Loading"><span class="spinner spinner--lg"></span></div>
<span class="skeleton" style="width:12rem"></span>
```

### Code, kbd, description list

`<pre class="code">` is the monospace box (teacup's curl box); `code--scroll` keeps lines and
scrolls (logs), `code--plain` uses body colour. Inline `<code>` gets a muted chip.
`<kbd class="kbd">Ctrl</kbd>`. `<dl class="dl"><dt>Version</dt><dd>0.1.0</dd></dl>` is a
two-column key/value list (stacks on phones).

### Avatar, status dot

`<span class="avatar">AB</span>` (`avatar--sm|lg`, or an `<img>` inside).
`<span class="status-dot status-dot--online"></span>`: `online` (green), `away` (amber),
`busy` (amber, no halo), `offline` (red), none (grey); `status-dot--pulse` blinks.

### Icons

`<svg class="icon">` 16px in `currentColor`; `icon--xs|sm|lg|xl|2xl` (12/14/20/24/32px),
`icon--spin`. Use the `Icon` component; names are lucide's:

activity, arrow-down, arrow-left, arrow-right, arrow-up, arrow-up-down, ban, chart-column,
check, chevron-down, chevron-left, chevron-right, chevron-up, circle, circle-alert,
circle-check, circle-help, circle-stop, circle-x, clock, cloud-upload, contrast, copy,
corner-left-up, door-open, download, ellipsis, external-link, eye, eye-off, file,
file-audio, file-text, filter, flask-conical, folder, folder-open, gauge, globe, hard-drive,
hash, headphones, history, inbox, info, key, list, list-filter, loader-circle, lock,
lock-open, log-in, log-out, megaphone, menu, message-square, messages-square, minus,
monitor, moon, music, pause, pencil, play, plug, plug-zap, plus, power, refresh-cw,
rotate-ccw, rotate-cw, save, search, send, server, settings, share-2, shield, square, star,
sun, trash-2, triangle-alert, undo-2, unplug, upload, user, user-plus, user-x, users, wifi,
wifi-off, x.

Aliases: alert-circle, alert-triangle, warning, bar-chart, help, loader, messages,
more-horizontal, sort, stop, trash, unlock. To add an icon, copy the inner markup of
`lucide-static@0.460.0/icons/<name>.svg` into `src/ui/icon_data.rs` (sorted by name).

## Components (Rust, `crate::ui`)

All take an extra `class` where it makes sense. Optional props can be left out.

| Component | Props | Renders |
| --- | --- | --- |
| `Icon` | `name`, `size` ("xs".."2xl"), `spin: Signal<bool>`, `class`, `label` (makes it an `img`) | `<svg class="icon">` |
| `Button` | `variant` ("primary", "secondary", "ghost", "destructive", "danger-outline", "link", "action"), `size` ("xs", "sm", "lg"), `icon`, `text` or children, `label` (aria-label + title; required for icon-only), `disabled`, `loading`, `pressed`, `block`, `button_type`, `href`, `on_click` | `<button class="btn …">` or `<a>` |
| `Card` | `title`, `description`, `actions: ViewFn`, `footer: ViewFn`, `flush`, `compact`, `heading` ("h2"/"h3"), `title_id`, `id`, children | `.card` |
| `PageHeader` | `title`, `description`, `actions: ViewFn` | `.page-header` |
| `EmptyState` | `icon`, `title`, `description`, `dashed`, children (actions) | `.empty-state` |
| `Stat` | `label`, `value: Signal<String>`, `hint`, `large`, children | `.stat` |
| `Badge` | `variant`, `icon`, `text` or children, `title` | `.badge` |
| `Alert` | `variant` ("info", "success", "warning", "danger"; Semantic's "negative"/"positive" accepted), `title`, `icon` ("" hides), `role`, `on_dismiss`, children | `.alert` |
| `Spinner` | `large`, `label` | `.spinner` |
| `Loading` | `label`, `page` (half-screen), `show_label` | `.loading` |
| `Progress` | `value: Option<Signal<f64>>` (None = indeterminate), `total` (100), `variant: Signal<&str>`, `large`, `label` | `.progress` |
| `Toast` | `variant`, `icon`, `on_dismiss`, children | `.toast` (inside your `.toast-region`) |
| `StatusDot` | `status: Signal<&'static str>` ("online", "away", "busy", "offline"), `label`, `pulse` | `.status-dot` |
| `Avatar` | `name`, `src`, `size` | `.avatar` with initials |
| `Tooltip` | `text`, children | `<span title>` |
| `Checkbox` | `checked`, `on_change: Callback<bool>`, `label` or `aria_label`, `disabled`, `id` | `label.check` |
| `Switch` | same as `Checkbox` | `label.switch` |
| `Tabs` | `items: Vec<TabItem>` (`TabItem::new(value, label).icon("…")`), `value`, `on_change`, `underline`, `label` | button tablist with arrow-key focus |
| `DropdownMenu` | `label` (accessible name), `text`, `icon`, `variant`, `size`, `align` ("end"), children (menu items) | `.dropdown` + `.menu`; closes on item click, outside click, Escape |
| `Modal` | `open: Signal<bool>` or `trigger: ViewFn`, `on_close`, `title`, `description`, `size` ("sm", "lg", "xl"), `close_button` (true), `content` (text) or children, `actions: Vec<ModalAction>` or `footer: ViewFn`, `class` | portal dialog |
| `ModalAction` | `ModalAction::new("Cancel")`, `.primary()`, `.negative()`, `.on_click(..)`; clicking closes the modal | footer button |
| `ThemeToggle` | — (needs `theme::install()` above it) | theme icon button |
| `Brand` | `href` | logo mark + "slskr" + "Soulseek client" |

### Form components (`crate::ui::form`)

The settings and setup fields use `assets/static/css/pages/settings.css`.
`Field` provides the control's `id`, `aria-describedby`, invalid and locked state as context,
so the control inside needs none.

Two layouts. **Stacked** (setup wizard, dialogs): label, control, help, error. **Row**
(`row=true`, settings): label and help on the left, a right-sized control on the right, rows
separated by hairlines inside a `Section`. Use `wide=true` to put a full-width control
(chips, a text area, the folder list) below its label. In rows:

- Booleans are `Toggle` switches and enums `Select`s. Numbers are narrow `NumberInput`s with
  a `unit` such as "KiB/s" or "files". Use a `zero_label` checkbox where 0 means unlimited.
- Durations are a number and a unit (`DurationInput`), with "Never" where empty means never.
- Lists of strings are `ChipInput`s (Enter adds, Backspace in the empty input removes).
- A value set in `slskr.toml` or the environment is locked: the control is disabled and a
  lock icon after the label carries the reason as tooltip and screen-reader text
  (`Set in slskr.toml or SLSKR_WEB_LISTEN (web.listen)`).
- Rarely changed rows go into `Advanced` ("Show advanced settings"), which opens by itself
  when a row inside has an error.
- Undo (`form-revert`) and Reset to default (`form-reset`) are small icon buttons after the
  label.

| Component | Purpose |
| --- | --- |
| `Section { id, title, description, actions, footer }` | a `card form-section` whose content is `form-rows` |
| `RowHeading { title, description }` | a small heading between rows |
| `Field { id, label, help, error, locked_reason, tag, tag_title, actions, row, wide }` | `field form-field` (+ `form-row`, `form-row--wide`, `field--error`, `field--locked`): label head (lock icon `.form-locked`, `badge--outline` tag, actions), control, help, error |
| `FieldGrid` | stacked fields side by side (`form-grid`) |
| `TextInput { mono }`, `PasswordInput`, `TextArea`, `Select` | `input`/`textarea`/`select` bound to the field |
| `NumberInput { unit, zero_label, fallback }` | narrow number with a unit addon (`input-affix`) and an optional "Unlimited" checkbox (`form-zero`) |
| `DurationInput { optional }` | number and a unit select with choices from seconds to weeks plus "Never". The select uses `<id>-unit` |
| `Toggle { checked, on_change, states }` | a `switch` with optional On/Off text via `states` |
| `ChipInput { items, on_change, placeholder, mono }` | `chip-input` with removable `chip`s |
| `Advanced { open, label }` | `details.form-advanced` disclosure of rows |
| `FormMessage { kind, title }` | an `Alert` with the `form-message` hook |
| `SaveBar { dirty, saving, on_save, on_discard, problem }` | floating bar at the bottom while there are unsaved changes: count, Discard, "Save changes" |

`components/directory_picker.rs` adds `FolderList` (shared-folder rows with rename and
remove, and an "Add folder" button) and `DirectoryPicker` (the folder dialog).

## App shell

`components/app.rs`: `AuthLayout` (sign-in, setup, connection error: brand + theme toggle
over one centred card) and the signed-in shell: a header (`Brand` left; pending-action
and new-version buttons, the Soulseek connection button with a `StatusDot`, `ThemeToggle`,
the Settings gear and Logout right) above the main navigation, a segmented `tabs` bar
(`nav.navigation`: Overview, Search, Downloads, Uploads, Browse, Users, Rooms, Chat,
System) that scrolls sideways on phones. There is no footer; live transfer speeds and
queue counts are the stat tiles at the top of Overview.

## From Semantic UI

| Semantic UI | Now |
| --- | --- |
| `ui segment`, `ui raised segment`, `ui card` | `Card` / `.card` (`card__header`, `card__content`) |
| `ui placeholder segment` | `EmptyState` |
| `ui header`, `ui icon header` | `PageHeader`, `.card__title`, `.section-title` |
| `ui button`, `primary`, `basic`, `negative`, `mini`/`small` | `.btn`, `btn--primary`, (default), `btn--destructive` or `btn--danger-outline`, `btn--xs`/`btn--sm` |
| `ui icon button` | `Button icon="…" label="…"` (`btn--icon`), `btn--action` in table rows |
| `ui buttons` | `btn-group` / `btn-joined`, or `Tabs` for a choice of one |
| `loading` class on a button | `loading` prop / `<span class="spinner">` + `disabled` |
| `<i class="search icon">` | `<Icon name="search"/>` (mapping table in `src/ui/icon.rs`) |
| `ui input`, `ui action input`, `ui left icon input` | `.input`, `.input-group`, `.input-icon` |
| `ui dropdown` (native select) | `.select` |
| `ui toggle checkbox`, `ui checkbox` | `Switch`, `Checkbox` |
| `ui label`, `ui basic label`, coloured labels | `Badge` (`badge--…`), `badge--outline` |
| `ui message` (`negative`/`positive`/`warning`/`info`) | `Alert variant="danger"/"success"/"warning"/"info"` |
| `ui table`, `celled`, `compact`, `collapsing` | `.table-wrap > .table`, `table--compact`, `shrink` |
| `ui menu` / `tabular` / `pointing secondary` | `Tabs` (`tabs`, `tabs-underline`), `nav-list` |
| `ui loader` | `Loading`, `Spinner` |
| `ui progress` | `Progress` |
| `ui statistic(s)` | `Stat` in `.stats` |
| `ui modal` | `Modal` |
| `ui popup` | `.popover`, `DropdownMenu`, or `title` |
| `ui list`, `divided list` | `.list` |
| `ui grid`, `column` | `.grid grid--N`, `.stack`, `.spread` |
| `ui breadcrumb` | `.breadcrumb` |
| `green/red/yellow circle icon` | `StatusDot` |
