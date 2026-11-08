# Yii2 Support for OpenIDEA

Port of core features from [nvlad/yii2support](https://github.com/nvlad/yii2support) for OpenIDEA.

## Status (0.1.6)

| Feature | Status | Notes |
|---------|--------|-------|
| **Views** — completion in `render()` / `renderPartial()` | ✅ | Scans `**/views/**/*.php`, priority from controller |
| **Views** — Go to Definition from controller/module | ✅ | `//`, `/`, `@app`, relative + module path |
| **Views** — missing view diagnostics + quick-fix | ⚠️ | Code action exists; file creation limited by API |
| **Controllers** — index `*Controller.php` | ✅ | id, module, view_path, actions |
| **Routes** — `Url::to` / `redirect` / `Html::a` | ✅ | Completion + Go to controller |
| **Routes with parameters** | ✅ | `<id:\d+>`, param keys in array, action signature |
| **urlManager rules** | ✅ | Scans config, pattern → route + params |
| **i18n** — `Yii::t()` category | ✅ | From message files + app/yii |
| **i18n** — message keys | ✅ | Scans `messages/**/*.php` |
| **ActiveRecord / Forms** — attributes | ✅ | Indexes models + type hints / `@var` / `new` |
| **rules() / attributes() / scenarios()** | ✅ | Extraction from model methods |
| **Config arrays** (`'class' => …`) | ✅ | Common Yii components |
| Database / ActiveQuery / Migrations UI | ❌ | Requires deep DB / process access |

## Build

```bash
# Docker (recommended). Files in dist/ get owner $(id -u):$(id -g)
./build.sh docker   # wasm + yii2.support-*.oiplugin

# Locally (Rust + wasm32-wasip2 or openidea-plugin CLI)
./build.sh local

# Package for IDE: yii2.support-<ver>.oiplugin
./build.sh package

# All together
./build.sh all

./build.sh check
```

Result: `dist/plugin.wasm` (path from `plugin.toml` → `wasm = "dist/plugin.wasm"`).

## Installation

**Important:** Without built `dist/plugin.wasm`, Install from Disk **will not work**.

1. Build: `./build.sh docker && ./build.sh package`
2. In OpenIDEA:
   - **Settings › Plugins › ⚙ › Install Plugin from Package File…** → `yii2.support-*.oiplugin`
   - or **Install Plugin from Disk…** → plugin directory (where `plugin.toml` **and** `dist/plugin.wasm` exist)
3. Accept capabilities (`project.read`, `editor.read`, `editor.write`)
4. Reload plugins / restart IDE

Development without installation:

```bash
openidea-plugin link .     # linked plugin, reads directory directly
# after changes: ./build.sh docker → Settings › Plugins › Reload
```

## Installation (Development)

```bash
# requires openidea-plugin CLI
cargo install --locked --git https://github.com/l0ng-ai/openidea ide-host --bin openidea-plugin

cd yii2support
openidea-plugin build          # → dist/plugin.wasm + check
openidea-plugin link           # link directory to IDE
# then Settings → Plugins → Reload
```

Or **Install Plugin from Disk…** and point to this directory.

## Configuration

Settings → Yii2 Support:

- `viewPath` — views path alias (default `@app/views`)
- `tablePrefix` — table prefix (reserved)
- `reportMissingViews` — whether to show issues for missing views

## Commands

- **Yii2: Go to View** — navigate to view from cursor in `render(...)`
- **Yii2: Create View from render()** — suggest path for creating missing view
- **Yii2: Go to Route Controller** — from `Url::to(['site/index'])` / `redirect` open controller

## Current OpenIDEA API Limitations

- No direct access to PHP PSI / type inference → everything via text scanning.
- No convenient API for creating files in project → create-view only reports path for now.
- No access to Database tool window / schema → ActiveQuery and table existence unavailable.
- Providers have strict 50 ms budget — heavy indexes must be cached.

## Indexes (TTL 30s)

### Controllers
- Scans `**/controllers/**/*Controller.php`, `**/*Controller.php`
- Fields: `id` (site), `class`, `module` (from `modules/<id>/`), `view_path`, `actions` (actionIndex → index)
- Context for current file: the controller itself or `views/<controller>/…`

### Views resolve
| Call | Path |
|------|------|
| `render('index')` in `SiteController` | `views/site/index.php` |
| same in `modules/blog/.../PostController` | `modules/blog/views/post/index.php` |
| `render('//site/about')` / `render('/site/about')` | `views/site/about.php` |
| `render('@app/views/layouts/main')` | `views/layouts/main.php` |

Completion shows relative names for current controller first.

### Models
- `**/models/**`, `**/forms/**`, advanced template paths, fallback `*Model.php` / `*Form.php`
- Attributes from public props + `rules()` / `attributes()` / `scenarios()`
- Resolves `$var` via `@var`, `new`, type-hint

### i18n
- Scans `**/messages/**/*.php` (and `translations/`)
- Category = filename (`app.php` → `app`)
- Keys — left side of `'key' => '…'`
- `Yii::t('app', '…')`: arg0 → categories, arg1 → keys of this category

### Routes (including with parameters)
- Catalog from controller index: `module/controller/action`
- Name context: `Url::to(`, `toRoute`, `redirect`, `Html::a`, `['post/view', …]`
- **Parameters**:
  - on key `'id' =>` — name completion
  - after comma `['post/view', |]` — `ExpectParam`, inserts `'id' => `
  - already specified keys not offered again (`used`)
- Param sources:
  1. `urlManager` rules: pattern **and** target (`<id>` from both sides)
  2. Action signature: `actionView($id, $slug = null)`
  3. Heuristic: `view|update|delete|edit` → `id`
- Pretty URL: `post/42` ↔ `post/<id:\d+>` (`bind_pattern` / `pattern_matches`)
- Rule completion inserts **internal** route (`post/view`), not pretty pattern
- Detail: `route · id, slug` / `rule → post/view · id`

## Future Development

1. Background document indexer (controller ↔ view gutter, route ↔ action line).
2. Write API — actual view creation + params.
3. Migration helper via `process:php` + yii console.
4. Index invalidation on `file-saved` instead of only TTL.
5. Navigate to exact `actionX` line in controller (currently — file start).
6. Support `['/module/controller/action']` and REST patterns in rules.

## License

MIT (same as original plugin).  
Original: https://github.com/nvlad/yii2support
