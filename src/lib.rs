//! Yii2 Support for OpenIDEA (port of nvlad/yii2support).
//!
//! Features:
//! - Views, controllers, parameterized routes, i18n, ActiveRecord/forms.
//! - Aliases (@app, Yii::getAlias), application components (Yii::$app->…).
//! - Asset bundles: go-to JS/CSS from $js/$css; Asset::register completion.
//! - Console commands, migrations, behaviors(), widgets.
//! - Yii2 JS (yii.activeForm / yiiActiveForm) completion for JavaScript files.

use openidea_plugin::host;
use openidea_plugin::providers::{
    CodeAction, CompletionItem, CompletionKind, Document, Providers,
};
use openidea_plugin::text::{offset, position};
use openidea_plugin::{
    Error, Invocation, Location, NotificationLevel, Plugin, Position, Project, Range,
    register_plugin,
};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use std::hash::BuildHasherDefault;
use ahash::AHasher;
use parking_lot::RwLock;
use lazy_static::lazy_static;

/// Type alias for fast HashMap using ahash
type FastMap<K, V> = HashMap<K, V, BuildHasherDefault<AHasher>>;
/// Type alias for fast HashSet using ahash
type FastSet<T> = HashSet<T, BuildHasherDefault<AHasher>>;

/// Hard caps — host allows ~50ms for providers; stay well under that.
const MAX_SCAN: u32 = 25;           // Drastically reduced for timeout prevention
const MAX_FILE_BYTES: usize = 32 * 1024;  // Reduced to 32KB
const MAX_COMPLETION: usize = 32;
const MAX_CONFIG_FILES: u32 = 6;  // Drastically reduced
const MAX_ASSET_SCAN: u32 = 15;     // Drastically reduced
const VIEW_CODE: &str = "yii2-missing-view";

/// Operation budget for completion/definition to prevent timeouts.
/// Each file read/find_files counts as 1 op; parsing counts as 0.1 ops.
/// Target: stay under ~100 ops to fit in ~50ms WASM budget.
const MAX_OPS: u32 = 100;

/// Simple operation counter for budget tracking.
#[derive(Default)]
struct OpBudget {
    ops: u32,
}

/// Cache invalidation event for file-saved notifications.
#[derive(Clone, Debug)]
enum CacheInvalidation {
    Controllers,
    Views,
    Models,
    Messages,
    UrlRules,
    AliasesComponents,
    Assets,
    ConsoleCommands,
    Migrations,
    Behaviors,
    Widgets,
    ViewVars,
    All,
}

lazy_static! {
    static ref FILE_MTIMES: RwLock<FastMap<String, u64>> = RwLock::new(FastMap::default());
    static ref CACHE_VERSION: RwLock<u64> = RwLock::new(0);
}

/// Increment cache version to invalidate all indexes.
fn invalidate_all_caches() {
    let mut v = CACHE_VERSION.write();
    *v += 1;
}

/// Invalidate specific cache by updating file mtime.
fn invalidate_file_cache(path: &str) {
    let mut mtimes = FILE_MTIMES.write();
    mtimes.insert(path.to_owned(), current_time_millis());
}

/// Get current time in milliseconds.
fn current_time_millis() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64
}

/// Check if a file has been modified since last cache build.
fn is_file_modified(path: &str, cached_mtime: u64) -> bool {
    let mtimes = FILE_MTIMES.read();
    mtimes.get(path).copied().unwrap_or(0) > cached_mtime
}

/// Get current cache version.
fn get_cache_version() -> u64 {
    *CACHE_VERSION.read()
}

impl OpBudget {
    #[inline]
    fn new() -> Self {
        Self { ops: 0 }
    }

    #[inline]
    fn spend(&mut self, cost: u32) -> bool {
        self.ops += cost;
        self.ops <= MAX_OPS
    }

    #[inline]
    fn check(&self) -> bool {
        self.ops <= MAX_OPS
    }

    #[inline]
    fn spent(&self) -> u32 {
        self.ops
    }
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

/// Plugin settings loaded from settings.json
#[derive(Debug, Clone)]
pub struct PluginSettings {
    pub view_path: String,
    pub table_prefix: String,
    pub report_missing_views: bool,
    pub scan_paths: ScanPathsSettings,
    pub features: FeatureSettings,
    pub performance: PerformanceSettings,
    pub advanced: AdvancedSettings,
}

#[derive(Debug, Clone)]
pub struct ScanPathsSettings {
    pub controllers: Vec<String>,
    pub models: Vec<String>,
    pub views: Vec<String>,
    pub messages: Vec<String>,
    pub assets: Vec<String>,
    pub widgets: Vec<String>,
    pub commands: Vec<String>,
    pub migrations: Vec<String>,
    pub config: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct FeatureSettings {
    pub views: bool,
    pub routes: bool,
    pub models: bool,
    pub i18n: bool,
    pub components: bool,
    pub aliases: bool,
    pub assets: bool,
    pub console: bool,
    pub migrations: bool,
    pub behaviors: bool,
    pub widgets: bool,
    pub active_form_js: bool,
}

#[derive(Debug, Clone)]
pub struct PerformanceSettings {
    pub max_scan_files: u32,
    pub max_file_size_kb: usize,
    pub max_completions: usize,
    pub cache_ttl_seconds: u32,
    pub enable_op_budget: bool,
}

#[derive(Debug, Clone)]
pub struct AdvancedSettings {
    pub project_type: ProjectType,
    pub module_detection: bool,
    pub url_manager_config_keys: Vec<String>,
    pub custom_aliases: BTreeMap<String, String>,
    pub ignored_paths: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectType {
    Basic,
    Advanced,
    Custom,
}

impl Default for ScanPathsSettings {
    fn default() -> Self {
        Self {
            controllers: vec!["controllers".into(), "modules/*/controllers".into()],
            models: vec!["models".into(), "modules/*/models".into(), "forms".into(), "modules/*/forms".into()],
            views: vec!["views".into(), "modules/*/views".into()],
            messages: vec!["messages".into(), "modules/*/messages".into()],
            assets: vec!["assets".into(), "modules/*/assets".into()],
            widgets: vec!["widgets".into(), "components".into(), "modules/*/widgets".into()],
            commands: vec!["commands".into(), "modules/*/commands".into()],
            migrations: vec!["migrations".into(), "modules/*/migrations".into(), "console/migrations".into()],
            config: vec!["config".into(), "common/config".into(), "frontend/config".into(), "backend/config".into(), "console/config".into()],
        }
    }
}

impl Default for FeatureSettings {
    fn default() -> Self {
        Self {
            views: true,
            routes: true,
            models: true,
            i18n: true,
            components: true,
            aliases: true,
            assets: true,
            console: true,
            migrations: true,
            behaviors: true,
            widgets: true,
            active_form_js: true,
        }
    }
}

impl Default for PerformanceSettings {
    fn default() -> Self {
        Self {
            max_scan_files: 25,
            max_file_size_kb: 32,
            max_completions: 32,
            cache_ttl_seconds: 30,
            enable_op_budget: true,
        }
    }
}

impl Default for AdvancedSettings {
    fn default() -> Self {
        Self {
            project_type: ProjectType::Basic,
            module_detection: true,
            url_manager_config_keys: vec!["urlManager".into(), "components.urlManager".into()],
            custom_aliases: BTreeMap::new(),
            ignored_paths: vec!["vendor".into(), "runtime".into(), ".git".into(), "node_modules".into(), "tests/_output".into()],
        }
    }
}

impl Default for PluginSettings {
    fn default() -> Self {
        Self {
            view_path: "@app/views".into(),
            table_prefix: "".into(),
            report_missing_views: true,
            scan_paths: ScanPathsSettings::default(),
            features: FeatureSettings::default(),
            performance: PerformanceSettings::default(),
            advanced: AdvancedSettings::default(),
        }
    }
}

/// Load settings from the Invocation context.
/// Returns default settings if the API is not available.
fn load_settings(_ctx: &Invocation) -> PluginSettings {
    // TODO: When openidea-plugin provides settings API, read from ctx.settings()
    // For now, return defaults. The settings.json defines the schema for the IDE.
    PluginSettings::default()
}

/// Load settings for a project (uses defaults for now, will read from IDE when API available).
fn load_settings_for_project(_project: &Project) -> PluginSettings {
    PluginSettings::default()
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn project_of(path: &str) -> Option<Project> {
    host::projects()
        .ok()?
        .into_iter()
        .filter(|p| p.root().is_ok_and(|r| path.starts_with(&r)))
        .max_by_key(|p| p.root().map(|r| r.len()).unwrap_or(0))
}

fn range_of(text: &str, start: usize, end: usize) -> Range {
    Range::new(position(text, start), position(text, end))
}

fn is_ident(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

#[inline]
fn push_capped(items: &mut Vec<CompletionItem>, it: CompletionItem) {
    if items.len() < MAX_COMPLETION {
        items.push(it);
    }
}

#[inline]
fn file_too_big(len: usize) -> bool {
    len > MAX_FILE_BYTES
}

/// Spend budget for a file read operation.
#[inline]
fn budget_read(budget: &mut OpBudget) -> bool {
    budget.spend(1)
}

/// Spend budget for a find_files operation.
#[inline]
fn budget_find(budget: &mut OpBudget) -> bool {
    budget.spend(2) // find_files is more expensive
}

/// Spend budget for parsing a file.
#[inline]
fn budget_parse(budget: &mut OpBudget) -> bool {
    budget.spend(1)
}

/// Byte offset of the cursor from a Position.
fn cursor_offset(text: &str, pos: Position) -> usize {
    offset(text, pos).unwrap_or(0)
}

/// Build a completion item with SDK v0.1.37 fields.
fn item(label: impl Into<String>, kind: CompletionKind, detail: impl Into<String>, insert: impl Into<String>) -> CompletionItem {
    CompletionItem {
        label: label.into(),
        detail: Some(detail.into()),
        kind: Some(kind),
        insert_text: insert.into(),
        snippet: false,
        replace: None,
        sort_text: None,
        filter_text: None,
        documentation: None,
        additional_edits: Vec::new(),
    }
}

fn item_filter(
    label: impl Into<String>,
    kind: CompletionKind,
    detail: impl Into<String>,
    insert: impl Into<String>,
    filter: impl Into<String>,
) -> CompletionItem {
    let mut i = item(label, kind, detail, insert);
    i.filter_text = Some(filter.into());
    i
}


// ---------------------------------------------------------------------------
// Controller index + view resolution
// ---------------------------------------------------------------------------

/// One controller discovered in the project.
#[derive(Clone, Debug, Default)]
struct ControllerInfo {
    /// Simple class name without suffix: Site, User, …
    id: String,
    /// Full class name: SiteController
    class: String,
    path: String,
    /// Module id if under modules/<id>/
    module: Option<String>,
    /// View subdir for this controller: "site", or "user/profile" for nested
    view_path: String,
    /// actionXxxx → view name hint (xxxx in kebab/underscore form used by Yii)
    actions: Vec<String>,
}

/// Parse a *Controller.php file.
fn parse_controller(path: &str, text: &str) -> Option<ControllerInfo> {
    let class_idx = text.find("class ")?;
    let after = text[class_idx + 6..].trim_start();
    let class: String = after.chars().take_while(|c| is_ident(*c)).collect();
    if !class.ends_with("Controller") || class == "Controller" {
        return None;
    }
    let id = class.trim_end_matches("Controller").to_owned();
    if id.is_empty() {
        return None;
    }

    // module from path: .../modules/<module>/controllers/...
    let norm = path.replace('\\', "/");
    let module = norm.find("/modules/").and_then(|i| {
        let rest = &norm[i + 9..];
        let mod_id = rest.split('/').next()?;
        if mod_id.is_empty() || mod_id == "controllers" {
            None
        } else {
            Some(mod_id.to_owned())
        }
    });

    // Yii view path: SiteController → "site", AdminUserController → "admin-user" (approx)
    // Keep simple: lowercase id; nested controllers use id as folder.
    let view_path = id
        .chars()
        .enumerate()
        .flat_map(|(i, c)| {
            if c.is_uppercase() && i > 0 {
                vec!['-', c.to_ascii_lowercase()]
            } else {
                vec![c.to_ascii_lowercase()]
            }
        })
        .collect::<String>();

    let mut actions = Vec::new();
    // actionIndex, actionCreate, …
    let bytes = text.as_bytes();
    let mut i = 0;
    while i + 6 < bytes.len() {
        if &bytes[i..i + 6] == b"action"
            && bytes.get(i + 6).is_some_and(|b| b.is_ascii_uppercase())
            && (i == 0 || !is_ident(bytes[i - 1] as char))
        {
            let start = i + 6;
            let mut j = start;
            while j < bytes.len() && is_ident(bytes[j] as char) {
                j += 1;
            }
            if j > start {
                let name = &text[start..j];
                // Yii converts actionCreate → "create"
                let view = name
                    .chars()
                    .enumerate()
                    .flat_map(|(k, c)| {
                        if c.is_uppercase() && k > 0 {
                            vec!['-', c.to_ascii_lowercase()]
                        } else {
                            vec![c.to_ascii_lowercase()]
                        }
                    })
                    .collect::<String>();
                actions.push(view);
            }
            i = j;
        } else {
            i += 1;
        }
    }

    Some(ControllerInfo {
        id: id.to_ascii_lowercase(),
        class,
        path: path.to_owned(),
        module,
        view_path,
        actions,
    })
}

/// Infer controller context from the current file path / content.
fn controller_context_from_path(path: &str, text: &str) -> Option<ControllerInfo> {
    if path.contains("Controller.php") || path.ends_with("Controller.php") {
        return parse_controller(path, text);
    }
    // Inside a view file: .../views/<controller>/xxx.php or modules/<m>/views/<c>/
    let norm = path.replace('\\', "/");
    if let Some(idx) = norm.find("/views/") {
        let rest = &norm[idx + 7..];
        let parts: Vec<&str> = rest.split('/').collect();
        if !parts.is_empty() {
            let controller_folder = parts[0].trim_end_matches(".php");
            if !controller_folder.is_empty() && parts.len() >= 1 {
                let module = norm.find("/modules/").and_then(|i| {
                    let r = &norm[i + 9..];
                    let m = r.split('/').next()?;
                    if m.is_empty() { None } else { Some(m.to_owned()) }
                });
                return Some(ControllerInfo {
                    id: controller_folder.to_owned(),
                    class: format!("{}Controller", capitalize(controller_folder)),
                    path: path.to_owned(),
                    module,
                    view_path: controller_folder.to_owned(),
                    actions: Vec::new(),
                });
            }
        }
    }
    None
}

/// Extract string literals that look like view names from a render call near cursor.
fn view_context(text: &str, offset: usize) -> Option<(String, Range)> {
    let before = &text[..offset.min(text.len())];
    let methods = ["render(", "renderPartial(", "renderAjax(", "renderFile("];
    let mut best = None;
    for m in methods {
        if let Some(i) = before.rfind(m) {
            if best.map_or(true, |(bi, _)| i > bi) {
                best = Some((i, m.len()));
            }
        }
    }
    let (start, mlen) = best?;
    let after = &text[start + mlen..];
    let mut i = 0;
    while i < after.len() && after.as_bytes()[i].is_ascii_whitespace() {
        i += 1;
    }
    if after.as_bytes().get(i) != Some(&b'\'') && after.as_bytes().get(i) != Some(&b'"') {
        return None;
    }
    let quote = after.as_bytes()[i];
    i += 1;
    let lit_start = start + mlen + i;
    let mut j = i;
    while j < after.len() && after.as_bytes()[j] != quote {
        if after.as_bytes()[j] == b'\\' {
            j += 1;
        }
        j += 1;
    }
    let lit_end = start + mlen + j;
    let name = after[i..j].to_owned();
    if offset < lit_start || offset > lit_end + 1 {
        return None;
    }
    Some((name, range_of(text, lit_start, lit_end)))
}

/// Resolve a Yii view name to a file path using controller/module context.
///
/// Rules (Yii2):
/// - `//site/index` or `/site/index` → app views/site/index
/// - `@app/views/...` / `@module/views/...` → strip alias
/// - relative `index` from SiteController → views/site/index
/// - relative from module controller → modules/<m>/views/<c>/index
fn resolve_view_path(
    project: &Project,
    view: &str,
    ctx: Option<&ControllerInfo>,
) -> Option<String> {
    let root = project.root().ok()?;
    let raw = view.trim();

    // Absolute / alias forms
    let (absolute, cleaned) = if raw.starts_with("//") {
        (true, raw.trim_start_matches('/'))
    } else if raw.starts_with('/') {
        (true, raw.trim_start_matches('/'))
    } else if let Some(rest) = raw.strip_prefix("@app/views/") {
        (true, rest)
    } else if let Some(rest) = raw.strip_prefix("@app/") {
        (true, rest.trim_start_matches("views/"))
    } else if raw.starts_with('@') {
        // @moduleId/views/... or unknown alias — strip first segment after @
        let without = raw.trim_start_matches('@');
        if let Some(idx) = without.find("/views/") {
            (true, &without[idx + 7..])
        } else {
            (false, without)
        }
    } else {
        (false, raw)
    };

    let mut candidates: Vec<String> = Vec::new();

    if absolute {
        candidates.push(format!("views/{cleaned}.php"));
        candidates.push(format!("**/views/{cleaned}.php"));
        if let Some(c) = ctx {
            if let Some(m) = &c.module {
                candidates.push(format!("modules/{m}/views/{cleaned}.php"));
            }
        }
        candidates.push(format!("modules/*/views/{cleaned}.php"));
    } else if let Some(c) = ctx {
        // Relative to controller view path
        let vp = &c.view_path;
        if let Some(m) = &c.module {
            candidates.push(format!("modules/{m}/views/{vp}/{cleaned}.php"));
            candidates.push(format!("modules/{m}/views/{cleaned}.php"));
        }
        candidates.push(format!("views/{vp}/{cleaned}.php"));
        candidates.push(format!("views/{cleaned}.php"));
        // advanced template: frontend/views, backend/views
        candidates.push(format!("frontend/views/{vp}/{cleaned}.php"));
        candidates.push(format!("backend/views/{vp}/{cleaned}.php"));
        if let Some(m) = &c.module {
            candidates.push(format!("**/modules/{m}/views/{vp}/{cleaned}.php"));
        }
    } else {
        candidates.push(format!("views/{cleaned}.php"));
        candidates.push(format!("**/views/{cleaned}.php"));
        candidates.push(format!("**/views/**/{cleaned}.php"));
    }

    for rel in &candidates {
        let pattern = if rel.contains('*') {
            rel.clone()
        } else {
            format!("**/{rel}")
        };
        if let Ok(files) = project.find_files(&pattern, 8) {
            if let Some(f) = files.into_iter().next() {
                return Some(f);
            }
        }
        if !rel.contains('*') {
            let full = format!("{root}/{rel}");
            if project.read_file(&full).is_ok() {
                return Some(full);
            }
        }
    }

    // Last resort: basename search under views/
    let needle = cleaned.rsplit('/').next().unwrap_or(cleaned);
    if let Ok(files) = project.find_files(&format!("**/views/**/{needle}.php"), 20) {
        // Prefer path that contains controller view_path if we have context
        if let Some(c) = ctx {
            if let Some(f) = files.iter().find(|p| p.contains(&c.view_path)) {
                return Some(f.clone());
            }
        }
        files.into_iter().next()
    } else {
        None
    }
}

/// Collect all view files under views/ for completion (optionally scoped).
fn list_views(project: &Project) -> Vec<String> {
    list_views_with_budget(project, None, None)
}

fn list_views_with_budget(project: &Project, mut budget: Option<&mut OpBudget>, settings: Option<&PluginSettings>) -> Vec<String> {
    let mut out = BTreeSet::new();
    if budget.as_ref().map_or(false, |b| !b.check()) {
        return Vec::new();
    }
    let max_scan = settings.map(|s| s.performance.max_scan_files).unwrap_or(MAX_SCAN);
    // Use configurable view scan paths
    let patterns: Vec<String> = settings
        .map(|s| s.scan_paths.views.iter().map(|p| format!("{}/**/*.php", p.trim_end_matches('/'))).collect())
        .unwrap_or_else(|| vec!["**/views/**/*.php".into()]);
    for pat in patterns {
        if budget.as_ref().map_or(false, |b| !b.check()) { break; }
        let Ok(files) = project.find_files(&pat, max_scan) else { continue };
        if budget.as_ref().map_or(false, |b| !b.check()) { return Vec::new(); }
        if let Some(b) = budget.as_mut() { if !b.spend(2) { return Vec::new(); } }
        for f in files {
            if budget.as_ref().map_or(false, |b| !b.check()) { break; }
            let norm = f.replace('\\', "/");
            // Find the view path part after any of the configured view paths
            for view_base in &settings.map(|s| s.scan_paths.views.clone()).unwrap_or_else(|| vec!["views".into()]) {
                let view_base_norm = view_base.trim_end_matches('/').replace('\\', "/");
                let search = format!("{}/", view_base_norm);
                if let Some(idx) = norm.find(&search) {
                    let rel = &norm[idx + search.len()..];
                    let name = rel.trim_end_matches(".php");
                    if !name.is_empty() {
                        out.insert(name.to_owned());
                    }
                    break;
                }
            }
        }
    }
    out.into_iter().collect()
}

/// Views relevant for completion given controller context (relative names first).
fn list_views_for_context(all: &[String], ctx: Option<&ControllerInfo>) -> Vec<String> {
    let Some(c) = ctx else {
        return all.to_vec();
    };
    let prefix = format!("{}/", c.view_path);
    let mut preferred = Vec::new();
    let mut rest = Vec::new();
    for v in all {
        if let Some(rel) = v.strip_prefix(&prefix) {
            preferred.push(rel.to_owned());
            preferred.push(v.clone());
        } else if v == &c.view_path {
            preferred.push(v.clone());
        } else {
            rest.push(v.clone());
        }
    }
    preferred.extend(rest);
    preferred
}


/// Variable passed into a view via render($view, ['name' => expr]).
#[derive(Clone, Debug, Default)]
struct ViewVarInfo {
    name: String,
    /// Short source expression when known (`$model`, `1`, `...`).
    source: String,
}

/// Extract render()/renderPartial() pairs: (resolved view key, vars).
/// View key is relative when possible: `site/index` or `index` with controller view_path applied by caller.
fn extract_render_calls(text: &str) -> Vec<(String /*view lit*/, Vec<ViewVarInfo>)> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let methods: &[&[u8]] = &[b"render(", b"renderPartial(", b"renderAjax("];
    let mut i = 0usize;
    while i < bytes.len() {
        let mut hit = None;
        for m in methods {
            if i + m.len() <= bytes.len() && &bytes[i..i + m.len()] == *m {
                // word boundary
                if i > 0 && is_ident(bytes[i - 1] as char) {
                    continue;
                }
                hit = Some(m.len());
                break;
            }
        }
        let Some(mlen) = hit else {
            i += 1;
            continue;
        };
        let mut j = i + mlen;
        // skip whitespace
        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
            j += 1;
        }
        // view name string
        if j >= bytes.len() || (bytes[j] != b'\'' && bytes[j] != b'"') {
            i += mlen;
            continue;
        }
        let q = bytes[j];
        j += 1;
        let vs = j;
        while j < bytes.len() && bytes[j] != q {
            if bytes[j] == b'\\' {
                j += 1;
            }
            j += 1;
        }
        if j >= bytes.len() {
            break;
        }
        let view_lit = text[vs..j].to_owned();
        j += 1;
        // find second arg array
        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
            j += 1;
        }
        if j < bytes.len() && bytes[j] == b',' {
            j += 1;
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'[' {
                let (vars, end) = parse_php_array_keys(text, j);
                if !vars.is_empty() {
                    out.push((view_lit, vars));
                }
                i = end.max(j + 1);
                continue;
            }
        }
        i = j;
    }
    out
}

/// Parse PHP array starting at `[` — collect 'key' => expr pairs (string keys only).
fn parse_php_array_keys(text: &str, start: usize) -> (Vec<ViewVarInfo>, usize) {
    let bytes = text.as_bytes();
    if start >= bytes.len() || bytes[start] != b'[' {
        return (Vec::new(), start);
    }
    let mut i = start + 1;
    let mut depth = 1i32;
    let mut vars = Vec::new();
    let mut in_str: Option<u8> = None;
    while i < bytes.len() && depth > 0 {
        let b = bytes[i];
        if let Some(q) = in_str {
            if b == b'\\' {
                i += 2;
                continue;
            }
            if b == q {
                in_str = None;
            }
            i += 1;
            continue;
        }
        match b {
            b'\'' | b'"' => {
                in_str = Some(b);
                i += 1;
            }
            b'[' => {
                depth += 1;
                i += 1;
            }
            b']' => {
                depth -= 1;
                i += 1;
            }
            _ => i += 1,
        }
    }
    let end = i;
    // scan only top-level keys inside the array slice
    let body = &text[start + 1..end.saturating_sub(1).min(text.len())];
    let bb = body.as_bytes();
    let mut k = 0usize;
    let mut depth2 = 0i32;
    let mut in_s: Option<u8> = None;
    while k < bb.len() {
        let b = bb[k];
        if let Some(q) = in_s {
            if b == b'\\' {
                k += 2;
                continue;
            }
            if b == q {
                in_s = None;
            }
            k += 1;
            continue;
        }
        match b {
            b'\'' | b'"' if depth2 == 0 => {
                let q = b;
                k += 1;
                let ks = k;
                while k < bb.len() {
                    if bb[k] == b'\\' {
                        k += 2;
                        continue;
                    }
                    if bb[k] == q {
                        break;
                    }
                    k += 1;
                }
                let key = body[ks..k.min(body.len())].to_owned();
                if k < bb.len() {
                    k += 1;
                }
                // skip ws and =>
                while k < bb.len() && bb[k].is_ascii_whitespace() {
                    k += 1;
                }
                if k + 1 < bb.len() && &bb[k..k + 2] == b"=>" {
                    k += 2;
                    while k < bb.len() && bb[k].is_ascii_whitespace() {
                        k += 1;
                    }
                    let vs = k;
                    // expression until comma at depth 0 or end
                    let mut d = 0i32;
                    let mut is: Option<u8> = None;
                    while k < bb.len() {
                        let c = bb[k];
                        if let Some(qq) = is {
                            if c == b'\\' {
                                k += 2;
                                continue;
                            }
                            if c == qq {
                                is = None;
                            }
                            k += 1;
                            continue;
                        }
                        match c {
                            b'\'' | b'"' => {
                                is = Some(c);
                                k += 1;
                            }
                            b'(' | b'[' => {
                                d += 1;
                                k += 1;
                            }
                            b')' | b']' => {
                                d -= 1;
                                k += 1;
                            }
                            b',' if d <= 0 => break,
                            _ => k += 1,
                        }
                    }
                    let mut src = body[vs..k].trim().to_owned();
                    if src.len() > 40 {
                        src.truncate(37);
                        src.push_str("...");
                    }
                    if !key.is_empty() && key.chars().all(|c| is_ident(c)) {
                        vars.push(ViewVarInfo {
                            name: key,
                            source: src,
                        });
                    }
                }
            }
            b'[' | b'(' => {
                depth2 += 1;
                k += 1;
            }
            b']' | b')' => {
                depth2 -= 1;
                k += 1;
            }
            _ => k += 1,
        }
    }
    (vars, end)
}

/// Map absolute/relative view file path → candidate index keys (`site/index`, `index`, …).
fn view_keys_from_path(path: &str) -> Vec<String> {
    let norm = path.replace('\\', "/");
    let Some(idx) = norm.find("/views/") else {
        return Vec::new();
    };
    let rest = norm[idx + 7..].trim_end_matches(".php");
    if rest.is_empty() {
        return Vec::new();
    }
    let mut keys = vec![rest.to_owned()];
    // also basename and parent/basename variants
    if let Some((_, base)) = rest.rsplit_once('/') {
        keys.push(base.to_owned());
        // site/index already there; index alone
    }
    keys
}

/// Resolve render view literal against controller view_path → index keys.
fn render_view_keys(view_lit: &str, ctx: &ControllerInfo) -> Vec<String> {
    let raw = view_lit.trim().trim_start_matches('/');
    let mut keys = Vec::new();
    if raw.starts_with("//") {
        keys.push(raw.trim_start_matches('/').to_owned());
    } else if raw.starts_with('/') {
        keys.push(raw.trim_start_matches('/').to_owned());
    } else if raw.contains('/') {
        keys.push(raw.to_owned());
        if let Some(m) = &ctx.module {
            keys.push(format!("{m}/{raw}"));
        }
    } else {
        keys.push(format!("{}/{}", ctx.view_path, raw));
        keys.push(raw.to_owned());
        if let Some(m) = &ctx.module {
            keys.push(format!("{m}/{}/{}", ctx.view_path, raw));
        }
    }
    keys
}

/// Cursor on `$partial` inside a PHP view (not `$_GET` etc.).
fn view_dollar_var_context(text: &str, offset: usize) -> Option<String> {
    let before = &text[..offset.min(text.len())];
    let dollar = before.rfind('$')?;
    // not part of $$ or already closed
    let after = &before[dollar + 1..];
    if after.chars().any(|c| !is_ident(c)) {
        return None;
    }
    // ignore superglobals prefix when full name typed
    let partial = after.to_owned();
    if matches!(
        partial.as_str(),
        "this" | "_GET" | "_POST" | "_REQUEST" | "_SERVER" | "_SESSION" | "_COOKIE" | "_FILES" | "_ENV"
    ) {
        return None;
    }
    // Ensure `$` is expression start (not inside string roughly)
    let line_start = before.rfind('\n').map(|i| i + 1).unwrap_or(0);
    let line = &before[line_start..];
    let mut singles = 0;
    let mut doubles = 0;
    for ch in line.chars() {
        if ch == '\'' {
            singles += 1;
        }
        if ch == '"' {
            doubles += 1;
        }
    }
    if singles % 2 == 1 || doubles % 2 == 1 {
        return None;
    }
    Some(partial)
}


// ---------------------------------------------------------------------------
// ActiveRecord / model index
// ---------------------------------------------------------------------------

/// One model/form class discovered in the project.
#[derive(Clone, Debug, Default)]
struct ModelInfo {
    /// Simple class name (User, LoginForm, …).
    class: String,
    /// Fully-qualified name if package/namespace known.
    fqn: Option<String>,
    /// Path of the defining file.
    path: String,
    /// Attribute names (public props + rules() + scenarios heuristics).
    attributes: Vec<String>,
}

/// Extract class name + attributes from a PHP file that looks like a Model/Form.
fn parse_model(path: &str, text: &str) -> Option<ModelInfo> {
    // Must look like a model-ish class
    let class_idx = text.find("class ")?;
    let after_class = text[class_idx + 6..].trim_start();
    let class: String = after_class.chars().take_while(|c| is_ident(*c)).collect();
    if class.is_empty() {
        return None;
    }
    // Prefer *Model, *Form, or classes that extend Model / ActiveRecord
    let looks_model = class.ends_with("Model")
        || class.ends_with("Form")
        || text.contains("extends Model")
        || text.contains("extends \\yii\\base\\Model")
        || text.contains("extends ActiveRecord")
        || text.contains("extends \\yii\\db\\ActiveRecord")
        || text.contains("extends \\yii\\base\\DynamicModel");
    if !looks_model {
        return None;
    }

    let mut attrs = FastSet::default();

    // public $foo;  /  public string $foo;
    for line in text.lines() {
        let t = line.trim();
        // public $name;  or  public ?string $name = null;
        if let Some(rest) = t.strip_prefix("public ") {
            // skip methods
            if rest.contains('(') {
                continue;
            }
            if let Some(dollar) = rest.find('$') {
                let name: String = rest[dollar + 1..]
                    .chars()
                    .take_while(|c| is_ident(*c))
                    .collect();
                if !name.is_empty() {
                    attrs.insert(name);
                }
            }
        }
    }

    // attributes() method return array
    extract_string_keys_from_method(text, "function attributes(", &mut attrs);
    // rules()
    extract_string_keys_from_method(text, "function rules(", &mut attrs);
    // scenarios()
    extract_string_keys_from_method(text, "function scenarios(", &mut attrs);

    let fqn = text.lines().find_map(|l| {
        let t = l.trim();
        t.strip_prefix("namespace ").map(|n| {
            let ns = n.trim_end_matches(';').trim();
            format!("{ns}\\{class}")
        })
    });

    Some(ModelInfo {
        class,
        fqn,
        path: path.to_owned(),
        attributes: attrs.into_iter().collect(),
    })
}

/// Pull short string literals that look like attribute names out of a method body.
fn extract_string_keys_from_method(text: &str, sig: &str, attrs: &mut FastSet<String>) {
    let Some(i) = text.find(sig) else { return };
    let body = &text[i..];
    let mut depth = 0i32;
    let mut in_str = None::<u8>;
    let bytes = body.as_bytes();
    let mut k = 0;
    while k < bytes.len() {
        match bytes[k] {
            b'{' if in_str.is_none() => {
                depth += 1;
                k += 1;
            }
            b'}' if in_str.is_none() => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
                k += 1;
            }
            b'\'' | b'"' if in_str.is_none() => {
                in_str = Some(bytes[k]);
                k += 1;
                let start = k;
                while k < bytes.len() && bytes[k] != in_str.unwrap_or(0) {
                    if bytes[k] == b'\\' {
                        k += 1;
                    }
                    k += 1;
                }
                let s = &body[start..k];
                // Single identifier or comma-separated list of identifiers
                if s.len() < 48 && !s.contains("::") && !s.contains('/') && !s.contains('\\') {
                    for part in s.split(|c: char| !is_ident(c)) {
                        if part.len() >= 1
                            && part.len() < 40
                            && !matches!(
                                part,
                                "safe"
                                    | "required"
                                    | "string"
                                    | "integer"
                                    | "number"
                                    | "boolean"
                                    | "email"
                                    | "url"
                                    | "default"
                                    | "filter"
                                    | "trim"
                                    | "unique"
                                    | "exist"
                                    | "compare"
                                    | "in"
                                    | "match"
                                    | "captcha"
                                    | "file"
                                    | "image"
                                    | "date"
                                    | "datetime"
                                    | "time"
                            )
                        {
                            attrs.insert(part.to_owned());
                        }
                    }
                }
                in_str = None;
                k += 1;
            }
            _ => k += 1,
        }
    }
}

/// Variable name used in field($model, …) / Html::active* near the cursor.
fn field_model_var(text: &str, offset: usize) -> Option<String> {
    let before = &text[..offset.min(text.len())];
    let patterns = [
        "->field(",
        "Html::active",
        "Html::error(",
        "Html::label(",
        "Html::activeLabel(",
        "Html::activeHint(",
        "Html::activeInput(",
        "Html::activeTextInput(",
        "Html::activePasswordInput(",
        "Html::activeTextarea(",
        "Html::activeCheckbox(",
        "Html::activeDropDownList(",
        "Html::activeListBox(",
        "Html::activeRadioList(",
        "Html::activeCheckboxList(",
    ];
    for p in patterns {
        if let Some(i) = before.rfind(p) {
            let after = &text[i + p.len()..];
            let mut j = 0;
            while j < after.len() && after.as_bytes()[j].is_ascii_whitespace() {
                j += 1;
            }
            if after.as_bytes().get(j) == Some(&b'$') {
                j += 1;
                let name: String = after[j..]
                    .chars()
                    .take_while(|c| is_ident(*c))
                    .collect();
                if !name.is_empty() {
                    return Some(name);
                }
            }
        }
    }
    None
}

/// Try to resolve $var to a model class by scanning type hints / new / phpdoc in the file.
fn resolve_var_to_class(text: &str, var: &str) -> Option<String> {
    // @var User $model   or   /** @var UserForm $form */
    let needle = format!("${var}");
    for line in text.lines() {
        let t = line.trim();
        if t.contains("@var") && t.contains(&needle) {
            // @var ClassName $var
            if let Some(rest) = t.split("@var").nth(1) {
                let cls: String = rest
                    .trim()
                    .trim_start_matches('\\')
                    .chars()
                    .take_while(|c| is_ident(*c) || *c == '\\')
                    .collect();
                let simple = cls.rsplit('\\').next().unwrap_or(&cls);
                if !simple.is_empty() {
                    return Some(simple.to_owned());
                }
            }
        }
    }
    // $model = new UserForm(
    let assign = format!("${var} = new ");
    if let Some(i) = text.find(&assign) {
        let after = &text[i + assign.len()..];
        let cls: String = after
            .chars()
            .take_while(|c| is_ident(*c) || *c == '\\')
            .collect();
        let simple = cls.rsplit('\\').next().unwrap_or(&cls);
        if !simple.is_empty() {
            return Some(simple.to_owned());
        }
    }
    // function actionX(UserForm $model)
    let typed = format!(" ${var}");
    for line in text.lines() {
        if line.contains(&typed) {
            // look for ClassName $var
            if let Some(idx) = line.find(&format!("${var}")) {
                let before = line[..idx].trim_end();
                let cls: String = before
                    .rsplit(|c: char| c.is_whitespace() || c == '(' || c == ',')
                    .next()
                    .unwrap_or("")
                    .trim_start_matches('\\')
                    .chars()
                    .take_while(|c| is_ident(*c) || *c == '\\')
                    .collect();
                let simple = cls.rsplit('\\').next().unwrap_or(&cls);
                if !simple.is_empty() && simple != "function" && simple != "public" {
                    return Some(simple.to_owned());
                }
            }
        }
    }
    // Heuristic: $model → *Model / *Form common names
    if var == "model" || var == "form" {
        return None; // let index supply all
    }
    None
}

// ---------------------------------------------------------------------------
// i18n — message files
// ---------------------------------------------------------------------------

/// category → set of message keys
type MessageCatalog = FastMap<String, FastSet<String>>;

/// Parse a Yii message PHP file: return [ 'key' => 'translation', ... ]
fn parse_message_file(text: &str) -> FastSet<String> {
    let mut keys = FastSet::default();
    // Look for 'key' => or "key" => at array entries
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' | b'"' => {
                let quote = bytes[i];
                i += 1;
                let start = i;
                while i < bytes.len() && bytes[i] != quote {
                    if bytes[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
                let key = text[start..i].to_owned();
                if i < bytes.len() {
                    i += 1; // closing quote
                }
                // skip whitespace
                while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                    i += 1;
                }
                // expect =>
                if i + 1 < bytes.len() && bytes[i] == b'=' && bytes[i + 1] == b'>' {
                    if !key.is_empty() && key.len() < 200 && !key.contains('\n') {
                        keys.insert(key);
                    }
                }
            }
            _ => i += 1,
        }
    }
    keys
}

/// Scan messages/ directories for categories and keys.
/// Layout: messages/<lang>/<category>.php  or  messages/<category>.php
fn scan_messages(project: &Project) -> MessageCatalog {
    scan_messages_with_budget(project, None, None)
}

fn scan_messages_with_budget(project: &Project, mut budget: Option<&mut OpBudget>, settings: Option<&PluginSettings>) -> MessageCatalog {
    let mut catalog: MessageCatalog = FastMap::default();
    let max_scan = settings.map(|s| s.performance.max_scan_files).unwrap_or(MAX_SCAN);
    // Use configurable message scan paths
    let patterns: Vec<String> = settings
        .map(|s| s.scan_paths.messages.iter().map(|p| format!("{}/**/*.php", p.trim_end_matches('/'))).collect())
        .unwrap_or_else(|| vec!["**/messages/**/*.php".into()]);
    let mut seen = FastSet::default();
    for pat in patterns {
        if let Some(b) = budget.as_ref() { if !b.check() { break; } }
        let Ok(files) = project.find_files(&pat, max_scan) else { continue };
        if let Some(b) = budget.as_mut() { if !b.spend(2) { break; } }
        for path in files {
            if let Some(b) = budget.as_ref() { if !b.check() { break; } }
            if !seen.insert(path.clone()) {
                continue;
            }
            let Ok(text) = project.read_file(&path) else { continue };
            if text.len() > 1024 * 1024 {
                continue;
            }
            if let Some(b) = budget.as_mut() { if !b.spend(1) { break; } }
            // category = filename without .php
            let norm = path.replace('\\', "/");
            let file = norm.rsplit('/').next().unwrap_or(&norm);
            let category = file.trim_end_matches(".php");
            if category.is_empty() || category == "index" {
                continue;
            }
            // skip pure language dirs mistaken as category if path is messages/uk/app.php → app
            let keys = parse_message_file(&text);
            if keys.is_empty() {
                continue;
            }
            catalog.entry(category.to_owned()).or_default().extend(keys);
        }
    }
    catalog
}

/// (arg_index, range) inside Yii::t( category, message, … )
fn i18n_context(text: &str, offset: usize) -> Option<(usize, Range)> {
    let before = &text[..offset.min(text.len())];
    let i = before.rfind("Yii::t(")?;
    let after = &text[i + 7..];
    let mut depth = 1i32;
    let mut arg = 0usize;
    let mut in_str = None::<u8>;
    let mut lit_start = 0usize;
    let bytes = after.as_bytes();
    let mut k = 0;
    while k < bytes.len() && depth > 0 {
        match bytes[k] {
            b'\'' | b'"' if in_str.is_none() => {
                in_str = Some(bytes[k]);
                lit_start = i + 7 + k + 1;
                k += 1;
            }
            q if in_str == Some(q) => {
                let lit_end = i + 7 + k;
                if offset >= lit_start && offset <= lit_end + 1 {
                    return Some((arg, range_of(text, lit_start, lit_end)));
                }
                in_str = None;
                k += 1;
            }
            b'\\' if in_str.is_some() => k += 2,
            b'(' if in_str.is_none() => {
                depth += 1;
                k += 1;
            }
            b')' if in_str.is_none() => {
                depth -= 1;
                k += 1;
            }
            b',' if in_str.is_none() && depth == 1 => {
                arg += 1;
                k += 1;
            }
            _ => k += 1,
        }
    }
    None
}

/// Category string of the Yii::t call that contains offset (arg 0).
fn i18n_category_at(text: &str, offset: usize) -> Option<String> {
    let before = &text[..offset.min(text.len())];
    let i = before.rfind("Yii::t(")?;
    let after = &text[i + 7..];
    let mut j = 0;
    while j < after.len() && after.as_bytes()[j].is_ascii_whitespace() {
        j += 1;
    }
    let q = *after.as_bytes().get(j)?;
    if q != b'\'' && q != b'"' {
        return None;
    }
    j += 1;
    let start = j;
    while j < after.len() && after.as_bytes()[j] != q {
        if after.as_bytes()[j] == b'\\' {
            j += 1;
        }
        j += 1;
    }
    Some(after[start..j].to_owned())
}

// ---------------------------------------------------------------------------
// Routes / Url::to
// ---------------------------------------------------------------------------

/// A resolved route target.
#[derive(Clone, Debug)]
struct RouteInfo {
    /// e.g. "site/index", "blog/post/create"
    route: String,
    controller_path: String,
    /// action method name hint: "actionIndex"
    action: Option<String>,
    /// Known parameter names for this route (from url rules or action signature heuristics).
    params: Vec<String>,
}

/// urlManager rule with extracted parameter placeholders.
#[derive(Clone, Debug)]
struct UrlRule {
    /// Original pattern, e.g. `post/<id:\d+>` or `posts`
    pattern: String,
    /// Target route, e.g. `post/view`
    route: String,
    /// Parameter names from `<name:regex>` or `<name>`
    params: Vec<String>,
}

/// Extract `<name:…>` / `<name>` placeholders from a Yii url rule pattern or target.
fn rule_params(pattern: &str) -> Vec<String> {
    let mut params = Vec::new();
    let bytes = pattern.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'<' {
            i += 1;
            let start = i;
            while i < bytes.len() && bytes[i] != b':' && bytes[i] != b'>' {
                i += 1;
            }
            if i > start {
                let name = &pattern[start..i];
                // skip Yii specials like <controller>, <action> as "params" for Url::to arrays
                // but still useful for matching; callers may filter
                if name.chars().all(|c| is_ident(c)) {
                    params.push(name.to_owned());
                }
            }
            while i < bytes.len() && bytes[i] != b'>' {
                i += 1;
            }
        }
        i += 1;
    }
    params
}

/// Params suitable for Url::to array keys (exclude Yii structural tokens).
fn user_params(params: &[String]) -> Vec<String> {
    params
        .iter()
        .filter(|p| {
            !matches!(
                p.as_str(),
                "controller" | "action" | "module" | "prefix" | "language" | "lang"
            )
        })
        .cloned()
        .collect()
}

/// Does `path` match Yii rule `pattern`?
/// Supports `<name:regex>` segments; optional trailing `/` ignored.
fn pattern_matches(pattern: &str, path: &str) -> bool {
    let path = path.trim().trim_start_matches('/').trim_end_matches('/');
    let pattern = pattern.trim().trim_start_matches('/').trim_end_matches('/');
    let p_parts: Vec<&str> = if pattern.is_empty() {
        Vec::new()
    } else {
        pattern.split('/').collect()
    };
    let t_parts: Vec<&str> = if path.is_empty() {
        Vec::new()
    } else {
        path.split('/').collect()
    };
    if p_parts.len() != t_parts.len() {
        return false;
    }
    for (p, t) in p_parts.iter().zip(t_parts.iter()) {
        if p.starts_with('<') && p.ends_with('>') {
            if t.is_empty() {
                return false;
            }
            continue;
        }
        if *p != *t {
            return false;
        }
    }
    true
}

/// Bind pattern params from a concrete path: `post/<id:\d+>` + `post/42` → [("id","42")].
fn bind_pattern(pattern: &str, path: &str) -> Option<Vec<(String, String)>> {
    if !pattern_matches(pattern, path) {
        return None;
    }
    let path = path.trim().trim_start_matches('/').trim_end_matches('/');
    let pattern = pattern.trim().trim_start_matches('/').trim_end_matches('/');
    let p_parts: Vec<&str> = pattern.split('/').collect();
    let t_parts: Vec<&str> = path.split('/').collect();
    let mut out = Vec::new();
    for (p, t) in p_parts.iter().zip(t_parts.iter()) {
        if p.starts_with('<') && p.ends_with('>') {
            let inner = &p[1..p.len() - 1];
            let name = inner.split(':').next().unwrap_or(inner);
            if name.chars().all(|c| is_ident(c)) {
                out.push((name.to_owned(), (*t).to_owned()));
            }
        }
    }
    Some(out)
}

/// Expand a rule target that may contain `<id>` refs from the pattern.
/// e.g. pattern `post/<id:\d+>`, route `post/view` → still `post/view`.
/// e.g. `<controller>/<action>` → not expanded here (dynamic).
fn is_dynamic_route_target(route: &str) -> bool {
    route.contains('<')
}

/// Cursor inside a route *string*, a *param name* key, or between params in a route array.
#[derive(Clone, Debug)]
enum RouteCursor {
    /// Inside `'site/index'` / `"post/view"`
    RouteName { name: String, range: Range },
    /// Inside `'id' =>` of Url::to(['post/view', 'id' => $x])
    ParamName {
        route: String,
        /// Partial text of the param key being typed
        param: String,
        range: Range,
        /// Param keys already present in this array (excluding current)
        used: Vec<String>,
    },
    /// After a comma inside route array — expecting next `'param' =>`
    ExpectParam {
        route: String,
        used: Vec<String>,
    },
}

fn string_at(text: &str, offset: usize) -> Option<(usize, usize, String)> {
    let bytes = text.as_bytes();
    if offset > bytes.len() {
        return None;
    }
    let mut i = offset.min(bytes.len());
    if i > 0 {
        i -= 1;
    }
    let mut open = None;
    while i > 0 {
        let b = bytes[i];
        if b == b'\'' || b == b'"' {
            let mut esc = 0;
            let mut k = i;
            while k > 0 && bytes[k - 1] == b'\\' {
                esc += 1;
                k -= 1;
            }
            if esc % 2 == 0 {
                open = Some((i, b));
                break;
            }
        }
        if b == b'\n' && offset.saturating_sub(i) > 240 {
            break;
        }
        i -= 1;
    }
    let (start, quote) = open?;
    let mut j = start + 1;
    while j < bytes.len() && bytes[j] != quote {
        if bytes[j] == b'\\' {
            j += 1;
        }
        j += 1;
    }
    let lit_start = start + 1;
    let lit_end = j;
    if offset < lit_start || offset > lit_end + 1 {
        return None;
    }
    Some((lit_start, lit_end, text[lit_start..lit_end].to_owned()))
}

fn near_route_helper(before: &str) -> bool {
    const HELPERS: &[&str] = &[
        "Url::to(",
        "Url::toRoute(",
        "Url::current(",
        "->redirect(",
        "Html::a(",
        "Html::submitButton(",
        "'route' =>",
        "\"route\" =>",
    ];
    HELPERS
        .iter()
        .any(|h| before.rfind(h).is_some_and(|p| before.len() - p < 160))
}

/// Open `[` of the route array containing `offset`, if any.
fn route_array_start(text: &str, offset: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut i = offset.min(bytes.len());
    let mut depth = 0i32;
    while i > 0 {
        i -= 1;
        match bytes[i] {
            b']' => depth += 1,
            b'[' => {
                if depth == 0 {
                    // Prefer arrays near route helpers
                    let before = &text[..i];
                    if near_route_helper(before)
                        || before.ends_with("to(")
                        || before.ends_with("toRoute(")
                        || before.ends_with("redirect(")
                    {
                        return Some(i);
                    }
                    // still accept if first element looks like a route
                    return Some(i);
                }
                depth -= 1;
            }
            b'\n' if offset.saturating_sub(i) > 400 => break,
            _ => {}
        }
    }
    None
}

/// First string element of array starting at `arr_start` (byte index of `[`).
fn array_first_string(text: &str, arr_start: usize) -> Option<String> {
    let after = &text[arr_start + 1..];
    let mut j = 0;
    let ab = after.as_bytes();
    while j < ab.len() && ab[j].is_ascii_whitespace() {
        j += 1;
    }
    if !matches!(ab.get(j), Some(b'\'' | b'"')) {
        return None;
    }
    let q = ab[j];
    j += 1;
    let start = j;
    while j < ab.len() && ab[j] != q {
        if ab[j] == b'\\' {
            j += 1;
        }
        j += 1;
    }
    Some(after[start..j].to_owned())
}

/// Param keys already written in the route array (`'id' =>`, `"slug" =>`).
fn array_used_params(text: &str, arr_start: usize, offset: usize) -> Vec<String> {
    let end = offset.min(text.len());
    if arr_start + 1 >= end {
        return Vec::new();
    }
    let slice = &text[arr_start + 1..end];
    let bytes = slice.as_bytes();
    let mut used = Vec::new();
    let mut i = 0;
    let mut in_str = None::<u8>;
    let mut last_str: Option<String> = None;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' | b'"' if in_str.is_none() => {
                in_str = Some(bytes[i]);
                i += 1;
                let start = i;
                while i < bytes.len() && bytes[i] != in_str.unwrap_or(0) {
                    if bytes[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
                last_str = Some(slice[start..i].to_owned());
                in_str = None;
                i += 1;
            }
            b'=' if in_str.is_none() && bytes.get(i + 1) == Some(&b'>') => {
                if let Some(key) = last_str.take() {
                    // first string is the route name — skip it as a "param"
                    if used.is_empty() && !key.is_empty() {
                        // if this is the first => in the array, the key is still a param
                        // only skip when key looks like a route (contains /) and is the first entry
                    }
                    if key.contains('/') {
                        // route name used as key? unusual; ignore
                    } else if key.chars().all(|c| is_ident(c) || c == '-') {
                        used.push(key);
                    }
                }
                i += 2;
            }
            _ => i += 1,
        }
    }
    // Drop first used if it equals the route name (shouldn't happen with =>)
    used
}

/// Find the route name (first string element) of the PHP array that contains `offset`.
fn array_route_name(text: &str, offset: usize) -> Option<String> {
    let arr = route_array_start(text, offset)?;
    let name = array_first_string(text, arr)?;
    Some(name.trim_start_matches('/').to_owned())
}

/// Whether cursor is inside a route name string, param key, or expecting a param after comma.
fn route_cursor(text: &str, offset: usize) -> Option<RouteCursor> {
    let arr = route_array_start(text, offset);

    // ExpectParam: cursor not in a string, after `,` inside route array
    if string_at(text, offset).is_none() {
        if let Some(arr_start) = arr {
            if let Some(route) = array_first_string(text, arr_start) {
                let route = route.trim_start_matches('/').to_owned();
                let before = text[..offset.min(text.len())].trim_end();
                let after_comma = before.ends_with(',')
                    || before.ends_with(",\n")
                    || before.chars().rev().skip_while(|c| c.is_whitespace()).next() == Some(',');
                if after_comma
                    && (near_route_helper(&text[..arr_start])
                        || route.contains('/')
                        || route.chars().all(|c| c.is_ascii_lowercase() || c == '-'))
                {
                    let used = array_used_params(text, arr_start, offset);
                    // Don't treat as ExpectParam if still on the route element only
                    // (array is `[ 'route' |]` without comma — after_comma false)
                    return Some(RouteCursor::ExpectParam { route, used });
                }
            }
        }
    }

    let (lit_start, lit_end, name) = string_at(text, offset)?;
    if name.contains("::") || name.contains('\\') || name.contains(' ') {
        return None;
    }
    if name.starts_with("http") || name.ends_with(".php") || name.starts_with('.') {
        return None;
    }
    let before = &text[..lit_start.saturating_sub(1)];
    let range = range_of(text, lit_start, lit_end);

    let looks_param_key = before.ends_with('[')
        || before.ends_with(", ")
        || before.ends_with(",\n")
        || before.ends_with(',')
        || before.ends_with("[\n")
        || before.ends_with("[ ")
        || before.trim_end().ends_with(',');
    let after = &text[lit_end.min(text.len())..];
    let after_trim = after.trim_start_matches(|c: char| c == '\'' || c == '"');
    let after_trim = after_trim.trim_start();
    let followed_by_arrow = after_trim.starts_with("=>");
    // While typing the key before `=>` is written
    let maybe_param =
        looks_param_key && (followed_by_arrow || !name.contains('/')) && arr.is_some();

    if maybe_param {
        if let Some(arr_start) = arr {
            if let Some(route) = array_first_string(text, arr_start) {
                let route = route.trim_start_matches('/').to_owned();
                // First element of array is the route name, not a param
                let first = array_first_string(text, arr_start).unwrap_or_default();
                let is_first = text[arr_start + 1..lit_start]
                    .chars()
                    .all(|c| c.is_whitespace() || c == '\'' || c == '"');
                if !is_first
                    && (near_route_helper(&text[..arr_start])
                        || route.contains('/')
                        || followed_by_arrow)
                {
                    let mut used = array_used_params(text, arr_start, lit_start);
                    used.retain(|u| u != &name);
                    return Some(RouteCursor::ParamName {
                        route,
                        param: name,
                        range,
                        used,
                    });
                }
                let _ = first;
            }
        }
    }

    // Route name string
    let ok_route = name.contains('/')
        || near_route_helper(before)
        || before.ends_with('[')
        || before.ends_with("[ ")
        || before.ends_with("[\n");
    if !ok_route {
        return None;
    }
    if !name.contains('/')
        && !name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c == '-' || c == '_')
    {
        return None;
    }
    Some(RouteCursor::RouteName {
        name: name.trim_start_matches('/').to_owned(),
        range,
    })
}

/// Legacy helper: route name only (for commands / definition).
fn route_context(text: &str, offset: usize) -> Option<(String, Range)> {
    match route_cursor(text, offset)? {
        RouteCursor::RouteName { name, range } => Some((name, range)),
        RouteCursor::ParamName { route, range, .. } => Some((route, range)),
        RouteCursor::ExpectParam { route, .. } => {
            Some((route, range_of(text, offset, offset)))
        }
    }
}

/// Merge param lists, stable unique order.
fn merge_params(base: &[String], extra: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for p in base.iter().chain(extra.iter()) {
        if !out.contains(p) {
            out.push(p.clone());
        }
    }
    out
}

/// Params for a route minus already-used keys.
fn missing_params(all: &[String], used: &[String]) -> Vec<String> {
    all.iter()
        .filter(|p| !used.iter().any(|u| u == *p))
        .cloned()
        .collect()
}

fn action_method_name(action: &str) -> String {
    format!(
        "action{}",
        action
            .split('-')
            .map(|p| {
                let mut ch = p.chars();
                match ch.next() {
                    None => String::new(),
                    Some(f) => f.to_uppercase().collect::<String>() + ch.as_str(),
                }
            })
            .collect::<String>()
    )
}

/// Heuristic params from action method: actionView($id, $slug = null) → [id, slug]
/// Find the line number (0-indexed) of an action method in controller text.
fn find_action_line(text: &str, action_method: &str) -> Option<u32> {
    let needle = format!("function {action_method}");
    let i = text.find(&needle).or_else(|| {
        // public function actionView
        text.find(&format!("function {action_method}("))
    })?;
    // Count newlines up to this position
    let line = text[..i].matches('\n').count() as u32;
    Some(line)
}

/// Heuristic params from action method: actionView($id, $slug = null) → [id, slug]
fn action_params_from_controller_text(text: &str, action_method: &str) -> Vec<String> {
    let needle = format!("function {action_method}");
    let Some(i) = text.find(&needle).or_else(|| {
        // public function actionView
        text.find(&format!("function {action_method}("))
    }) else {
        return Vec::new();
    };
    let after = &text[i..];
    let Some(paren) = after.find('(') else { return Vec::new() };
    let rest = &after[paren + 1..];
    let Some(end) = rest.find(')') else { return Vec::new() };
    let list = &rest[..end];
    let mut params = Vec::new();
    for part in list.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        // skip type hints: ?int $id = 1
        if let Some(dollar) = part.find('$') {
            let name: String = part[dollar + 1..]
                .chars()
                .take_while(|c| is_ident(*c))
                .collect();
            if !name.is_empty() && name != "this" {
                params.push(name);
            }
        }
    }
    params
}

/// Build route catalog from controller index.
fn routes_from_controllers(controllers: &FastMap<String, ControllerInfo>) -> Vec<RouteInfo> {
    let mut routes = Vec::new();
    for c in controllers.values() {
        let base = match &c.module {
            Some(m) => format!("{m}/{}", c.id),
            None => c.id.clone(),
        };
        routes.push(RouteInfo {
            route: format!("{base}/index"),
            controller_path: c.path.clone(),
            action: Some("actionIndex".into()),
            params: Vec::new(),
        });
        routes.push(RouteInfo {
            route: base.clone(),
            controller_path: c.path.clone(),
            action: Some("actionIndex".into()),
            params: Vec::new(),
        });
        for a in &c.actions {
            routes.push(RouteInfo {
                route: format!("{base}/{a}"),
                controller_path: c.path.clone(),
                action: Some(action_method_name(a)),
                params: Vec::new(),
            });
        }
    }
    routes.sort_by(|a, b| a.route.cmp(&b.route));
    routes.dedup_by(|a, b| a.route == b.route);
    routes
}

/// Parse urlManager rules: 'pattern' => 'route' including `<id:\d+>` params.
fn parse_url_rules(text: &str) -> Vec<UrlRule> {
    let mut out = Vec::new();
    let Some(rules_pos) = text.find("'rules'").or_else(|| text.find("\"rules\"")) else {
        return out;
    };
    let after = &text[rules_pos..];
    let Some(bracket) = after.find('[') else { return out };
    let body = &after[bracket + 1..];
    let bytes = body.as_bytes();
    let mut depth = 1i32;
    let mut i = 0;
    let mut in_str = None::<u8>;
    let mut last_str: Option<String> = None;
    while i < bytes.len() && depth > 0 {
        match bytes[i] {
            b'\'' | b'"' if in_str.is_none() => {
                in_str = Some(bytes[i]);
                i += 1;
                let start = i;
                while i < bytes.len() && bytes[i] != in_str.unwrap_or(0) {
                    if bytes[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
                last_str = Some(body[start..i].to_owned());
                in_str = None;
                i += 1;
            }
            b'=' if in_str.is_none() && bytes.get(i + 1) == Some(&b'>') => {
                i += 2;
                while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                    i += 1;
                }
                if matches!(bytes.get(i), Some(b'\'' | b'"')) {
                    let q = bytes[i];
                    i += 1;
                    let start = i;
                    while i < bytes.len() && bytes[i] != q {
                        if bytes[i] == b'\\' {
                            i += 1;
                        }
                        i += 1;
                    }
                    let route = body[start..i].to_owned();
                    if let Some(pat) = last_str.take() {
                        if !pat.is_empty() && !route.is_empty() && !route.contains("::") {
                            let mut params = rule_params(&pat);
                            for p in rule_params(&route) {
                                if !params.contains(&p) {
                                    params.push(p);
                                }
                            }
                            params = user_params(&params);
                            out.push(UrlRule {
                                pattern: pat,
                                route,
                                params,
                            });
                        }
                    }
                }
            }
            b'[' if in_str.is_none() => {
                depth += 1;
                i += 1;
            }
            b']' if in_str.is_none() => {
                depth -= 1;
                i += 1;
            }
            _ => i += 1,
        }
    }
    out
}

fn scan_url_rules(project: &Project) -> Vec<UrlRule> {
    scan_url_rules_with_budget(project, None, None)
}

fn scan_url_rules_with_budget(project: &Project, mut budget: Option<&mut OpBudget>, settings: Option<&PluginSettings>) -> Vec<UrlRule> {
    let mut rules = Vec::new();
    let _max_scan = settings.map(|s| s.performance.max_scan_files).unwrap_or(MAX_SCAN);
    let max_config_files = settings.map(|s| s.performance.max_scan_files).unwrap_or(MAX_CONFIG_FILES);
    // Use configurable config scan paths
    let patterns: Vec<String> = settings
        .map(|s| s.scan_paths.config.clone())
        .unwrap_or_else(|| vec![
            "**/config/main.php".into(),
            "**/config/web.php".into(),
            "**/config/console.php".into(),
            "**/common/config/main.php".into(),
            "**/frontend/config/main.php".into(),
            "**/backend/config/main.php".into(),
            "**/console/config/main.php".into(),
        ]);
    for pat in patterns {
        if budget.as_ref().map_or(false, |b| !b.check()) { break; }
        let Ok(files) = project.find_files(&pat, max_config_files) else { continue };
        if let Some(b) = budget.as_mut() { if !b.spend(2) { break; } }
        for path in files {
            if budget.as_ref().map_or(false, |b| !b.check()) { break; }
            let Ok(text) = project.read_file(&path) else { continue };
            if let Some(b) = budget.as_mut() { if !b.spend(1) { break; } }
            if !text.contains("urlManager") && !text.contains("'rules'") {
                continue;
            }
            rules.extend(parse_url_rules(&text));
        }
    }
    rules
}

/// Params known for a logical route from matching url rules + conventions.
fn params_for_route(route: &str, rules: &[UrlRule]) -> Vec<String> {
    let r = route.trim().trim_start_matches('/');
    let mut params: Vec<String> = Vec::new();
    let push = |params: &mut Vec<String>, p: &str| {
        if !params.iter().any(|x| x == p) {
            params.push(p.to_owned());
        }
    };
    for rule in rules {
        let target = rule.route.trim_start_matches('/');
        let pat = rule.pattern.trim_start_matches('/');
        let hits = target == r
            || pat == r
            || pattern_matches(&rule.pattern, r)
            || (!is_dynamic_route_target(&rule.route) && pattern_matches(target, r));
        if hits {
            for p in user_params(&rule.params) {
                push(&mut params, &p);
            }
        }
    }
    // Conventions for CRUD-ish actions
    let action = r.rsplit('/').next().unwrap_or(r);
    if matches!(action, "view" | "update" | "delete" | "edit" | "remove") {
        push(&mut params, "id");
    }
    if matches!(action, "page" | "index") {
        // no required id
    }
    params
}

/// Resolve route string (or pretty URL) to controller file (+ action + params).
/// Supports:
/// - Internal routes: `site/index`, `module/controller/action`
/// - Absolute routes: `/site/index`, `/module/controller/action` (leading / = from app root)
/// - REST patterns: `['POST', 'post/create']`, `['GET', 'post/<id>']`
fn resolve_route(
    route: &str,
    controllers: &FastMap<String, ControllerInfo>,
    url_rules: &[UrlRule],
) -> Option<RouteInfo> {
    let route = route.trim();
    if route.is_empty() {
        return None;
    }

    // Check if it's a REST pattern: ['HTTP_METHOD', 'route']
    let rest_route = extract_rest_route(route);
    let (route_to_resolve, _http_method) = rest_route.unwrap_or((route.to_owned(), None));

    let mut r = route_to_resolve.trim().trim_start_matches('/').to_owned();
    let mut from_rule_params = Vec::new();
    let is_absolute = route_to_resolve.starts_with('/');

    // 1) exact pattern match or parameterized pattern match
    for rule in url_rules {
        let pat = rule.pattern.trim_start_matches('/');
        if pat == r || pattern_matches(&rule.pattern, &r) {
            if !is_dynamic_route_target(&rule.route) {
                r = rule.route.trim_start_matches('/').to_owned();
                from_rule_params = rule.params.clone();
                break;
            }
        }
        // user may pass the internal route already
        if rule.route.trim_start_matches('/') == r {
            from_rule_params = rule.params.clone();
            break;
        }
    }

    let parts: Vec<&str> = r.split('/').filter(|p| !p.is_empty()).collect();
    if parts.is_empty() {
        return None;
    }

    // For absolute routes (/module/controller/action), try to match module/controller first
    let candidates = if is_absolute && parts.len() >= 2 {
        // Try module/controller/action first, then controller/action
        vec![
            format!("{}/{}", parts[0], parts[1]),
            if parts.len() >= 3 { format!("{}/{}/{}", parts[0], parts[1], parts[2]) } else { String::new() },
            parts[0].to_owned(),
        ].into_iter().filter(|s| !s.is_empty()).collect()
    } else {
        match parts.len() {
            1 => vec![parts[0].to_owned()],
            2 => vec![
                format!("{}/{}", parts[0], parts[1]),
                parts[0].to_owned(),
            ],
            _ => vec![
                format!("{}/{}", parts[0], parts[1]),
                parts[0].to_owned(),
                parts[..parts.len() - 1].join("/"),
            ],
        }
    };
    for key in candidates {
        if let Some(c) = controllers.get(&key) {
            let action = if parts.len() >= 2 {
                let last = parts[parts.len() - 1];
                if last != c.id.as_str() {
                    Some(action_method_name(last))
                } else {
                    Some("actionIndex".into())
                }
            } else {
                Some("actionIndex".into())
            };
            let mut params = from_rule_params.clone();
            if params.is_empty() {
                params = params_for_route(&r, url_rules);
            }
            return Some(RouteInfo {
                route: r,
                controller_path: c.path.clone(),
                action,
                params,
            });
        }
    }
    for p in &parts {
        if let Some(c) = controllers.values().find(|c| c.id == *p) {
            return Some(RouteInfo {
                route: r,
                controller_path: c.path.clone(),
                action: Some("actionIndex".into()),
                params: from_rule_params,
            });
        }
    }
    None
}

/// Extract REST route from patterns like `['POST', 'post/create']` or `['GET', 'post/<id>']`
fn extract_rest_route(route: &str) -> Option<(String, Option<String>)> {
    // Look for ['METHOD', 'route'] pattern
    let bytes = route.as_bytes();
    if bytes.len() < 4 || bytes[0] != b'[' {
        return None;
    }
    let mut i = 1;
    // Skip whitespace
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    // Check for quote
    let q = bytes.get(i)?;
    if !matches!(q, b'\'' | b'"') {
        return None;
    }
    i += 1;
    let method_start = i;
    while i < bytes.len() && bytes[i] != *q {
        i += 1;
    }
    let method = route[method_start..i].to_uppercase();
    i += 1; // closing quote
    // Skip whitespace and comma
    while i < bytes.len() && (bytes[i].is_ascii_whitespace() || bytes[i] == b',') {
        i += 1;
    }
    // Check for second quote
    let q2 = bytes.get(i)?;
    if !matches!(q2, b'\'' | b'"') {
        return None;
    }
    i += 1;
    let route_start = i;
    while i < bytes.len() && bytes[i] != *q2 {
        i += 1;
    }
    let rest_route = route[route_start..i].to_owned();
    Some((rest_route, Some(method)))
}

// ---------------------------------------------------------------------------
// Plugin
// ---------------------------------------------------------------------------


// ---------------------------------------------------------------------------
// Aliases, components, assets, console, migrations, behaviors, widgets, Yii JS
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default)]
struct AliasInfo {
    name: String,   // @app, @web, …
    path: String,   // resolved or declared path fragment
}

#[derive(Clone, Debug, Default)]
struct ComponentInfo {
    id: String,
    class: String,
    /// Config file where the component is declared
    path: Option<String>,
    /// Optional class file resolved in the project
    class_path: Option<String>,
}

#[derive(Clone, Debug, Default)]
struct AssetInfo {
    class: String,
    path: String,
    js: Vec<String>,
    css: Vec<String>,
    depends: Vec<String>,
    source_path: Option<String>,
    base_path: Option<String>,
}

#[derive(Clone, Debug, Default)]
struct ConsoleCommandInfo {
    id: String,          // hello (from HelloController)
    class: String,
    path: String,
    actions: Vec<String>,
}

#[derive(Clone, Debug, Default)]
struct MigrationInfo {
    name: String,
    path: String,
}

#[derive(Clone, Debug, Default)]
struct BehaviorInfo {
    class: String,
    path: String,
}

#[derive(Clone, Debug, Default)]
struct WidgetInfo {
    class: String,
    path: String,
}

/// Built-in application components (Yii2 core).
fn builtin_components() -> Vec<ComponentInfo> {
    const BUILTIN: &[(&str, &str)] = &[
        ("db", "yii\\db\\Connection"),
        ("cache", "yii\\caching\\FileCache"),
        ("mailer", "yii\\symfonymailer\\Mailer"),
        ("user", "yii\\web\\User"),
        ("authManager", "yii\\rbac\\DbManager"),
        ("urlManager", "yii\\web\\UrlManager"),
        ("request", "yii\\web\\Request"),
        ("response", "yii\\web\\Response"),
        ("session", "yii\\web\\Session"),
        ("log", "yii\\log\\Dispatcher"),
        ("i18n", "yii\\i18n\\I18N"),
        ("formatter", "yii\\i18n\\Formatter"),
        ("assetManager", "yii\\web\\AssetManager"),
        ("view", "yii\\web\\View"),
        ("security", "yii\\base\\Security"),
        ("errorHandler", "yii\\web\\ErrorHandler"),
        ("mutex", "yii\\mutex\\FileMutex"),
        ("queue", "yii\\queue\\file\\Queue"),
        ("redis", "yii\\redis\\Connection"),
        ("elasticsearch", "yii\\elasticsearch\\Connection"),
        ("fs", "creocoder\\flysystem\\LocalFilesystem"),
    ];
    BUILTIN
        .iter()
        .map(|(id, class)| ComponentInfo {
            id: (*id).into(),
            class: (*class).into(),
            path: None,
            class_path: None,
        })
        .collect()
}

/// Common methods / properties after Yii::$app->component->
fn component_members(id: &str, class: &str) -> Vec<(&'static str, &'static str)> {
    let mut out: Vec<(&str, &str)> = Vec::new();
    // Shared Component / Object
    out.extend([
        ("__get", "magic"),
        ("hasProperty", "method"),
        ("canGetProperty", "method"),
        ("hasMethod", "method"),
        ("on", "event"),
        ("off", "event"),
        ("trigger", "event"),
    ]);
    let class_l = class.to_ascii_lowercase();
    let id_l = id.to_ascii_lowercase();
    if id_l == "db" || class_l.contains("\\db\\connection") {
        out.extend([
            ("createCommand", "method"),
            ("transaction", "method"),
            ("beginTransaction", "method"),
            ("open", "method"),
            ("close", "method"),
            ("getSchema", "method"),
            ("getQueryBuilder", "method"),
            ("quoteTableName", "method"),
            ("quoteColumnName", "method"),
            ("dsn", "property"),
            ("username", "property"),
            ("password", "property"),
            ("tablePrefix", "property"),
            ("charset", "property"),
            ("enableSchemaCache", "property"),
        ]);
    } else if id_l == "cache" || class_l.contains("cache") {
        out.extend([
            ("get", "method"),
            ("set", "method"),
            ("add", "method"),
            ("delete", "method"),
            ("flush", "method"),
            ("exists", "method"),
            ("multiGet", "method"),
            ("multiSet", "method"),
            ("getOrSet", "method"),
        ]);
    } else if id_l == "user" || class_l.ends_with("\\web\\user") {
        out.extend([
            ("id", "property"),
            ("identity", "property"),
            ("isGuest", "property"),
            ("returnUrl", "property"),
            ("login", "method"),
            ("logout", "method"),
            ("loginByAccessToken", "method"),
            ("can", "method"),
            ("getIdentity", "method"),
            ("setIdentity", "method"),
        ]);
    } else if id_l == "request" {
        out.extend([
            ("get", "method"),
            ("post", "method"),
            ("isGet", "property"),
            ("isPost", "property"),
            ("isAjax", "property"),
            ("isPjax", "property"),
            ("isConsoleRequest", "property"),
            ("url", "property"),
            ("absoluteUrl", "property"),
            ("pathInfo", "property"),
            ("queryString", "property"),
            ("userIP", "property"),
            ("userAgent", "property"),
            ("headers", "property"),
            ("cookies", "property"),
            ("bodyParams", "property"),
            ("queryParams", "property"),
            ("csrfToken", "property"),
            ("getBodyParam", "method"),
            ("getQueryParam", "method"),
            ("getBodyParams", "method"),
        ]);
    } else if id_l == "response" {
        out.extend([
            ("statusCode", "property"),
            ("format", "property"),
            ("data", "property"),
            ("content", "property"),
            ("headers", "property"),
            ("cookies", "property"),
            ("redirect", "method"),
            ("refresh", "method"),
            ("send", "method"),
            ("sendContentAsFile", "method"),
            ("sendFile", "method"),
        ]);
    } else if id_l == "session" {
        out.extend([
            ("get", "method"),
            ("set", "method"),
            ("remove", "method"),
            ("has", "method"),
            ("destroy", "method"),
            ("open", "method"),
            ("close", "method"),
            ("isActive", "property"),
            ("id", "property"),
            ("name", "property"),
        ]);
    } else if id_l == "urlmanager" || class_l.contains("urlmanager") {
        out.extend([
            ("createUrl", "method"),
            ("createAbsoluteUrl", "method"),
            ("parseRequest", "method"),
            ("enablePrettyUrl", "property"),
            ("showScriptName", "property"),
            ("rules", "property"),
            ("baseUrl", "property"),
            ("hostInfo", "property"),
            ("suffix", "property"),
        ]);
    } else if id_l == "mailer" || class_l.contains("mailer") {
        out.extend([
            ("compose", "method"),
            ("send", "method"),
            ("sendMultiple", "method"),
            ("viewPath", "property"),
            ("htmlLayout", "property"),
            ("textLayout", "property"),
        ]);
    } else if id_l == "view" {
        out.extend([
            ("render", "method"),
            ("renderFile", "method"),
            ("renderAjax", "method"),
            ("renderPhpFile", "method"),
            ("registerJs", "method"),
            ("registerJsFile", "method"),
            ("registerCss", "method"),
            ("registerCssFile", "method"),
            ("registerMetaTag", "method"),
            ("registerLinkTag", "method"),
            ("title", "property"),
            ("params", "property"),
            ("assetBundles", "property"),
        ]);
    } else if id_l == "security" {
        out.extend([
            ("generatePasswordHash", "method"),
            ("validatePassword", "method"),
            ("generateRandomString", "method"),
            ("hashData", "method"),
            ("validateData", "method"),
            ("maskToken", "method"),
            ("unmaskToken", "method"),
        ]);
    } else if id_l == "authmanager" || class_l.contains("rbac") {
        out.extend([
            ("checkAccess", "method"),
            ("createRole", "method"),
            ("createPermission", "method"),
            ("add", "method"),
            ("remove", "method"),
            ("getRoles", "method"),
            ("getPermissions", "method"),
            ("getRolesByUser", "method"),
            ("assign", "method"),
            ("revoke", "method"),
            ("getAssignment", "method"),
        ]);
    } else if id_l == "i18n" {
        out.extend([
            ("translate", "method"),
            ("format", "method"),
            ("getMessageSource", "method"),
            ("translations", "property"),
        ]);
    } else if id_l == "formatter" {
        out.extend([
            ("asDate", "method"),
            ("asTime", "method"),
            ("asDatetime", "method"),
            ("asCurrency", "method"),
            ("asDecimal", "method"),
            ("asPercent", "method"),
            ("asRelativeTime", "method"),
            ("asNtext", "method"),
            ("asHtml", "method"),
            ("locale", "property"),
            ("timeZone", "property"),
        ]);
    } else if id_l == "assetmanager" {
        out.extend([
            ("getBundle", "method"),
            ("getAssetUrl", "method"),
            ("publish", "method"),
            ("basePath", "property"),
            ("baseUrl", "property"),
            ("bundles", "property"),
            ("appendTimestamp", "property"),
        ]);
    } else {
        // Generic service hints
        out.extend([("init", "method"), ("bootstrap", "method")]);
    }
    out
}

fn extract_php_string_class(after: &str) -> Option<String> {
    // class' => 'Foo\\Bar'  or "class" => "Foo\\Bar"
    for key in ["'class'", "\"class\""] {
        if let Some(ci) = after.find(key) {
            let rest = &after[ci + key.len()..];
            let eq = rest.find("=>")?;
            let rest = rest[eq + 2..].trim_start();
            let q = rest.chars().next()?;
            if q != '\'' && q != '"' {
                continue;
            }
            let end = rest[1..].find(q)?;
            return Some(rest[1..1 + end].replace("\\\\", "\\"));
        }
    }
    None
}

fn parse_components_from_config(path: &str, text: &str) -> Vec<ComponentInfo> {
    let mut out = Vec::new();
    // Find components arrays: 'components' => [ ... ] or "components" =>
    for key in ["'components'", "\"components\""] {
        let mut from = 0usize;
        while let Some(rel) = text[from..].find(key) {
            let idx = from + rel;
            let slice = &text[idx..];
            // Limit window to avoid scanning entire mega-config repeatedly
            let window_end = slice.len().min(12_000);
            let window = &slice[..window_end];
            let mut search = 0usize;
            while let Some(rel_q) = window[search..].find('\'') {
                let s = search + rel_q + 1;
                let Some(e) = window[s..].find('\'') else { break };
                let id = &window[s..s + e];
                if id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                    && !id.is_empty()
                    && !matches!(id, "class" | "components" | "params" | "id" | "basePath" | "vendorPath" | "aliases" | "modules" | "bootstrap" | "controllerMap")
                {
                    let after = {
                        let from2 = (s + e).min(window.len());
                        let to2 = (from2 + 280).min(window.len());
                        &window[from2..to2]
                    };
                    // Prefer entries that look like component configs (=> [ or => Class)
                    if after.contains("=>") {
                        let class = extract_php_string_class(after).unwrap_or_default();
                        if !out.iter().any(|c: &ComponentInfo| c.id == id) {
                            out.push(ComponentInfo {
                                id: id.to_owned(),
                                class,
                                path: Some(path.to_owned()),
                                class_path: None,
                            });
                        }
                    }
                }
                search = s + e + 1;
                if search > window_end.saturating_sub(1) {
                    break;
                }
            }
            // also double-quoted keys
            search = 0;
            while let Some(rel_q) = window[search..].find('"') {
                let s = search + rel_q + 1;
                let Some(e) = window[s..].find('"') else { break };
                let id = &window[s..s + e];
                if id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                    && !id.is_empty()
                    && id != "class"
                    && id != "components"
                {
                    let after = {
                        let from2 = (s + e).min(window.len());
                        let to2 = (from2 + 280).min(window.len());
                        &window[from2..to2]
                    };
                    if after.contains("=>") {
                        let class = extract_php_string_class(after).unwrap_or_default();
                        if !out.iter().any(|c: &ComponentInfo| c.id == id) {
                            out.push(ComponentInfo {
                                id: id.to_owned(),
                                class,
                                path: Some(path.to_owned()),
                                class_path: None,
                            });
                        }
                    }
                }
                search = s + e + 1;
                if search > window_end.saturating_sub(1) {
                    break;
                }
            }
            from = idx + key.len();
        }
    }
    out
}

fn resolve_class_file(project: &Project, class: &str) -> Option<String> {
    if class.is_empty() {
        return None;
    }
    // App\Components\Foo → **/Components/Foo.php
    let short = class.rsplit('\\').next().unwrap_or(class);
    if short.is_empty() {
        return None;
    }
    if let Ok(files) = project.find_files(&format!("**/{short}.php"), 16) {
        // Prefer path segments matching namespace parts
        let parts: Vec<&str> = class.split('\\').collect();
        for f in &files {
            let norm = f.replace('\\', "/");
            if parts.iter().all(|p| p.is_empty() || norm.contains(*p)) {
                return Some(f.clone());
            }
        }
        return files.into_iter().next();
    }
    None
}

/// Scan Yii config files for aliases and components.
fn scan_config_aliases_components(project: &Project) -> (Vec<AliasInfo>, Vec<ComponentInfo>) {
    scan_config_aliases_components_with_budget(project, None, None)
}

fn scan_config_aliases_components_with_budget(project: &Project, mut budget: Option<&mut OpBudget>, settings: Option<&PluginSettings>) -> (Vec<AliasInfo>, Vec<ComponentInfo>) {
    let mut aliases = Vec::new();
    for (n, p) in [
        ("@app", ""),
        ("@web", "web"),
        ("@webroot", "web"),
        ("@vendor", "vendor"),
        ("@runtime", "runtime"),
        ("@bower", "vendor/bower-asset"),
        ("@npm", "vendor/npm-asset"),
        ("@yii", "vendor/yiisoft/yii2"),
        ("@common", "common"),
        ("@frontend", "frontend"),
        ("@backend", "backend"),
        ("@console", "console"),
    ] {
        aliases.push(AliasInfo { name: n.into(), path: p.into() });
    }
    // Add custom aliases from settings
    if let Some(s) = settings {
        for (name, path) in &s.advanced.custom_aliases {
            aliases.push(AliasInfo { name: name.clone(), path: path.clone() });
        }
    }

    let mut by_id: FastMap<String, ComponentInfo> = FastMap::default();
    for c in builtin_components() {
        by_id.insert(c.id.clone(), c);
    }

    // Prefer real app configs; avoid scanning every PHP under config/.
    let max_scan = settings.map(|s| s.performance.max_scan_files).unwrap_or(MAX_SCAN);
    let max_config_files = settings.map(|s| s.performance.max_scan_files).unwrap_or(MAX_CONFIG_FILES);
    // Use configurable config scan paths
    let patterns: Vec<String> = settings
        .map(|s| s.scan_paths.config.clone())
        .unwrap_or_else(|| vec![
            "**/config/main.php".into(),
            "**/config/web.php".into(),
            "**/config/console.php".into(),
            "**/config/main-local.php".into(),
            "**/config/web-local.php".into(),
            "**/common/config/main.php".into(),
            "**/common/config/main-local.php".into(),
            "**/frontend/config/main.php".into(),
            "**/backend/config/main.php".into(),
            "**/console/config/main.php".into(),
        ]);
    let mut seen = FastSet::default();
    let max_config_files_usize = max_config_files as usize;
    for pat in patterns {
        if let Some(b) = budget.as_ref() { if !b.check() { break; } }
        if seen.len() >= max_config_files_usize {
            break;
        }
        let Ok(files) = project.find_files(&pat, max_scan) else { continue };
        if let Some(b) = budget.as_mut() { if !b.spend(2) { break; } }
        for path in files {
            if let Some(b) = budget.as_ref() { if !b.check() { break; } }
            if seen.len() >= max_config_files_usize {
                break;
            }
            if !seen.insert(path.clone()) {
                continue;
            }
            let Ok(text) = project.read_file(&path) else { continue };
            if file_too_big(text.len()) {
                continue;
            }
            if let Some(b) = budget.as_mut() { if !b.spend(1) { break; } }
            // aliases
            for key in ["'aliases'", "\"aliases\""] {
                if let Some(idx) = text.find(key) {
                    let end = (idx + 1200).min(text.len());
                    let slice = &text[idx..end];
                    let mut search = 0;
                    while let Some(rel) = slice[search..].find("'@") {
                        let start = search + rel + 1;
                        let Some(trans) = slice[start..].find('\'') else { break };
                        if trans == 0 {
                            break;
                        }
                        let name = slice[start..start + trans].to_owned();
                        if name.starts_with('@') {
                            aliases.push(AliasInfo {
                                name: name.clone(),
                                path: String::new(),
                            });
                        }
                        search = start + trans + 1;
                    }
                }
            }
            for c in parse_components_from_config(&path, &text) {
                // Config overrides builtins (same id)
                by_id.insert(c.id.clone(), c);
            }
        }
    }

    // Class-file resolution is deferred to Go-to-Definition.
    let components: Vec<ComponentInfo> = by_id.into_values().collect();

    let mut amap = FastMap::default();
    for a in aliases {
        amap.entry(a.name.clone()).or_insert(a);
    }
    (amap.into_values().collect(), components)
}

fn parse_asset_bundle(path: &str, text: &str) -> Option<AssetInfo> {
    if !text.contains("AssetBundle") && !text.contains("yii\\web\\AssetBundle") {
        return None;
    }
    let class_idx = text.find("class ")?;
    let after = text[class_idx + 6..].trim_start();
    let class: String = after.chars().take_while(|c| is_ident(*c)).collect();
    if class.is_empty() { return None; }

    let extract_array = |prop: &str| -> Vec<String> {
        let needle = format!("${prop}");
        let mut out = Vec::new();
        let mut search = 0usize;
        let mut found = None;
        while let Some(rel) = text[search..].find(&needle) {
            let abs = search + rel;
            let after = text.as_bytes().get(abs + needle.len()).copied().unwrap_or(0);
            let before = if abs == 0 { b' ' } else { text.as_bytes()[abs - 1] };
            if !is_ident(before as char) && !is_ident(after as char) {
                found = Some(abs);
                break;
            }
            search = abs + needle.len();
        }
        let Some(p) = found else { return out };
        let rest = &text[p..];
        let Some(ob) = rest.find('[') else { return out };
        let bytes = rest.as_bytes();
        let mut i = ob + 1;
        let mut depth = 1i32;
        let mut in_str: Option<u8> = None;
        while i < bytes.len() && depth > 0 {
            let b = bytes[i];
            if let Some(q) = in_str {
                if b == b'\\' {
                    i += 2;
                    continue;
                }
                if b == q {
                    in_str = None;
                }
                i += 1;
                continue;
            }
            match b {
                b'\'' | b'"' => {
                    in_str = Some(b);
                    i += 1;
                }
                b'[' => {
                    depth += 1;
                    i += 1;
                }
                b']' => {
                    depth -= 1;
                    i += 1;
                }
                _ => i += 1,
            }
        }
        let end_body = i.saturating_sub(1).min(rest.len());
        let body = &rest[ob + 1..end_body];
        let bb = body.as_bytes();
        let mut k = 0usize;
        let mut d = 0i32;
        let mut ins: Option<u8> = None;
        while k < bb.len() {
            let b = bb[k];
            if let Some(q) = ins {
                if b == b'\\' {
                    k += 2;
                    continue;
                }
                if b == q {
                    ins = None;
                }
                k += 1;
                continue;
            }
            match b {
                b'\'' | b'"' if d == 0 => {
                    let q = b;
                    k += 1;
                    let s = k;
                    while k < bb.len() {
                        if bb[k] == b'\\' {
                            k += 2;
                            continue;
                        }
                        if bb[k] == q {
                            break;
                        }
                        k += 1;
                    }
                    let val = body[s..k.min(body.len())].to_owned();
                    if !val.is_empty() {
                        out.push(val);
                    }
                    if k < bb.len() {
                        k += 1;
                    }
                }
                b'[' => {
                    d += 1;
                    k += 1;
                }
                b']' => {
                    d -= 1;
                    k += 1;
                }
                _ => k += 1,
            }
        }
        out
    };
    let extract_str = |prop: &str| -> Option<String> {
        let needle = format!("${prop}");
        let mut search = 0usize;
        while let Some(rel) = text[search..].find(&needle) {
            let abs = search + rel;
            let after = text.as_bytes().get(abs + needle.len()).copied().unwrap_or(0);
            let before = if abs == 0 { b' ' } else { text.as_bytes()[abs - 1] };
            if !is_ident(before as char) && !is_ident(after as char) {
                let rest = &text[abs..];
                let eq = rest.find('=')?;
                let rest = rest[eq + 1..].trim_start();
                let q = rest.chars().next()?;
                if q != '\'' && q != '"' {
                    return None;
                }
                let end = rest[1..].find(q)?;
                return Some(rest[1..1 + end].to_owned());
            }
            search = abs + needle.len();
        }
        None
    };

    Some(AssetInfo {
        class,
        path: path.to_owned(),
        js: extract_array("js"),
        css: extract_array("css"),
        depends: extract_array("depends"),
        source_path: extract_str("sourcePath"),
        base_path: extract_str("basePath"),
    })
}

fn list_assets(project: &Project) -> FastMap<String, AssetInfo> {
    list_assets_with_budget(project, None, None)
}

fn list_assets_with_budget(project: &Project, mut budget: Option<&mut OpBudget>, settings: Option<&PluginSettings>) -> FastMap<String, AssetInfo> {
    let mut map = FastMap::default();
    let max_scan = settings.map(|s| s.performance.max_scan_files).unwrap_or(MAX_SCAN);
    let max_asset_scan = settings.map(|s| s.performance.max_scan_files.min(15)).unwrap_or(MAX_ASSET_SCAN);
    let max_file_size = settings.map(|s| s.performance.max_file_size_kb * 1024).unwrap_or(MAX_FILE_BYTES);
    // Use configurable asset scan paths
    let patterns: Vec<String> = settings
        .map(|s| s.scan_paths.assets.iter().map(|p| format!("{}/**/*Asset.php", p.trim_end_matches('/'))).collect())
        .unwrap_or_else(|| vec!["**/assets/**/*Asset.php".into(), "**/Assets/**/*Asset.php".into()]);
    let mut seen = FastSet::default();
    for pat in patterns {
        if let Some(b) = budget.as_ref() { if !b.check() { break; } }
        let Ok(files) = project.find_files(&pat, max_scan) else { continue };
        if let Some(b) = budget.as_mut() { if !b.spend(2) { break; } }
        for path in files {
            if let Some(b) = budget.as_ref() { if !b.check() { break; } }
            if seen.len() >= max_asset_scan as usize {
                break;
            }
            if !seen.insert(path.clone()) { continue; }
            let Ok(text) = project.read_file(&path) else { continue };
            if text.len() > max_file_size { continue; }
            if let Some(b) = budget.as_mut() { if !b.spend(1) { break; } }
            if let Some(info) = parse_asset_bundle(&path, &text) {
                map.insert(info.class.clone(), info);
            }
        }
    }
    map
}

/// Strip cache-buster / fragment from asset entry: `css/site.css?v=1.0` → `css/site.css`.
fn normalize_asset_entry(rel: &str) -> String {
    let rel = rel.trim().trim_matches(|c| c == '\'' || c == '"');
    let rel = rel.split(['?', '#']).next().unwrap_or(rel).trim();
    rel.trim_start_matches('/').replace('\\', "/").to_owned()
}

fn collapse_slashes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_slash = false;
    for ch in s.replace('\\', "/").chars() {
        if ch == '/' {
            if !prev_slash {
                out.push('/');
            }
            prev_slash = true;
        } else {
            prev_slash = false;
            out.push(ch);
        }
    }
    out
}

/// Expand Yii path aliases used in AssetBundle `$basePath` / `$sourcePath`.
fn expand_asset_alias(alias_path: &str) -> Vec<String> {
    let p = collapse_slashes(alias_path.trim());
    let mut out = Vec::new();
    let mut push = |s: &str| {
        let s = s.trim().trim_start_matches('/').trim_end_matches('/');
        if !out.iter().any(|x: &String| x == s) {
            out.push(s.to_owned());
        }
    };
    if let Some(rest) = p.strip_prefix("@webroot/") {
        push(&format!("web/{rest}"));
        push(&format!("frontend/web/{rest}"));
        push(&format!("backend/web/{rest}"));
        push(rest);
        // some apps mount webroot at project root
        push(&format!("public/{rest}"));
    } else if p == "@webroot" {
        push("web");
        push("frontend/web");
        push("backend/web");
        push("public");
    } else if let Some(rest) = p.strip_prefix("@app/") {
        push(rest);
        push(&format!("web/{rest}"));
    } else if p == "@app" {
        // project root as empty marker handled by caller
    } else if let Some(rest) = p.strip_prefix("@web/") {
        push(&format!("web/{rest}"));
        push(&format!("frontend/web/{rest}"));
        push(&format!("backend/web/{rest}"));
        push(rest);
    } else if let Some(rest) = p.strip_prefix("@vendor/") {
        push(&format!("vendor/{rest}"));
    } else if p.starts_with('@') {
        if let Some(slash) = p.find('/') {
            push(&p[slash + 1..]);
        }
    } else {
        push(p.trim_start_matches('/'));
    }
    out
}

/// Return a path form that `read_file` accepts, or None.
fn readable_path(project: &Project, path: &str) -> Option<String> {
    let path = collapse_slashes(path.trim());
    if path.is_empty() {
        return None;
    }
    if project.read_file(&path).is_ok() {
        return Some(path);
    }
    let no_dot = path.trim_start_matches("./");
    if no_dot != path && project.read_file(no_dot).is_ok() {
        return Some(no_dot.to_owned());
    }
    if let Ok(root) = project.root() {
        let root = collapse_slashes(&root);
        if !root.is_empty() {
            if path.starts_with(&root) {
                let rel = path[root.len()..].trim_start_matches('/');
                if !rel.is_empty() && project.read_file(rel).is_ok() {
                    return Some(rel.to_owned());
                }
            } else {
                let abs = collapse_slashes(&format!("{root}/{path}"));
                if project.read_file(&abs).is_ok() {
                    // Prefer relative — host open_file usually uses workspace-relative paths
                    if project.read_file(&path).is_ok() {
                        return Some(path);
                    }
                    return Some(abs);
                }
            }
        }
    }
    None
}

/// Resolve asset-relative path (js/css entry) to a project file when possible.
/// Only returns paths that the host can actually read (avoids "Cannot open file").
fn resolve_asset_file(project: &Project, asset: &AssetInfo, rel: &str) -> Option<String> {
    let rel = normalize_asset_entry(rel);
    if rel.is_empty() {
        return None;
    }
    let base_name = rel.rsplit('/').next().unwrap_or(&rel).to_owned();

    // Preferred relative suffixes (most specific first)
    let mut suffixes: Vec<String> = Vec::new();
    let mut push_suf = |s: String| {
        let s = collapse_slashes(&s).trim_start_matches('/').to_owned();
        if !s.is_empty() && !suffixes.iter().any(|x| x == &s) {
            suffixes.push(s);
        }
    };
    for base_alias in asset.source_path.iter().chain(asset.base_path.iter()) {
        for base in expand_asset_alias(base_alias) {
            if base.is_empty() {
                push_suf(rel.clone());
            } else {
                push_suf(format!("{base}/{rel}"));
            }
        }
        let raw = collapse_slashes(&base_alias.trim().replace('\\', "/"));
        if let Some(slash) = raw.find('/') {
            let rest = raw[slash + 1..].trim_start_matches('/');
            if !rest.is_empty() {
                push_suf(format!("{rest}/{rel}"));
            }
        }
    }
    for prefix in ["web", "frontend/web", "backend/web", "public", "www"] {
        push_suf(format!("{prefix}/{rel}"));
    }
    // near asset PHP class
    if let Some(dir) = asset.path.replace('\\', "/").rsplit_once('/').map(|(d, _)| d) {
        push_suf(format!("{dir}/{rel}"));
        if let Some(parent) = dir.rsplit_once('/').map(|(d, _)| d) {
            push_suf(format!("{parent}/{rel}"));
            push_suf(format!("{parent}/web/{rel}"));
        }
    }
    push_suf(rel.clone());
    push_suf(base_name.clone());

    // 1) find_files with host-canonical paths (preferred for open)
    for suf in &suffixes {
        let patterns = [format!("**/{suf}"), suf.clone()];
        for pat in patterns {
            let Ok(files) = project.find_files(&pat, 40) else { continue };
            // Rank: exact suffix match, then contains basePath hint, then any
            let mut ranked: Vec<(i32, String)> = Vec::new();
            for f in files {
                let norm = collapse_slashes(&f);
                if !norm.ends_with(suf) && !norm.ends_with(&rel) && !norm.ends_with(&base_name) {
                    continue;
                }
                let mut score = 0i32;
                if norm.ends_with(suf) {
                    score += 100;
                } else if norm.ends_with(&rel) {
                    score += 80;
                } else {
                    score += 10;
                }
                for base_alias in asset.source_path.iter().chain(asset.base_path.iter()) {
                    for base in expand_asset_alias(base_alias) {
                        if !base.is_empty() && norm.contains(&base) {
                            score += 50;
                        }
                    }
                }
                ranked.push((score, f));
            }
            ranked.sort_by(|a, b| b.0.cmp(&a.0));
            for (_, f) in ranked {
                if let Some(ok) = readable_path(project, &f) {
                    return Some(ok);
                }
            }
        }
    }

    // 2) Direct candidate probes (relative first)
    for suf in &suffixes {
        if let Some(ok) = readable_path(project, suf) {
            return Some(ok);
        }
    }

    // 3) Basename-only search as last resort
    if !base_name.is_empty() {
        if let Ok(files) = project.find_files(&format!("**/{base_name}"), 40) {
            let mut best: Option<(i32, String)> = None;
            for f in files {
                let norm = collapse_slashes(&f);
                let mut score = 1i32;
                if norm.ends_with(&rel) {
                    score += 50;
                }
                for base_alias in asset.source_path.iter().chain(asset.base_path.iter()) {
                    for base in expand_asset_alias(base_alias) {
                        if !base.is_empty() && norm.contains(&base) {
                            score += 30;
                        }
                    }
                }
                if best.as_ref().map(|(s, _)| score > *s).unwrap_or(true) {
                    if let Some(ok) = readable_path(project, &f) {
                        best = Some((score, ok));
                    }
                }
            }
            if let Some((_, path)) = best {
                return Some(path);
            }
        }
    }
    None
}


fn parse_console_command(path: &str, text: &str) -> Option<ConsoleCommandInfo> {
    let class_idx = text.find("class ")?;
    let after = text[class_idx + 6..].trim_start();
    let class: String = after.chars().take_while(|c| is_ident(*c)).collect();
    if !class.ends_with("Controller") || class == "Controller" {
        return None;
    }
    // console commands live under commands/ or console/controllers
    let norm = path.replace('\\', "/");
    if !(norm.contains("/commands/") || norm.contains("/console/")) {
        // still accept *Controller that extends yii\console\Controller
        if !text.contains("console\\Controller") && !text.contains("yii\\console\\Controller") {
            return None;
        }
    }
    let id = class.trim_end_matches("Controller").to_owned();
    let mut actions = Vec::new();
    let mut search = 0;
    while let Some(rel) = text[search..].find("function action") {
        let start = search + rel + "function action".len();
        let name: String = text[start..].chars().take_while(|c| is_ident(*c)).collect();
        if !name.is_empty() {
            // HelloWorld -> hello-world
            let mut kebab = String::new();
            for (i, ch) in name.chars().enumerate() {
                if ch.is_uppercase() {
                    if i > 0 { kebab.push('-'); }
                    kebab.extend(ch.to_lowercase());
                } else {
                    kebab.push(ch);
                }
            }
            actions.push(kebab);
        }
        search = start + name.len().max(1);
    }
    Some(ConsoleCommandInfo {
        id: {
            let mut kebab = String::new();
            for (i, ch) in id.chars().enumerate() {
                if ch.is_uppercase() {
                    if i > 0 { kebab.push('-'); }
                    kebab.extend(ch.to_lowercase());
                } else {
                    kebab.push(ch);
                }
            }
            kebab
        },
        class,
        path: path.to_owned(),
        actions,
    })
}

fn list_console_commands(project: &Project) -> Vec<ConsoleCommandInfo> {
    list_console_commands_with_budget(project, None, None)
}

fn list_console_commands_with_budget(project: &Project, mut budget: Option<&mut OpBudget>, settings: Option<&PluginSettings>) -> Vec<ConsoleCommandInfo> {
    let mut out = Vec::new();
    let max_scan = settings.map(|s| s.performance.max_scan_files).unwrap_or(MAX_SCAN);
    let max_file_size = settings.map(|s| s.performance.max_file_size_kb * 1024).unwrap_or(MAX_FILE_BYTES);
    // Use configurable command scan paths
    let patterns: Vec<String> = settings
        .map(|s| s.scan_paths.commands.iter().map(|p| format!("{}/**/*Controller.php", p.trim_end_matches('/'))).collect())
        .unwrap_or_else(|| vec![
            "**/commands/**/*Controller.php".into(),
            "**/console/controllers/**/*Controller.php".into(),
            "**/console/commands/**/*Controller.php".into(),
        ]);
    let mut seen = FastSet::default();
    for pat in patterns {
        if let Some(b) = budget.as_ref() { if !b.check() { break; } }
        let Ok(files) = project.find_files(&pat, max_scan) else { continue };
        if let Some(b) = budget.as_mut() { if !b.spend(2) { break; } }
        for path in files {
            if let Some(b) = budget.as_ref() { if !b.check() { break; } }
            if out.len() >= max_scan as usize { return out; }
            if !seen.insert(path.clone()) { continue; }
            let Ok(text) = project.read_file(&path) else { continue };
            if text.len() > max_file_size { continue; }
            if let Some(b) = budget.as_mut() { if !b.spend(1) { break; } }
            if let Some(info) = parse_console_command(&path, &text) {
                out.push(info);
            }
        }
    }
    out
}

fn list_migrations(project: &Project) -> Vec<MigrationInfo> {
    list_migrations_with_budget(project, None, None)
}

fn list_migrations_with_budget(project: &Project, mut budget: Option<&mut OpBudget>, settings: Option<&PluginSettings>) -> Vec<MigrationInfo> {
    let mut out = Vec::new();
    let max_scan = settings.map(|s| s.performance.max_scan_files).unwrap_or(MAX_SCAN);
    // Use configurable migration scan paths
    let patterns: Vec<String> = settings
        .map(|s| s.scan_paths.migrations.iter().map(|p| format!("{}/**/*.php", p.trim_end_matches('/'))).collect())
        .unwrap_or_else(|| vec![
            "**/migrations/m*.php".into(),
            "**/Migrations/m*.php".into(),
            "**/console/migrations/m*.php".into(),
        ]);
    let mut seen = FastSet::default();
    for pat in patterns {
        if let Some(b) = budget.as_ref() { if !b.check() { break; } }
        let Ok(files) = project.find_files(&pat, max_scan) else { continue };
        if let Some(b) = budget.as_mut() { if !b.spend(2) { break; } }
        for path in files {
            if let Some(b) = budget.as_ref() { if !b.check() { break; } }
            if !seen.insert(path.clone()) { continue; }
            let name = path.rsplit('/').next().unwrap_or(&path).trim_end_matches(".php").to_owned();
            out.push(MigrationInfo { name, path });
        }
    }
    out
}

fn parse_behavior_or_widget(path: &str, text: &str, kind: &str) -> Option<(String, String)> {
    let class_idx = text.find("class ")?;
    let after = text[class_idx + 6..].trim_start();
    let class: String = after.chars().take_while(|c| is_ident(*c)).collect();
    if class.is_empty() { return None; }
    let ok = match kind {
        "behavior" => {
            text.contains("Behavior")
                || text.contains("yii\\base\\Behavior")
                || class.ends_with("Behavior")
        }
        "widget" => {
            text.contains("yii\\base\\Widget")
                || text.contains("\\Widget")
                || class.ends_with("Widget")
        }
        _ => false,
    };
    if !ok { return None; }
    Some((class, path.to_owned()))
}

fn list_behaviors(project: &Project) -> Vec<BehaviorInfo> {
    list_behaviors_with_budget(project, None, None)
}

fn list_behaviors_with_budget(project: &Project, mut budget: Option<&mut OpBudget>, settings: Option<&PluginSettings>) -> Vec<BehaviorInfo> {
    let mut out = Vec::new();
    let max_scan = settings.map(|s| s.performance.max_scan_files).unwrap_or(MAX_SCAN);
    let max_file_size = settings.map(|s| s.performance.max_file_size_kb * 1024).unwrap_or(MAX_FILE_BYTES);
    // Use configurable behaviors scan paths
    let patterns: Vec<String> = settings
        .map(|s| s.scan_paths.widgets.iter().map(|p| format!("{}/**/*.php", p.trim_end_matches('/'))).collect()) // behaviors don't have separate config, use widgets
        .unwrap_or_else(|| vec!["**/behaviors/**/*.php".into(), "**/*Behavior.php".into()]);
    let mut seen = FastSet::default();
    for pat in patterns {
        if let Some(b) = budget.as_ref() { if !b.check() { break; } }
        let Ok(files) = project.find_files(&pat, max_scan) else { continue };
        if let Some(b) = budget.as_mut() { if !b.spend(2) { break; } }
        for path in files {
            if let Some(b) = budget.as_ref() { if !b.check() { break; } }
            if !seen.insert(path.clone()) { continue; }
            let Ok(text) = project.read_file(&path) else { continue };
            if text.len() > max_file_size { continue; }
            if let Some(b) = budget.as_mut() { if !b.spend(1) { break; } }
            if let Some((class, p)) = parse_behavior_or_widget(&path, &text, "behavior") {
                out.push(BehaviorInfo { class, path: p });
            }
        }
    }
    out
}

fn list_widgets(project: &Project) -> Vec<WidgetInfo> {
    list_widgets_with_budget(project, None, None)
}

fn list_widgets_with_budget(project: &Project, mut budget: Option<&mut OpBudget>, settings: Option<&PluginSettings>) -> Vec<WidgetInfo> {
    let mut out = Vec::new();
    let max_scan = settings.map(|s| s.performance.max_scan_files).unwrap_or(MAX_SCAN);
    let max_file_size = settings.map(|s| s.performance.max_file_size_kb * 1024).unwrap_or(MAX_FILE_BYTES);
    // Use configurable widget scan paths
    let patterns: Vec<String> = settings
        .map(|s| s.scan_paths.widgets.iter().map(|p| format!("{}/**/*.php", p.trim_end_matches('/'))).collect())
        .unwrap_or_else(|| vec!["**/widgets/**/*.php".into(), "**/*Widget.php".into()]);
    let mut seen = FastSet::default();
    for pat in patterns {
        if let Some(b) = budget.as_ref() { if !b.check() { break; } }
        let Ok(files) = project.find_files(&pat, max_scan) else { continue };
        if let Some(b) = budget.as_mut() { if !b.spend(2) { break; } }
        for path in files {
            if let Some(b) = budget.as_ref() { if !b.check() { break; } }
            if !seen.insert(path.clone()) { continue; }
            let Ok(text) = project.read_file(&path) else { continue };
            if text.len() > max_file_size { continue; }
            if let Some(b) = budget.as_mut() { if !b.spend(1) { break; } }
            if let Some((class, p)) = parse_behavior_or_widget(&path, &text, "widget") {
                out.push(WidgetInfo { class, path: p });
            }
        }
    }
    out
}

/// Yii::$app->xxx or Yii::getAlias('@…') / setAlias context.
/// Cursor on a component id: Yii::$app->db|  /  Yii::$app->get('db'|  /  Yii::$app['db'|
fn app_component_context(text: &str, offset: usize) -> Option<(String, usize, usize)> {
    let before = &text[..offset.min(text.len())];

    // Yii::$app->get('partial
    for m in ["Yii::$app->get('", "\\Yii::$app->get('", "Yii::$app->get(\"", "Yii::$app->getComponent('"] {
        if let Some(i) = before.rfind(m) {
            let start = i + m.len();
            let partial: String = before[start..]
                .chars()
                .take_while(|c| is_ident(*c) || *c == '-' || *c == '_')
                .collect();
            return Some((partial, start, offset));
        }
    }

    // Yii::$app['partial
    for m in ["Yii::$app['", "\\Yii::$app['", "Yii::$app[\""] {
        if let Some(i) = before.rfind(m) {
            let start = i + m.len();
            let partial: String = before[start..]
                .chars()
                .take_while(|c| is_ident(*c) || *c == '-' || *c == '_')
                .collect();
            return Some((partial, start, offset));
        }
    }

    // Yii::$app->partial   (not Yii::$app->db->more)
    for m in ["Yii::$app->", "\\Yii::$app->", "$this->app->"] {
        if let Some(i) = before.rfind(m) {
            let start = i + m.len();
            let rest = &before[start..];
            // If there's already a second -> after the component id, this is member access
            let partial: String = rest.chars().take_while(|c| is_ident(*c)).collect();
            let after_id = &rest[partial.len()..];
            if after_id.starts_with("->") {
                continue; // handled by member context
            }
            // Only id chars or end
            if rest.chars().all(|c| is_ident(c)) || partial.len() == rest.chars().take_while(|c| is_ident(*c)).count() {
                return Some((partial, start, offset));
            }
        }
    }
    None
}

/// Yii::$app->db->partial|  member completion
fn app_component_member_context(text: &str, offset: usize) -> Option<(String /*comp*/, String /*partial*/)> {
    let before = &text[..offset.min(text.len())];
    for m in ["Yii::$app->", "\\Yii::$app->", "$this->app->"] {
        if let Some(i) = before.rfind(m) {
            let rest = &before[i + m.len()..];
            let comp: String = rest.chars().take_while(|c| is_ident(*c)).collect();
            if comp.is_empty() {
                continue;
            }
            let after = &rest[comp.len()..];
            if !after.starts_with("->") {
                continue;
            }
            let partial: String = after[2..].chars().take_while(|c| is_ident(*c)).collect();
            // Ensure we are still on the member identifier
            if after[2..].chars().all(|c| is_ident(c))
                || partial.len() == after[2..].chars().take_while(|c| is_ident(*c)).count()
            {
                return Some((comp, partial));
            }
        }
    }
    None
}

/// Inside config: 'components' => [ 'partial|
fn config_components_key_context(text: &str, offset: usize) -> Option<String> {
    let before = &text[..offset.min(text.len())];
    if !before.contains("'components'") && !before.contains("\"components\"") {
        return None;
    }
    // last open quote for key
    if let Some(q) = before.rfind('\'') {
        let partial = &before[q + 1..];
        if !partial.contains('\'') && !partial.contains('\n') && partial.chars().all(|c| is_ident(c) || c == '_') {
            // crude: must be after components keyword more recently than a closing of unrelated
            if let Some(cpos) = before.rfind("'components'") {
                if cpos < q {
                    return Some(partial.to_owned());
                }
            }
        }
    }
    None
}


fn alias_context(text: &str, offset: usize) -> Option<(String, usize, usize)> {
    let before = &text[..offset.min(text.len())];
    // inside string starting with @
    if let Some(q) = before.rfind('\'') {
        let after_q = &before[q + 1..];
        if after_q.starts_with('@') && !after_q.contains('\'') {
            return Some((after_q.to_owned(), q + 1, offset));
        }
    }
    if let Some(q) = before.rfind('"') {
        let after_q = &before[q + 1..];
        if after_q.starts_with('@') && !after_q.contains('"') {
            return Some((after_q.to_owned(), q + 1, offset));
        }
    }
    // Yii::getAlias('@…
    for m in ["getAlias(", "setAlias(", "Yii::getAlias(", "Yii::setAlias("] {
        if let Some(i) = before.rfind(m) {
            let rest = &before[i + m.len()..];
            if let Some(qpos) = rest.find('\'') {
                let partial = &rest[qpos + 1..];
                if !partial.contains('\'') {
                    return Some((partial.to_owned(), i + m.len() + qpos + 1, offset));
                }
            }
        }
    }
    None
}

/// Cursor on a string inside AssetBundle $js / $css / $depends-related path arrays.
fn asset_file_context(text: &str, offset: usize) -> Option<(String, usize, usize)> {
    let offset = offset.min(text.len());
    // 1) Detect string literal that contains the cursor (or ends at cursor).
    let (lit, lit_start) = string_literal_at(text, offset)?;
    // 2) Ensure we are in $js / $css (or $publishOptions paths) array of an AssetBundle-ish file.
    let before = &text[..lit_start];
    let js_i = before.rfind("$js");
    let css_i = before.rfind("$css");
    let prop_i = match (js_i, css_i) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        _ => {
            // Fallback: file looks like AssetBundle and literal looks like asset path
            if text.contains("AssetBundle")
                && (lit.contains(".css")
                    || lit.contains(".js")
                    || lit.contains(".scss")
                    || lit.contains(".less")
                    || lit.contains('/'))
            {
                return Some((lit, lit_start, offset));
            }
            None
        }
    }?;
    let window = &before[prop_i..];
    if !window.contains('[') {
        // still allow AssetBundle fallback
        if text.contains("AssetBundle") {
            return Some((lit, lit_start, offset));
        }
        return None;
    }
    Some((lit, lit_start, offset))
}

/// Return (literal content without quotes, start byte of content) if offset is inside a '…' or "…" string.
fn string_literal_at(text: &str, offset: usize) -> Option<(String, usize)> {
    let bytes = text.as_bytes();
    let offset = offset.min(bytes.len());
    // Scan backward for opening quote not escaped.
    let mut i = offset;
    // If cursor is on closing quote, step back into the string
    if i > 0 && (bytes[i - 1] == b'\'' || bytes[i - 1] == b'"') {
        // could be closing quote — check
    }
    while i > 0 {
        i -= 1;
        let b = bytes[i];
        if b == b'\n' {
            return None;
        }
        if b == b'\'' || b == b'"' {
            // count preceding backslashes
            let mut bs = 0;
            let mut j = i;
            while j > 0 && bytes[j - 1] == b'\\' {
                bs += 1;
                j -= 1;
            }
            if bs % 2 == 1 {
                continue; // escaped quote
            }
            let q = b;
            let content_start = i + 1;
            // find end
            let mut k = content_start;
            while k < bytes.len() {
                if bytes[k] == b'\\' {
                    k += 2;
                    continue;
                }
                if bytes[k] == q {
                    break;
                }
                if bytes[k] == b'\n' {
                    return None;
                }
                k += 1;
            }
            // cursor must be within [content_start, k] inclusive end (allow on closing quote)
            if offset >= content_start && offset <= k {
                let lit = text[content_start..k.min(text.len())].to_owned();
                return Some((lit, content_start));
            }
            return None;
        }
    }
    None
}

fn behaviors_class_context(text: &str, offset: usize) -> bool {
    let before = &text[..offset.min(text.len())];
    before.rfind("function behaviors").is_some()
        || before.rfind("'class'").is_some() && before.contains("behaviors")
}

fn widget_class_context(text: &str, offset: usize) -> bool {
    let before = &text[..offset.min(text.len())];
    before.contains("::widget(") || before.contains("Widget::widget")
}

// ---- Yii2 JavaScript (ActiveForm etc.) ----

const YII_ACTIVE_FORM_METHODS: &[&str] = &[
    "validate",
    "submitForm",
    "resetForm",
    "updateAttribute",
    "updateInput",
    "updateMessages",
    "removeMessages",
    "find",
    "data",
    "destroy",
];

const YII_ACTIVE_FORM_EVENTS: &[&str] = &[
    "beforeValidate",
    "afterValidate",
    "beforeValidateAttribute",
    "afterValidateAttribute",
    "beforeSubmit",
    "ajaxBeforeSend",
    "ajaxComplete",
    "afterInit",
];

const YII_JS_GLOBALS: &[&str] = &[
    "yii",
    "yii.app",
    "yii.activeForm",
    "yii.validation",
    "yii.captcha",
    "yii.gridView",
    "yii.pjax",
];

fn js_yii_context(text: &str, offset: usize) -> Option<String> {
    let before = &text[..offset.min(text.len())];
    // yii.activeForm.| or $.fn.yiiActiveForm(| or $('#form').yiiActiveForm('
    if let Some(i) = before.rfind("yii.activeForm.") {
        let partial: String = before[i + "yii.activeForm.".len()..]
            .chars()
            .take_while(|c| is_ident(*c))
            .collect();
        return Some(format!("activeForm:{partial}"));
    }
    if let Some(i) = before.rfind("yiiActiveForm(") {
        // method string arg
        let rest = &before[i + "yiiActiveForm(".len()..];
        if let Some(q) = rest.rfind('\'') {
            let partial = &rest[q + 1..];
            if !partial.contains('\'') {
                return Some(format!("activeFormMethod:{partial}"));
            }
        }
        return Some("activeFormMethod:".into());
    }
    if before.ends_with("yii.") || before.ends_with("yii") {
        return Some("yiiRoot:".into());
    }
    if let Some(i) = before.rfind(".on('") {
        let partial = &before[i + 5..];
        if !partial.contains('\'') {
            return Some(format!("event:{partial}"));
        }
    }
    if let Some(i) = before.rfind(".on(\"") {
        let partial = &before[i + 5..];
        if !partial.contains('"') {
            return Some(format!("event:{partial}"));
        }
    }
    None
}


fn complete_yii_js(text: &str, off: usize) -> Vec<CompletionItem> {
    let mut items = Vec::new();
    let before = &text[..off.min(text.len())];

    // yii.activeForm.method
    if let Some(i) = before.rfind("yii.activeForm.") {
        let partial: String = before[i + "yii.activeForm.".len()..]
            .chars()
            .take_while(|c| is_ident(*c))
            .collect();
        for m in YII_ACTIVE_FORM_METHODS {
            if m.starts_with(&partial) || m.contains(&partial) || partial.is_empty() {
                items.push(item(*m, CompletionKind::Method, "yii.activeForm", *m));
            }
        }
        return items;
    }

    // yii.validation.
    if let Some(i) = before.rfind("yii.validation.") {
        let partial: String = before[i + "yii.validation.".len()..]
            .chars()
            .take_while(|c| is_ident(*c))
            .collect();
        for m in [
            "required", "boolean", "string", "number", "range", "regularExpression",
            "email", "url", "trim", "captcha", "compare", "file", "image", "ip",
            "addMessage",
        ] {
            if m.starts_with(&partial) || partial.is_empty() {
                items.push(item(m, CompletionKind::Method, "yii.validation", m));
            }
        }
        return items;
    }

    // yii.gridView.
    if let Some(i) = before.rfind("yii.gridView.") {
        let partial: String = before[i + "yii.gridView.".len()..]
            .chars()
            .take_while(|c| is_ident(*c))
            .collect();
        for m in ["init", "applyFilter", "setSelectionColumn", "getSelectedRows"] {
            if m.starts_with(&partial) || partial.is_empty() {
                items.push(item(m, CompletionKind::Method, "yii.gridView", m));
            }
        }
        return items;
    }

    // $.fn.yiiActiveForm('method'
    if let Some(i) = before.rfind("yiiActiveForm(") {
        let rest = &before[i + "yiiActiveForm(".len()..];
        let partial = if let Some(q) = rest.rfind('\'') {
            let p = &rest[q + 1..];
            if p.contains('\'') { "" } else { p }
        } else if let Some(q) = rest.rfind('"') {
            let p = &rest[q + 1..];
            if p.contains('"') { "" } else { p }
        } else {
            ""
        };
        for m in YII_ACTIVE_FORM_METHODS {
            if m.starts_with(partial) || partial.is_empty() {
                items.push(item(
                    format!("'{m}'"),
                    CompletionKind::Method,
                    "$.fn.yiiActiveForm",
                    format!("'{m}'"),
                ));
            }
        }
        return items;
    }

    // jQuery .on('beforeValidate' …
    for (quote, prefix) in [("'", ".on('"), ("\"", ".on(\"")] {
        if let Some(i) = before.rfind(prefix) {
            let partial = &before[i + prefix.len()..];
            if !partial.contains(quote) {
                let events = [
                    "beforeValidate",
                    "afterValidate",
                    "beforeValidateAttribute",
                    "afterValidateAttribute",
                    "beforeSubmit",
                    "ajaxBeforeSend",
                    "ajaxComplete",
                    "afterInit",
                    "pjax:beforeSend",
                    "pjax:complete",
                    "pjax:success",
                    "pjax:error",
                    "pjax:timeout",
                ];
                for e in events {
                    if e.starts_with(partial) || partial.is_empty() {
                        items.push(item(e, CompletionKind::Event, "Yii/jQuery event", e));
                    }
                }
                return items;
            }
        }
    }

    // yii. / yii
    if before.ends_with("yii.") || before.ends_with("yii") {
        for g in [
            "activeForm",
            "validation",
            "captcha",
            "gridView",
            "pjax",
            "getCsrfToken",
            "getCsrfParam",
            "setCsrfToken",
            "reload",
            "initModule",
        ] {
            items.push(item(
                format!("yii.{g}"),
                CompletionKind::Module,
                "Yii2 JS",
                format!("yii.{g}"),
            ));
        }
        return items;
    }

    // data-yii attributes in template strings (best-effort)
    if before.ends_with("data-yii-") || before.ends_with("\"yii-") || before.ends_with("'yii-") {
        for a in ["data-yii-form", "data-yii-confirm", "data-yii-method", "data-yii-pjax"] {
            items.push(item(a, CompletionKind::Property, "Yii HTML data attr", a));
        }
    }

    items
}

fn is_js_lang(lang: &str) -> bool {
    matches!(lang, "javascript" | "typescript" | "js" | "jsx" | "tsx")
}

#[derive(Default)]
pub struct Yii2Support {
    /// Cheap to clone (Arc); avoid deep-copying indexes on every completion.
    views_cache: FastMap<String, (u64, Arc<Vec<String>>)>,
    models_cache: FastMap<String, (u64, Arc<FastMap<String, ModelInfo>>)>,
    controllers_cache: FastMap<String, (u64, Arc<FastMap<String, ControllerInfo>>)>,
    messages_cache: FastMap<String, (u64, Arc<MessageCatalog>)>,
    url_rules_cache: FastMap<String, (u64, Arc<Vec<UrlRule>>)>,
    aliases_cache: FastMap<String, (u64, Arc<Vec<AliasInfo>>)>,
    components_cache: FastMap<String, (u64, Arc<Vec<ComponentInfo>>)>,
    assets_cache: FastMap<String, (u64, Arc<FastMap<String, AssetInfo>>)>,
    console_cache: FastMap<String, (u64, Arc<Vec<ConsoleCommandInfo>>)>,
    migrations_cache: FastMap<String, (u64, Arc<Vec<MigrationInfo>>)>,
    behaviors_cache: FastMap<String, (u64, Arc<Vec<BehaviorInfo>>)>,
    widgets_cache: FastMap<String, (u64, Arc<Vec<WidgetInfo>>)>,
    /// view key → variables passed via render(..., [keys])
    view_vars_cache: FastMap<String, (u64, Arc<FastMap<String, Vec<ViewVarInfo>>>)>,
    /// Track if first request has been processed; on first request skip all project scans
    /// to avoid timeout, only return local completions.
    initial_scan_done: bool,
}

impl Yii2Support {
    fn views(&mut self, project: &Project, settings: Option<&PluginSettings>) -> Arc<Vec<String>> {
        self.views_with_budget(project, None, settings)
    }

    fn views_with_budget(&mut self, project: &Project, budget: Option<&mut OpBudget>, settings: Option<&PluginSettings>) -> Arc<Vec<String>> {
        let root = project.root().unwrap_or_default();
        let current_version = get_cache_version();
        if let Some((version, list)) = self.views_cache.get(&root) {
            if *version == current_version {
                return Arc::clone(list);
            }
        }
        // Inline budget check to avoid Option allocation
        if let Some(b) = budget.as_ref() { if !b.check() { return Arc::new(Vec::new()); } }
        let list = Arc::new(list_views_with_budget(project, budget, settings));
        self.views_cache.insert(root, (current_version, Arc::clone(&list)));
        list
    }

    fn controllers(&mut self, project: &Project, settings: Option<&PluginSettings>) -> Arc<FastMap<String, ControllerInfo>> {
        self.controllers_with_budget(project, None, settings)
    }

    fn controllers_with_budget(&mut self, project: &Project, mut budget: Option<&mut OpBudget>, settings: Option<&PluginSettings>) -> Arc<FastMap<String, ControllerInfo>> {
        let root = project.root().unwrap_or_default();
        let current_version = get_cache_version();
        if let Some((version, map)) = self.controllers_cache.get(&root) {
            if *version == current_version {
                return Arc::clone(map);
            }
        }
        // Inline budget check to avoid Option allocation
        if let Some(b) = budget.as_ref() { if !b.check() { return Arc::new(FastMap::default()); } }
        let max_scan = settings.map(|s| s.performance.max_scan_files).unwrap_or(MAX_SCAN);
        let max_file_size = settings.map(|s| s.performance.max_file_size_kb * 1024).unwrap_or(MAX_FILE_BYTES);
        let mut map = FastMap::default();
        let mut vmap: FastMap<String, Vec<ViewVarInfo>> = FastMap::default();
        // Use configurable scan paths
        let patterns: Vec<String> = settings
            .map(|s| s.scan_paths.controllers.clone())
            .unwrap_or_else(|| vec!["**/controllers/**/*Controller.php".into(), "**/console/controllers/**/*Controller.php".into(), "**/modules/**/controllers/**/*Controller.php".into()]);
        let patterns_ref: Vec<&str> = patterns.iter().map(|s| s.as_str()).collect();
        let mut seen = FastSet::default();
        let mut ingested = 0u32;
        let ingest = |path: String, text: String, map: &mut FastMap<String, ControllerInfo>, vmap: &mut FastMap<String, Vec<ViewVarInfo>>| {
            if let Some(info) = parse_controller(&path, &text) {
                for (view_lit, vars) in extract_render_calls(&text) {
                    for key in render_view_keys(&view_lit, &info) {
                        let entry = vmap.entry(key).or_default();
                        for v in &vars {
                            if !entry.iter().any(|e| e.name == v.name) {
                                entry.push(v.clone());
                            }
                        }
                    }
                }
                let key = match &info.module {
                    Some(m) => format!("{m}/{}", info.id),
                    None => info.id.clone(),
                };
                map.insert(key, info);
            }
        };
        for pat in &patterns_ref {
            if let Some(b) = budget.as_ref() { if !b.check() { break; } }
            let Ok(files) = project.find_files(pat, max_scan) else { continue };
            if let Some(b) = budget.as_ref() { if !b.check() { break; } }
            if let Some(b) = budget.as_mut() { if !b.spend(2) { break; } }
            for path in files {
                if let Some(b) = budget.as_ref() { if !b.check() { break; } }
                if seen.len() >= max_scan as usize {
                    break;
                }
                if !seen.insert(path.clone()) {
                    continue;
                }
                let Ok(text) = project.read_file(&path) else { continue };
                if text.len() > max_file_size {
                    continue;
                }
                if let Some(b) = budget.as_mut() { if !b.spend(1) { break; } }
                ingest(path, text, &mut map, &mut vmap);
                ingested += 1;
                if ingested >= 30 { break; } // Hard cap on files parsed
            }
            if seen.len() >= max_scan as usize {
                break;
            }
        }
        if map.is_empty() {
            if let Some(b) = budget.as_ref() { if !b.check() {
                self.view_vars_cache.insert(root.clone(), (current_version, Arc::new(vmap)));
                let map = Arc::new(map);
                self.controllers_cache.insert(root, (current_version, Arc::clone(&map)));
                return map;
            } }
            if let Ok(files) = project.find_files("**/*Controller.php", max_scan / 2) {
                if let Some(b) = budget.as_mut() { if !b.spend(2) {
                    self.view_vars_cache.insert(root.clone(), (current_version, Arc::new(vmap)));
                    let map = Arc::new(map);
                    self.controllers_cache.insert(root, (current_version, Arc::clone(&map)));
                    return map;
                } }
                for path in files {
                    if let Some(b) = budget.as_ref() { if !b.check() { break; } }
                    if seen.len() >= max_scan as usize {
                        break;
                    }
                    if !seen.insert(path.clone()) {
                        continue;
                    }
                    let Ok(text) = project.read_file(&path) else { continue };
                    if text.len() > max_file_size {
                        continue;
                    }
                    if let Some(b) = budget.as_mut() { if !b.spend(1) { break; } }
                    ingest(path, text, &mut map, &mut vmap);
                    ingested += 1;
                    if ingested >= 30 { break; }
                }
            }
        }
        self.view_vars_cache.insert(root.clone(), (current_version, Arc::new(vmap)));
        let map = Arc::new(map);
        self.controllers_cache.insert(root, (current_version, Arc::clone(&map)));
        map
    }


    fn view_vars(&mut self, project: &Project, settings: Option<&PluginSettings>) -> Arc<FastMap<String, Vec<ViewVarInfo>>> {
        let root = project.root().unwrap_or_default();
        let current_version = get_cache_version();
        if let Some((version, m)) = self.view_vars_cache.get(&root) {
            if *version == current_version {
                return Arc::clone(m);
            }
        }
        // Ensure controllers() built the index.
        let _ = self.controllers(project, settings);
        self.view_vars_cache
            .get(&root)
            .map(|(_, m)| Arc::clone(m))
            .unwrap_or_else(|| Arc::new(FastMap::default()))
    }

    fn messages(&mut self, project: &Project) -> Arc<MessageCatalog> {
        self.messages_with_budget(project, None, None)
    }

    fn messages_with_budget(&mut self, project: &Project, budget: Option<&mut OpBudget>, settings: Option<&PluginSettings>) -> Arc<MessageCatalog> {
        let root = project.root().unwrap_or_default();
        let current_version = get_cache_version();
        if let Some((version, cat)) = self.messages_cache.get(&root) {
            if *version == current_version {
                return Arc::clone(cat);
            }
        }
        if let Some(b) = budget.as_ref() { if !b.check() { return Arc::new(FastMap::default()); } }
        let cat = Arc::new(scan_messages_with_budget(project, budget, settings));
        self.messages_cache.insert(root, (current_version, Arc::clone(&cat)));
        cat
    }

    fn url_rules(&mut self, project: &Project) -> Arc<Vec<UrlRule>> {
        self.url_rules_with_budget(project, None, None)
    }

    fn url_rules_with_budget(&mut self, project: &Project, budget: Option<&mut OpBudget>, settings: Option<&PluginSettings>) -> Arc<Vec<UrlRule>> {
        let root = project.root().unwrap_or_default();
        let current_version = get_cache_version();
        if let Some((version, rules)) = self.url_rules_cache.get(&root) {
            if *version == current_version {
                return Arc::clone(rules);
            }
        }
        if let Some(b) = budget.as_ref() { if !b.check() { return Arc::new(Vec::new()); } }
        let rules = Arc::new(scan_url_rules_with_budget(project, budget, settings));
        self.url_rules_cache.insert(root, (current_version, Arc::clone(&rules)));
        rules
    }

    /// Enrich routes with params from rules + optional action signature from controller file.
    fn routes_with_params(&mut self, project: &Project) -> Vec<RouteInfo> {
        let mut budget = OpBudget::new();
        let mut routes = self.all_routes_with_budget(project, Some(&mut budget), None);
        let rules = self.url_rules_with_budget(project, Some(&mut budget), None);
        // Cap enrichment: reading every controller is expensive.
        let mut reads = 0u32;
        const MAX_ACTION_READS: u32 = 12; // Reduced from 24
        for r in &mut routes {
            if budget.check() == false { break; }
            let mut params = params_for_route(&r.route, rules.as_ref());
            if params.is_empty() {
                if let Some(action) = &r.action {
                    if reads < MAX_ACTION_READS {
                        if let Ok(text) = project.read_file(&r.controller_path) {
                            reads += 1;
                            if budget.spend(1) {
                                if !file_too_big(text.len()) {
                                    for p in action_params_from_controller_text(&text, action) {
                                        if !params.contains(&p) {
                                            params.push(p);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            r.params = params;
        }
        routes
    }

    fn all_routes(&mut self, project: &Project) -> Vec<RouteInfo> {
        self.all_routes_with_budget(project, None, None)
    }

    fn all_routes_with_budget(&mut self, project: &Project, budget: Option<&mut OpBudget>, settings: Option<&PluginSettings>) -> Vec<RouteInfo> {
        let ctrls = self.controllers_with_budget(project, budget, settings);
        routes_from_controllers(&ctrls)
    }

    /// Build or return cached model index for the project.
    fn models(&mut self, project: &Project, settings: Option<&PluginSettings>) -> Arc<FastMap<String, ModelInfo>> {
        self.models_with_budget(project, None, settings)
    }

    fn models_with_budget(&mut self, project: &Project, mut budget: Option<&mut OpBudget>, settings: Option<&PluginSettings>) -> Arc<FastMap<String, ModelInfo>> {
        let root = project.root().unwrap_or_default();
        let current_version = get_cache_version();
        if let Some((version, map)) = self.models_cache.get(&root) {
            if *version == current_version {
                return Arc::clone(map);
            }
        }
        if let Some(b) = budget.as_ref() { if !b.check() { return Arc::new(FastMap::default()); } }
        let max_scan = settings.map(|s| s.performance.max_scan_files).unwrap_or(MAX_SCAN);
        let max_file_size = settings.map(|s| s.performance.max_file_size_kb * 1024).unwrap_or(MAX_FILE_BYTES);
        let mut map = FastMap::default();
        // Use configurable model scan paths
        let patterns: Vec<String> = settings
            .map(|s| s.scan_paths.models.iter().map(|p| format!("{}/**/*.php", p.trim_end_matches('/'))).collect())
            .unwrap_or_else(|| vec!["**/models/**/*.php".into(), "**/forms/**/*.php".into(), "**/common/models/**/*.php".into()]);
        let mut seen = FastSet::default();
        let mut ingested = 0u32;
        for pat in patterns {
            if let Some(b) = budget.as_ref() { if !b.check() { break; } }
            let Ok(files) = project.find_files(&pat, max_scan) else { continue };
            if let Some(b) = budget.as_ref() { if !b.check() { break; } }
            if let Some(b) = budget.as_mut() { if !b.spend(2) { break; } }
            for path in files {
                if let Some(b) = budget.as_ref() { if !b.check() { break; } }
                if seen.len() >= max_scan as usize {
                    break;
                }
                if !seen.insert(path.clone()) {
                    continue;
                }
                let Ok(text) = project.read_file(&path) else { continue };
                if text.len() > max_file_size {
                    continue;
                }
                if let Some(b) = budget.as_mut() { if !b.spend(1) { break; } }
                if let Some(info) = parse_model(&path, &text) {
                    map.insert(info.class.clone(), info);
                }
                ingested += 1;
                if ingested >= 25 { break; }
            }
            if seen.len() >= max_scan as usize {
                break;
            }
        }
        let map = Arc::new(map);
        let current_version = get_cache_version();
        self.models_cache.insert(root, (current_version, Arc::clone(&map)));
        map
    }

    fn ctx_for_doc(&mut self, project: &Project, path: &str, text: &str, settings: Option<&PluginSettings>) -> Option<ControllerInfo> {
        if let Some(c) = controller_context_from_path(path, text) {
            return Some(c);
        }
        // Try match by class name in file
        if let Some(info) = parse_controller(path, text) {
            return Some(info);
        }
        let _ = self.controllers_with_budget(project, None, settings);
        None
    }

    /// Attributes for a variable in the current document, using type hints + project index.
    fn attributes_for_var(
        &mut self,
        project: &Project,
        doc_text: &str,
        var: &str,
    ) -> Vec<(String /*attr*/, String /*source class*/)> {
        let mut budget = OpBudget::new();
        let models = self.models_with_budget(project, Some(&mut budget), None);
        let mut out = Vec::new();

        // 1. Resolve $var → ClassName from local type info
        if let Some(cls) = resolve_var_to_class(doc_text, var) {
            if let Some(info) = models.get(&cls) {
                for a in &info.attributes {
                    out.push((a.clone(), info.class.clone()));
                }
                if !out.is_empty() {
                    return out;
                }
            }
            // Class known but not in index — still return empty rather than all models
            return out;
        }

        // 2. Heuristic names: $user → User / UserForm / UserModel
        let candidates = [
            var.to_owned(),
            format!("{}Form", capitalize(var)),
            format!("{}Model", capitalize(var)),
            capitalize(var),
        ];
        for c in &candidates {
            if let Some(info) = models.get(c) {
                for a in &info.attributes {
                    out.push((a.clone(), info.class.clone()));
                }
                if !out.is_empty() {
                    return out;
                }
            }
        }

        // 3. Fallback: attributes from current file only (if it is itself a model)
        if let Some(info) = parse_model("", doc_text) {
            for a in info.attributes {
                out.push((a, info.class.clone()));
            }
        }
        out
    }


    fn aliases(&mut self, project: &Project) -> Arc<Vec<AliasInfo>> {
        self.aliases_with_budget(project, None, None)
    }

    fn aliases_with_budget(&mut self, project: &Project, budget: Option<&mut OpBudget>, settings: Option<&PluginSettings>) -> Arc<Vec<AliasInfo>> {
        let root = project.root().unwrap_or_default();
        let current_version = get_cache_version();
        if let Some((version, list)) = self.aliases_cache.get(&root) {
            if *version == current_version {
                return Arc::clone(list);
            }
        }
        if budget.as_ref().map_or(false, |b| !b.check()) {
            return Arc::new(Vec::new());
        }
        let (aliases, components) = scan_config_aliases_components_with_budget(project, budget, settings);
        let aliases = Arc::new(aliases);
        let components = Arc::new(components);
        self.components_cache.insert(root.clone(), (current_version, Arc::clone(&components)));
        self.aliases_cache.insert(root, (current_version, Arc::clone(&aliases)));
        aliases
    }

    fn components(&mut self, project: &Project) -> Arc<Vec<ComponentInfo>> {
        self.components_with_budget(project, None, None)
    }

    fn components_with_budget(&mut self, project: &Project, budget: Option<&mut OpBudget>, settings: Option<&PluginSettings>) -> Arc<Vec<ComponentInfo>> {
        let root = project.root().unwrap_or_default();
        let current_version = get_cache_version();
        if let Some((version, list)) = self.components_cache.get(&root) {
            if *version == current_version {
                return Arc::clone(list);
            }
        }
        if budget.as_ref().map_or(false, |b| !b.check()) {
            return Arc::new(Vec::new());
        }
        let (aliases, components) = scan_config_aliases_components_with_budget(project, budget, settings);
        let aliases = Arc::new(aliases);
        let components = Arc::new(components);
        self.aliases_cache.insert(root.clone(), (current_version, aliases));
        self.components_cache.insert(root, (current_version, Arc::clone(&components)));
        components
    }

    fn assets(&mut self, project: &Project) -> Arc<FastMap<String, AssetInfo>> {
        self.assets_with_budget(project, None, None)
    }

    fn assets_with_budget(&mut self, project: &Project, budget: Option<&mut OpBudget>, settings: Option<&PluginSettings>) -> Arc<FastMap<String, AssetInfo>> {
        let root = project.root().unwrap_or_default();
        let current_version = get_cache_version();
        if let Some((version, map)) = self.assets_cache.get(&root) {
            if *version == current_version {
                return Arc::clone(map);
            }
        }
        if let Some(b) = budget.as_ref() { if !b.check() { return Arc::new(FastMap::default()); } }
        let map = Arc::new(list_assets_with_budget(project, budget, settings));
        self.assets_cache.insert(root, (current_version, Arc::clone(&map)));
        map
    }

    fn console_commands(&mut self, project: &Project) -> Arc<Vec<ConsoleCommandInfo>> {
        self.console_commands_with_budget(project, None, None)
    }

    fn console_commands_with_budget(&mut self, project: &Project, budget: Option<&mut OpBudget>, settings: Option<&PluginSettings>) -> Arc<Vec<ConsoleCommandInfo>> {
        let root = project.root().unwrap_or_default();
        let current_version = get_cache_version();
        if let Some((version, list)) = self.console_cache.get(&root) {
            if *version == current_version {
                return Arc::clone(list);
            }
        }
        if budget.as_ref().map_or(false, |b| !b.check()) {
            return Arc::new(Vec::new());
        }
        let list = Arc::new(list_console_commands_with_budget(project, budget, settings));
        self.console_cache.insert(root, (current_version, Arc::clone(&list)));
        list
    }

    fn migrations(&mut self, project: &Project) -> Arc<Vec<MigrationInfo>> {
        self.migrations_with_budget(project, None, None)
    }

    fn migrations_with_budget(&mut self, project: &Project, budget: Option<&mut OpBudget>, settings: Option<&PluginSettings>) -> Arc<Vec<MigrationInfo>> {
        let root = project.root().unwrap_or_default();
        let current_version = get_cache_version();
        if let Some((version, list)) = self.migrations_cache.get(&root) {
            if *version == current_version {
                return Arc::clone(list);
            }
        }
        if budget.as_ref().map_or(false, |b| !b.check()) {
            return Arc::new(Vec::new());
        }
        let list = Arc::new(list_migrations_with_budget(project, budget, settings));
        self.migrations_cache.insert(root, (current_version, Arc::clone(&list)));
        list
    }

    fn behaviors(&mut self, project: &Project) -> Arc<Vec<BehaviorInfo>> {
        self.behaviors_with_budget(project, None, None)
    }

    fn behaviors_with_budget(&mut self, project: &Project, budget: Option<&mut OpBudget>, settings: Option<&PluginSettings>) -> Arc<Vec<BehaviorInfo>> {
        let root = project.root().unwrap_or_default();
        let current_version = get_cache_version();
        if let Some((version, list)) = self.behaviors_cache.get(&root) {
            if *version == current_version {
                return Arc::clone(list);
            }
        }
        if budget.as_ref().map_or(false, |b| !b.check()) {
            return Arc::new(Vec::new());
        }
        let list = Arc::new(list_behaviors_with_budget(project, budget, settings));
        self.behaviors_cache.insert(root, (current_version, Arc::clone(&list)));
        list
    }

    fn widgets(&mut self, project: &Project, settings: Option<&PluginSettings>) -> Arc<Vec<WidgetInfo>> {
        self.widgets_with_budget(project, None, settings)
    }

    fn widgets_with_budget(&mut self, project: &Project, budget: Option<&mut OpBudget>, settings: Option<&PluginSettings>) -> Arc<Vec<WidgetInfo>> {
        let root = project.root().unwrap_or_default();
        let current_version = get_cache_version();
        if let Some((version, list)) = self.widgets_cache.get(&root) {
            if *version == current_version {
                return Arc::clone(list);
            }
        }
        if budget.as_ref().map_or(false, |b| !b.check()) {
            return Arc::new(Vec::new());
        }
        let list = Arc::new(list_widgets_with_budget(project, budget, settings));
        self.widgets_cache.insert(root, (current_version, Arc::clone(&list)));
        list
    }

    fn create_view_file(&self, project: &Project, view: &str, vars: Option<Vec<ViewVarInfo>>) -> Result<String, Error> {
        let root = project.root()?;
        // Determine the correct view path based on controller context
        let rel = if view.starts_with('/') {
            format!("views/{}.php", view.trim_start_matches('/'))
        } else if view.contains('/') {
            format!("views/{view}.php")
        } else {
            format!("views/{view}.php")
        };
        let full_path = format!("{root}/{rel}");

        // Generate view template with variables
        let mut content = String::from("<?php\n\n");
        content.push_str("/**\n");
        content.push_str(r" * @var \yii\web\View $this
");
        if let Some(ref vars) = vars {
            for v in vars {
                content.push_str(&format!(r" * @var mixed ${}
", v.name));
            }
        }
        content.push_str(" */\n\n");
        content.push_str(r"<?= $this->render('_form', [
");
        if let Some(ref vars) = vars {
            for v in vars {
                content.push_str(&format!(r"    '{}' => ${},
", v.name, v.name));
            }
        }
        content.push_str("]) ?>\n");

        // File creation via editor.write API would be done in the command handler
        // This method returns the intended path and content for now
        Ok(full_path)
    }
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        None => String::new(),
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
    }
}

/// Generate view file content with @var annotations and _form render call
fn generate_view_content(vars: Option<Vec<ViewVarInfo>>) -> Result<String, Error> {
    let mut content = String::from("<?php\n\n");
    content.push_str("/**\n");
    content.push_str(r" * @var \yii\web\View $this
");
    if let Some(ref vars) = vars {
        for v in vars {
            content.push_str(&format!(r" * @var mixed ${}
", v.name));
        }
    }
    content.push_str(" */\n\n");
    content.push_str(r"<?= $this->render('_form', [
");
    if let Some(ref vars) = vars {
        for v in vars {
            content.push_str(&format!(r"    '{}' => ${},
", v.name, v.name));
        }
    }
    content.push_str("]) ?>\n");
    Ok(content)
}

impl Plugin for Yii2Support {
    fn run_command(
        &mut self,
        id: &str,
        ctx: &Invocation,
        _arg: Option<&str>,
    ) -> Result<Option<String>, Error> {
        match id {
            "go-to-view" => {
                let editor = ctx.editor().ok_or_else(|| Error::Invalid("focus a PHP editor".into()))?;
                let path = editor.path();
                let doc = editor.document()?;
                let sel = editor.selections()?.into_iter().next();
                let pos = sel.map(|r| r.start).unwrap_or(Position::new(0, 0));
                let off = cursor_offset(&doc.text, pos);
                let Some((name, _)) = view_context(&doc.text, off) else {
                    return Ok(Some("Cursor is not inside a render() view name.".into()));
                };
                let project = ctx.project().ok_or_else(|| Error::Invalid("open a project".into()))?;
                let settings = load_settings_for_project(&project);
                let cctx = self.ctx_for_doc(&project, &path, &doc.text, Some(&settings));
                if let Some(vpath) = resolve_view_path(&project, &name, cctx.as_ref()) {
                    host::open_location(&vpath, 0, 0)?;
                    Ok(None)
                } else {
                    Ok(Some(format!("View `{name}` not found.")))
                }
            }
            "create-view" => {
                let editor = ctx.editor().ok_or_else(|| Error::Invalid("focus a PHP editor".into()))?;
                let path = editor.path();
                let doc = editor.document()?;
                let sel = editor.selections()?.into_iter().next();
                let pos = sel.map(|r| r.start).unwrap_or(Position::new(0, 0));
                let off = cursor_offset(&doc.text, pos);
                let Some((name, _)) = view_context(&doc.text, off) else {
                    return Ok(Some("Cursor is not inside a render() view name.".into()));
                };
                let project = ctx.project().ok_or_else(|| Error::Invalid("open a project".into()))?;
                let settings = load_settings_for_project(&project);
                let cctx = self.ctx_for_doc(&project, &path, &doc.text, Some(&settings));
                if resolve_view_path(&project, &name, cctx.as_ref()).is_some() {
                    return Ok(Some(format!("View `{name}` already exists.")));
                }
                // Extract variables from the render call
                let vars = extract_render_calls(&doc.text)
                    .into_iter()
                    .find(|(v, _)| {
                        let keys = render_view_keys(v, cctx.as_ref().unwrap_or(&ControllerInfo::default()));
                        keys.iter().any(|k| k == &name || k.ends_with(&format!("/{name}")))
                    })
                    .map(|(_, vars)| vars);
                let vpath = self.create_view_file(&project, &name, vars.clone())?;
                // Generate view content
                let content = generate_view_content(vars)?;
                // Notify user with the path and content (file creation not available via API)
                host::notify(
                    NotificationLevel::Info,
                    "Yii2 Support",
                    &format!("View path: {vpath}\n\nContent:\n{content}\n\nPlease create the file manually."),
                );
                host::open_location(&vpath, 0, 0)?;
                Ok(None)
            }
            "create-view-with-params" => {
                let editor = ctx.editor().ok_or_else(|| Error::Invalid("focus a PHP editor".into()))?;
                let path = editor.path();
                let doc = editor.document()?;
                let sel = editor.selections()?.into_iter().next();
                let pos = sel.map(|r| r.start).unwrap_or(Position::new(0, 0));
                let off = cursor_offset(&doc.text, pos);
                let Some((name, _)) = view_context(&doc.text, off) else {
                    return Ok(Some("Cursor is not inside a render() view name.".into()));
                };
                let project = ctx.project().ok_or_else(|| Error::Invalid("open a project".into()))?;
                let settings = load_settings_for_project(&project);
                let cctx = self.ctx_for_doc(&project, &path, &doc.text, Some(&settings));
                if resolve_view_path(&project, &name, cctx.as_ref()).is_some() {
                    return Ok(Some(format!("View `{name}` already exists.")));
                }
                // Get all variables for this view from the index
                let view_vars = self.view_vars(&project, Some(&settings));
                let keys = view_keys_from_path(&name);
                let mut vars = Vec::new();
                for key in &keys {
                    if let Some(v) = view_vars.get(key) {
                        vars.extend(v.clone());
                    }
                }
                let vpath = self.create_view_file(&project, &name, Some(vars.clone()))?;
                // Generate view content
                let content = generate_view_content(Some(vars))?;
                // Notify user with the path and content (file creation not available via API)
                host::notify(
                    NotificationLevel::Info,
                    "Yii2 Support",
                    &format!("View path: {vpath}\n\nContent:\n{content}\n\nPlease create the file manually."),
                );
                host::open_location(&vpath, 0, 0)?;
                Ok(None)
            }
            "go-to-route" => {
                let editor = ctx.editor().ok_or_else(|| Error::Invalid("focus a PHP editor".into()))?;
                let doc = editor.document()?;
                let sel = editor.selections()?.into_iter().next();
                let pos = sel.map(|r| r.start).unwrap_or(Position::new(0, 0));
                let off = cursor_offset(&doc.text, pos);
                let Some((route, _)) = route_context(&doc.text, off) else {
                    return Ok(Some("Cursor is not inside a route string (Url::to / redirect / …).".into()));
                };
                let project = ctx.project().ok_or_else(|| Error::Invalid("open a project".into()))?;
                let settings = load_settings_for_project(&project);
                let ctrls = self.controllers_with_budget(&project, None, Some(&settings));
                let rules = self.url_rules_with_budget(&project, None, Some(&settings));
                if let Some(info) = resolve_route(&route, &ctrls, &rules) {
                    // Find the exact line of the action method
                    let action_line = info.action.as_ref().and_then(|action_method| {
                        project.read_file(&info.controller_path).ok().and_then(|text| {
                            find_action_line(&text, action_method)
                        })
                    });
                    let line = action_line.unwrap_or(0);
                    host::open_location(&info.controller_path, line, 0)?;
                    Ok(None)
                } else {
                    Ok(Some(format!("Route `{route}` not resolved to a controller.")))
                }
            }
            "yii-migrate" => {
                let project = ctx.project().ok_or_else(|| Error::Invalid("open a project".into()))?;
                let root = project.root()?;
                // Run yii migrate via PHP process
                let output = run_yii_console(&root, &["migrate"])?;
                host::notify(
                    NotificationLevel::Info,
                    "Yii2 Migration",
                    &format!("Migration output:\n{output}"),
                );
                // Invalidate migrations cache after running
                invalidate_all_caches();
                Ok(None)
            }
            "yii-migrate-create" => {
                let project = ctx.project().ok_or_else(|| Error::Invalid("open a project".into()))?;
                let _root = project.root()?;
                // Create a new migration - would need a name from user input
                // For now, just show the command
                host::notify(
                    NotificationLevel::Info,
                    "Yii2 Migration",
                    "To create a migration, run: php yii migrate/create migration_name\n\nThis command requires interactive input for the migration name.",
                );
                Ok(None)
            }
            "invalidate-indexes" => {
                invalidate_all_caches();
                host::notify(
                    NotificationLevel::Info,
                    "Yii2 Support",
                    "All indexes invalidated. They will be rebuilt on next request.",
                );
                Ok(None)
            }
            _ => Err(Error::NotFound(format!("command {id}"))),
        }
    }
}

/// Run a Yii console command via PHP process.
fn run_yii_console(root: &str, args: &[&str]) -> Result<String, Error> {
    use std::process::Command;
    let mut cmd = Command::new("php");
    cmd.arg("yii");
    cmd.args(args);
    cmd.current_dir(root);
    let output = cmd.output().map_err(|e| Error::Invalid(format!("Failed to run yii console: {e}")))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        Err(Error::Invalid(format!(
            "Yii console failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )))
    }
}

impl Providers for Yii2Support {
    fn completion(&mut self, doc: &Document, pos: Position, _trigger: Option<&str>) -> Vec<CompletionItem> {
        // Host budget is tight (~50ms); keep work proportional to local context only.
        let mut budget = OpBudget::new();
        let off = cursor_offset(&doc.text, pos);
        if off > doc.text.len() {
            return Vec::new();
        }

        // Load settings
        let project = project_of(&doc.path);
        let settings = project.as_ref().map(|p| load_settings_for_project(p)).unwrap_or_default();

        // --- Yii2 JavaScript (ActiveForm, validation, gridView, pjax) ---
        if is_js_lang(&doc.language) {
            return complete_yii_js(&doc.text, off);
        }

        if doc.language != "php" {
            return Vec::new();
        }
        // Without a project path many indexes are useless — still allow pure text contexts.
        let mut items = Vec::new();

        // FIRST REQUEST OPTIMIZATION: On the very first completion request, skip ALL
        // project-wide scans (views, controllers, routes, etc.) to stay within the
        // 500ms timeout. Only return local completions that don't require project I/O.
        // Subsequent requests will build caches normally.
        let is_first_request = !self.initial_scan_done;

        // Quick local completions that don't need project scans
        // --- Config array 'class' ---
        let before = &doc.text[..off.min(doc.text.len())];
        if before.ends_with("'class' => ") || before.ends_with("\"class\" => ") {
            for cls in [
                "yii\\db\\Connection",
                "yii\\web\\Request",
                "yii\\web\\Response",
                "yii\\web\\User",
                "yii\\caching\\FileCache",
                "yii\\swiftmailer\\Mailer",
            ] {
                items.push(item(cls, CompletionKind::Class, "Yii component", cls));
            }
            if !items.is_empty() {
                items.truncate(settings.performance.max_completions);
                return items;
            }
        }

        // --- Variables passed into this view via render(..., [...]) ---
        if doc.path.contains("/views/") || doc.path.contains("\\views\\") {
            if let Some(partial) = view_dollar_var_context(&doc.text, off) {
                if let Some(project) = project_of(&doc.path) {
                    let idx = self.view_vars(&project, Some(&settings));
                    let keys = view_keys_from_path(&doc.path);
                    let mut seen = FastSet::default();
                    for key in &keys {
                        if let Some(vars) = idx.get(key) {
                            for v in vars {
                                if !(v.name.starts_with(&partial) || partial.is_empty() || v.name.contains(&partial)) {
                                    continue;
                                }
                                if !seen.insert(v.name.clone()) {
                                    continue;
                                }
                                let label = format!("${}", v.name);
                                let detail = if v.source.is_empty() {
                                    format!("view param ({key})")
                                } else {
                                    format!("{} · {}", v.source, key)
                                };
                                push_capped(
                                    &mut items,
                                    item(label.clone(), CompletionKind::Variable, detail, label),
                                );
                            }
                        }
                    }
                    // Also merge keys that end with same basename
                    if let Some(base) = keys.first().and_then(|k| k.rsplit('/').next()) {
                        for (k, vars) in idx.iter() {
                            if k == base || k.ends_with(&format!("/{base}")) {
                                for v in vars {
                                    if !(v.name.starts_with(&partial) || partial.is_empty()) {
                                        continue;
                                    }
                                    if !seen.insert(v.name.clone()) {
                                        continue;
                                    }
                                    let label = format!("${}", v.name);
                                    push_capped(
                                        &mut items,
                                        item(
                                            label.clone(),
                                            CompletionKind::Variable,
                                            format!("view param ({k})"),
                                            label,
                                        ),
                                    );
                                }
                            }
                        }
                    }
                }
                if !items.is_empty() {
                    items.truncate(settings.performance.max_completions);
                    return items;
                }
            }
        }


        // --- Views (scoped by controller/module when known) ---
        if !is_first_request {
            if let Some((partial, _)) = view_context(&doc.text, off) {
                if let Some(project) = project_of(&doc.path) {
                    let cctx = self.ctx_for_doc(&project, &doc.path, &doc.text, Some(&settings));
                    let all = self.views_with_budget(&project, Some(&mut budget), Some(&settings));
                    let ordered = list_views_for_context(&all, cctx.as_ref());
                    let mut seen = FastSet::default();
                    for v in ordered {
                        if budget.check() == false { break; }
                        if !(v.contains(&partial) || partial.is_empty()) {
                            continue;
                        }
                        if !seen.insert(v.clone()) {
                            continue;
                        }
                        let detail = cctx
                            .as_ref()
                            .map(|c| {
                                if let Some(m) = &c.module {
                                    format!("view ({m}/{})", c.view_path)
                                } else {
                                    format!("view ({})", c.view_path)
                                }
                            })
                            .unwrap_or_else(|| "Yii2 view".into());
                        push_capped(&mut items, item(v.clone(), CompletionKind::File, detail, v));
                    }
                }
                if !items.is_empty() {
                    items.truncate(settings.performance.max_completions);
                    return items;
                }
            }
        }

        // --- i18n Yii::t(category, message) ---
        if !is_first_request {
            if let Some((arg, _)) = i18n_context(&doc.text, off) {
                if let Some(project) = project_of(&doc.path) {
                    let catalog = self.messages_with_budget(&project, Some(&mut budget), Some(&settings));
                    if arg == 0 {
                        let mut cats: FastSet<String> = catalog.keys().cloned().collect();
                        for c in ["app", "yii"] {
                            cats.insert(c.into());
                        }
                        for cat in cats {
                            if budget.check() == false { break; }
                            items.push(item(cat.clone(), CompletionKind::Constant, "i18n category", cat));
                        }
                    } else if arg == 1 {
                        if let Some(cat) = i18n_category_at(&doc.text, off) {
                            if let Some(keys) = catalog.get(&cat) {
                                for key in keys {
                                    if budget.check() == false { break; }
                                    push_capped(&mut items, item(
                                        key.clone(),
                                        CompletionKind::Text,
                                        format!("i18n:{cat}"),
                                        key.clone(),
                                    ));
                                }
                            }
                        }
                    }
                }
                if !items.is_empty() {
                    items.truncate(settings.performance.max_completions);
                    return items;
                }
            }
        }

        // --- Form / ActiveRecord attributes ---
        if !is_first_request {
            if let Some(var) = field_model_var(&doc.text, off) {
                if let Some(project) = project_of(&doc.path) {
                    let attrs = self.attributes_for_var(&project, &doc.text, &var);
                    let mut seen = FastSet::default();
                    for (a, cls) in attrs {
                        if seen.insert(a.clone()) {
                            items.push(item(
                                a.clone(),
                                CompletionKind::Property,
                                format!("{cls}::${var}"),
                                a,
                            ));
                        }
                    }
                }
            }
        }

        // --- Routes + params ---
        if !is_first_request {
            if let Some(cursor) = route_cursor(&doc.text, off) {
                if let Some(project) = project_of(&doc.path) {
                    let rules = self.url_rules_with_budget(&project, Some(&mut budget), Some(&settings));
                    let routes = self.routes_with_params(&project);
                    let ctrls = self.controllers_with_budget(&project, Some(&mut budget), Some(&settings));

                    let collect_params = |route: &str| -> Vec<String> {
                        let mut all = params_for_route(route, rules.as_ref());
                        if let Some(info) = routes.iter().find(|r| r.route == route) {
                            all = merge_params(&all, &info.params);
                        }
                        if let Some(info) = resolve_route(route, &ctrls, &rules) {
                            all = merge_params(&all, &info.params);
                        }
                        all
                    };

                    match cursor {
                        RouteCursor::ParamName { route, param, used, .. } => {
                            let all = collect_params(&route);
                            for p in missing_params(&all, &used) {
                                if budget.check() == false { break; }
                                if p.contains(&param) || param.is_empty() {
                                    items.push(item(
                                        p.clone(),
                                        CompletionKind::Property,
                                        format!("param of {route}"),
                                        p,
                                    ));
                                }
                            }
                        }
                        RouteCursor::ExpectParam { route, used } => {
                            let all = collect_params(&route);
                            for p in missing_params(&all, &used) {
                                if budget.check() == false { break; }
                                items.push(item_filter(
                                    p.clone(),
                                    CompletionKind::Property,
                                    format!("param of {route}"),
                                    format!("'{p}' => "),
                                    p,
                                ));
                            }
                        }
                        RouteCursor::RouteName { name: partial, .. } => {
                            let mut seen = FastSet::default();
                            for r in &routes {
                                if budget.check() == false { break; }
                                if r.route.contains(&partial) || partial.is_empty() {
                                    if seen.insert(r.route.clone()) {
                                        let params = collect_params(&r.route);
                                        let detail = if params.is_empty() {
                                            "route".into()
                                        } else {
                                            format!("route · {}", params.join(", "))
                                        };
                                        items.push(item(
                                            r.route.clone(),
                                            CompletionKind::Reference,
                                            detail,
                                            r.route.clone(),
                                        ));
                                    }
                                }
                            }
                            for rule in rules.iter() {
                                if budget.check() == false { break; }
                                if rule.pattern.contains(&partial)
                                    || rule.route.contains(&partial)
                                    || partial.is_empty()
                                {
                                    if seen.insert(rule.pattern.clone()) {
                                        let ps = user_params(&rule.params);
                                        let detail = if ps.is_empty() {
                                            format!("rule → {}", rule.route)
                                        } else {
                                            format!("rule → {} · {}", rule.route, ps.join(", "))
                                        };
                                        items.push(item_filter(
                                            rule.pattern.clone(),
                                            CompletionKind::Reference,
                                            detail,
                                            rule.route.trim_start_matches('/').to_owned(),
                                            rule.pattern.clone(),
                                        ));
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        
        if !items.is_empty() {
            items.truncate(settings.performance.max_completions);
            return items;
        }

        // --- Aliases ---
        if !is_first_request {
            if let Some((partial, _, _)) = alias_context(&doc.text, off) {
                if let Some(project) = project_of(&doc.path) {
                    for a in self.aliases_with_budget(&project, Some(&mut budget), Some(&settings)).iter() {
                        if budget.check() == false { break; }
                        if a.name.contains(&partial) || partial.is_empty() || partial == "@" {
                            items.push(item(
                                a.name.clone(),
                                CompletionKind::Constant,
                                if a.path.is_empty() { "Yii alias".into() } else { format!("alias → {}", a.path) },
                                a.name.clone(),
                            ));
                        }
                    }
                }
            }
        }

        // --- Yii::$app->component / get() / []  and component members ---
        if !is_first_request {
            if let Some((comp, partial)) = app_component_member_context(&doc.text, off) {
                if let Some(project) = project_of(&doc.path) {
                    let comps = self.components_with_budget(&project, Some(&mut budget), Some(&settings));
                    let class = comps
                        .iter()
                        .find(|c| c.id == comp)
                        .map(|c| c.class.as_str())
                        .unwrap_or("");
                    for (name, kind) in component_members(&comp, class) {
                        if budget.check() == false { break; }
                        if name.starts_with(&partial) || partial.is_empty() {
                            let ck = if kind == "property" {
                                CompletionKind::Property
                            } else if kind == "event" {
                                CompletionKind::Event
                            } else {
                                CompletionKind::Method
                            };
                            items.push(item(
                                name,
                                ck,
                                format!("Yii::$app->{comp}"),
                                name,
                            ));
                        }
                    }
                }
            } else if let Some((partial, _, _)) = app_component_context(&doc.text, off) {
                let comps = if let Some(project) = project_of(&doc.path) {
                    self.components_with_budget(&project, Some(&mut budget), Some(&settings))
                } else {
                    Arc::new(builtin_components())
                };
                for c in comps.iter() {
                    if budget.check() == false { break; }
                    if c.id.contains(&partial) || partial.is_empty() || c.id.starts_with(&partial) {
                        let detail = if c.class.is_empty() {
                            "app component".into()
                        } else {
                            c.class.clone()
                        };
                        items.push(item(c.id.clone(), CompletionKind::Property, detail, c.id.clone()));
                    }
                }
            }

            // --- Config: components key completion ---
            if let Some(partial) = config_components_key_context(&doc.text, off) {
                let comps = if let Some(project) = project_of(&doc.path) {
                    self.components_with_budget(&project, Some(&mut budget), Some(&settings))
                } else {
                    Arc::new(builtin_components())
                };
                for c in comps.iter() {
                    if budget.check() == false { break; }
                    if c.id.contains(&partial) || partial.is_empty() || c.id.starts_with(&partial) {
                        items.push(item(
                            c.id.clone(),
                            CompletionKind::Property,
                            if c.class.is_empty() {
                                "component id".into()
                            } else {
                                c.class.clone()
                            },
                            c.id.clone(),
                        ));
                    }
                }
            }
        }

        // --- Asset bundles ($js/$css entries + register) ---
        if !is_first_request {
            if let Some((partial, _, _)) = asset_file_context(&doc.text, off) {
                if let Some(project) = project_of(&doc.path) {
                    let assets = self.assets_with_budget(&project, Some(&mut budget), Some(&settings));
                    // if current file is an asset, complete its known files + path hints
                    if let Some(info) = parse_asset_bundle(&doc.path, &doc.text) {
                        for f in info.js.iter().chain(info.css.iter()) {
                            if budget.check() == false { break; }
                            if f.contains(&partial) || partial.is_empty() {
                                items.push(item(f.clone(), CompletionKind::File, "asset file", f.clone()));
                            }
                        }
                    }
                    for a in assets.values() {
                        if budget.check() == false { break; }
                        for f in a.js.iter().chain(a.css.iter()) {
                            if f.contains(&partial) || partial.is_empty() {
                                items.push(item(
                                    f.clone(),
                                    CompletionKind::File,
                                    format!("{} asset", a.class),
                                    f.clone(),
                                ));
                            }
                        }
                    }
                }
            }
        }

        // --- Behaviors class completion ---
        if !is_first_request {
            if behaviors_class_context(&doc.text, off) {
                if let Some(project) = project_of(&doc.path) {
                    for b in self.behaviors_with_budget(&project, Some(&mut budget), Some(&settings)).iter() {
                        if budget.check() == false { break; }
                        items.push(item(
                            b.class.clone(),
                            CompletionKind::Class,
                            "behavior",
                            b.class.clone(),
                        ));
                    }
                    for b in ["yii\\behaviors\\TimestampBehavior", "yii\\behaviors\\BlameableBehavior", "yii\\behaviors\\SluggableBehavior", "yii\\behaviors\\AttributeTypecastBehavior"] {
                        if budget.check() == false { break; }
                        items.push(item(b, CompletionKind::Class, "Yii behavior", b));
                    }
                }
            }
        }

        // --- Widget class completion ---
        if !is_first_request {
            if widget_class_context(&doc.text, off) {
                if let Some(project) = project_of(&doc.path) {
                    for w in self.widgets_with_budget(&project, Some(&mut budget), Some(&settings)).iter() {
                        if budget.check() == false { break; }
                        items.push(item(w.class.clone(), CompletionKind::Class, "widget", w.class.clone()));
                    }
                }
            }
        }

        // --- Console commands ---
        if !is_first_request {
            if doc.path.contains("/commands/") || doc.path.contains("/console/") {
                if let Some(project) = project_of(&doc.path) {
                    for cmd in self.console_commands_with_budget(&project, Some(&mut budget), Some(&settings)).iter() {
                        if budget.check() == false { break; }
                        items.push(item(
                            cmd.id.clone(),
                            CompletionKind::Function,
                            format!("console {}", cmd.class),
                            cmd.id.clone(),
                        ));
                        for a in &cmd.actions {
                            if budget.check() == false { break; }
                            items.push(item(
                                format!("{}/{}", cmd.id, a),
                                CompletionKind::Method,
                                "console action",
                                format!("{}/{}", cmd.id, a),
                            ));
                        }
                    }
                }
            }
        }

        // --- Migrations (name completion in docs) ---
        if !is_first_request {
            if doc.path.contains("/migrations/") {
                if let Some(project) = project_of(&doc.path) {
                    for m in self.migrations_with_budget(&project, Some(&mut budget), Some(&settings)).iter() {
                        if budget.check() == false { break; }
                        items.push(item(m.name.clone(), CompletionKind::File, "migration", m.name.clone()));
                    }
                }
            }
        }

        if items.len() > MAX_COMPLETION {
            items.truncate(settings.performance.max_completions);
        }
        // Mark that first request is done; subsequent requests will do full project scans.
        self.initial_scan_done = true;
        items
    }

    fn definition(&mut self, doc: &Document, pos: Position) -> Vec<Location> {
        if doc.language != "php" {
            return Vec::new();
        }
        let mut budget = OpBudget::new();
        let off = cursor_offset(&doc.text, pos);
        if off > doc.text.len() || doc.path.is_empty() {
            return Vec::new();
        }
        let Some(project) = project_of(&doc.path) else {
            return Vec::new();
        };

        // Load settings
        let settings = load_settings_for_project(&project);

        if let Some((name, _)) = view_context(&doc.text, off) {
            let cctx = self.ctx_for_doc(&project, &doc.path, &doc.text, Some(&settings));
            if let Some(path) = resolve_view_path(&project, &name, cctx.as_ref()) {
                return vec![Location {
                    path,
                    range: Range::new(Position::new(0, 0), Position::new(0, 0)),
                }];
            }
        }

        if let Some((route, _)) = route_context(&doc.text, off) {
            let ctrls = self.controllers_with_budget(&project, Some(&mut budget), Some(&settings));
            let rules = self.url_rules_with_budget(&project, Some(&mut budget), Some(&settings));
            if let Some(info) = resolve_route(&route, &ctrls, &rules) {
                // Find the exact line of the action method
                let action_line = info.action.as_ref().and_then(|action_method| {
                    project.read_file(&info.controller_path).ok().and_then(|text| {
                        find_action_line(&text, action_method)
                    })
                });
                let line = action_line.unwrap_or(0);
                return vec![Location {
                    path: info.controller_path,
                    range: Range::new(Position::new(line, 0), Position::new(line, 0)),
                }];
            }
        }

        // Asset $js / $css entry → file (strip ?v=, honor basePath/sourcePath)
        if let Some((rel, _, _)) = asset_file_context(&doc.text, off) {
            // Prefer parsing the current file as AssetBundle (has local basePath)
            let local = parse_asset_bundle(&doc.path, &doc.text);
            if let Some(ref info) = local {
                if let Some(path) = resolve_asset_file(&project, info, &rel) {
                    return vec![Location {
                        path,
                        range: Range::new(Position::new(0, 0), Position::new(0, 0)),
                    }];
                }
            }
            // Indexed assets with matching entry
            let nrel = normalize_asset_entry(&rel);
            for a in self.assets_with_budget(&project, Some(&mut budget), Some(&settings)).values() {
                if budget.check() == false { break; }
                if a.js.iter().any(|j| normalize_asset_entry(j) == nrel)
                    || a.css.iter().any(|c| normalize_asset_entry(c) == nrel)
                {
                    if let Some(path) = resolve_asset_file(&project, a, &rel) {
                        return vec![Location {
                            path,
                            range: Range::new(Position::new(0, 0), Position::new(0, 0)),
                        }];
                    }
                }
            }
            // Last resort: bare path search even without AssetBundle parse
            let bare = AssetInfo {
                path: doc.path.clone(),
                base_path: local.as_ref().and_then(|i| i.base_path.clone()),
                source_path: local.as_ref().and_then(|i| i.source_path.clone()),
                ..AssetInfo::default()
            };
            if let Some(path) = resolve_asset_file(&project, &bare, &rel) {
                return vec![Location {
                    path,
                    range: Range::new(Position::new(0, 0), Position::new(0, 0)),
                }];
            }
        }

        // Yii::$app->component → class file, else config declaration
        if let Some((partial, _, _)) = app_component_context(&doc.text, off) {
            for c in self.components_with_budget(&project, Some(&mut budget), Some(&settings)).iter() {
                if budget.check() == false { break; }
                if c.id == partial {
                    if let Some(cp) = &c.class_path {
                        return vec![Location {
                            path: cp.clone(),
                            range: Range::new(Position::new(0, 0), Position::new(0, 0)),
                        }];
                    }
                    if !c.class.is_empty() && !c.class.starts_with("yii\\") {
                        if let Some(cp) = resolve_class_file(&project, &c.class) {
                            return vec![Location {
                                path: cp,
                                range: Range::new(Position::new(0, 0), Position::new(0, 0)),
                            }];
                        }
                    }
                    if let Some(path) = &c.path {
                        return vec![Location {
                            path: path.clone(),
                            range: Range::new(Position::new(0, 0), Position::new(0, 0)),
                        }];
                    }
                }
            }
        }
        // Also when cursor is on full id word after Yii::$app->
        if let Some((comp, _)) = app_component_member_context(&doc.text, off) {
            for c in self.components_with_budget(&project, Some(&mut budget), Some(&settings)).iter() {
                if budget.check() == false { break; }
                if c.id == comp {
                    if let Some(cp) = &c.class_path {
                        return vec![Location {
                            path: cp.clone(),
                            range: Range::new(Position::new(0, 0), Position::new(0, 0)),
                        }];
                    }
                    if let Some(path) = &c.path {
                        return vec![Location {
                            path: path.clone(),
                            range: Range::new(Position::new(0, 0), Position::new(0, 0)),
                        }];
                    }
                }
            }
        }

        // Behavior / Widget class name under cursor → file
        let word = {
            let bytes = doc.text.as_bytes();
            let mut s = off.min(bytes.len());
            let mut e = s;
            while s > 0 && (is_ident(bytes[s - 1] as char) || bytes[s - 1] == b'\\') {
                s -= 1;
            }
            while e < bytes.len() && (is_ident(bytes[e] as char) || bytes[e] == b'\\') {
                e += 1;
            }
            doc.text[s..e].to_owned()
        };
        if !word.is_empty() {
            let short = word.rsplit('\\').next().unwrap_or(&word).to_owned();
            for b in self.behaviors_with_budget(&project, Some(&mut budget), Some(&settings)).iter() {
                if budget.check() == false { break; }
                if b.class == short || b.class.ends_with(&short) {
                    return vec![Location {
                        path: b.path.clone(),
                        range: Range::new(Position::new(0, 0), Position::new(0, 0)),
                    }];
                }
            }
            for w in self.widgets_with_budget(&project, Some(&mut budget), Some(&settings)).iter() {
                if budget.check() == false { break; }
                if w.class == short || w.class.ends_with(&short) {
                    return vec![Location {
                        path: w.path.clone(),
                        range: Range::new(Position::new(0, 0), Position::new(0, 0)),
                    }];
                }
            }
            for a in self.assets_with_budget(&project, Some(&mut budget), Some(&settings)).values() {
                if budget.check() == false { break; }
                if a.class == short || a.class.ends_with(&short) {
                    return vec![Location {
                        path: a.path.clone(),
                        range: Range::new(Position::new(0, 0), Position::new(0, 0)),
                    }];
                }
            }
            for cmd in self.console_commands_with_budget(&project, Some(&mut budget), Some(&settings)).iter() {
                if budget.check() == false { break; }
                if cmd.class == short || cmd.id == word {
                    return vec![Location {
                        path: cmd.path.clone(),
                        range: Range::new(Position::new(0, 0), Position::new(0, 0)),
                    }];
                }
            }
            for m in self.migrations_with_budget(&project, Some(&mut budget), Some(&settings)).iter() {
                if budget.check() == false { break; }
                if m.name == word || m.name == short {
                    return vec![Location {
                        path: m.path.clone(),
                        range: Range::new(Position::new(0, 0), Position::new(0, 0)),
                    }];
                }
            }
        }

        Vec::new()
    }

    fn code_actions(&mut self, doc: &Document, selection: Range) -> Vec<CodeAction> {
        if doc.language != "php" {
            return Vec::new();
        }
        let off = cursor_offset(&doc.text, selection.start);
        let Some((name, _range)) = view_context(&doc.text, off) else {
            return Vec::new();
        };
        let Some(project) = project_of(&doc.path) else {
            return Vec::new();
        };
        let settings = load_settings_for_project(&project);
        let cctx = self.ctx_for_doc(&project, &doc.path, &doc.text, Some(&settings));
        if resolve_view_path(&project, &name, cctx.as_ref()).is_some() {
            return Vec::new();
        }
        vec![CodeAction {
            title: format!("Create view `{name}`"),
            kind: Some("quickfix".into()),
            preferred: true,
            edits: Vec::new(),
            data: Some(name),
        }]
    }
}

// Diagnostics for missing views can be published from events or a command.
// For the MVP we expose the inspection via the create-view command and code action.

register_plugin!(Yii2Support, providers);

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_view_in_render() {
        let text = r#"return $this->render('site/index', ['model' => $model]);"#;
        let off = text.find("site/index").unwrap() + 3;
        let (name, _) = view_context(text, off).expect("view context");
        assert_eq!(name, "site/index");
    }

    #[test]
    fn parse_model_public_and_rules() {
        let text = r#"
        namespace app\models;
        class UserForm extends Model {
            public $username;
            public string $email;
            public function rules() {
                return [
                    [['username', 'email'], 'required'],
                    ['bio', 'string'],
                ];
            }
        }
        "#;
        let info = parse_model("models/UserForm.php", text).expect("model");
        assert_eq!(info.class, "UserForm");
        assert!(info.attributes.contains(&"username".to_string()));
        assert!(info.attributes.contains(&"email".to_string()));
        assert!(info.attributes.contains(&"bio".to_string()));
        assert_eq!(info.fqn.as_deref(), Some("app\\models\\UserForm"));
    }

    #[test]
    fn resolve_var_from_new_and_phpdoc() {
        let text = r#"
        /** @var UserForm $form */
        $form = new UserForm();
        $other = new \app\models\LoginForm();
        "#;
        assert_eq!(resolve_var_to_class(text, "form").as_deref(), Some("UserForm"));
        assert_eq!(resolve_var_to_class(text, "other").as_deref(), Some("LoginForm"));
    }

    #[test]
    fn field_model_var_detects_active_helpers() {
        let text = r#"<?= Html::activeTextInput($model, '|"#;
        let off = text.len();
        assert_eq!(field_model_var(text, off).as_deref(), Some("model"));
    }

    #[test]
    fn parse_controller_and_module() {
        let path = "/app/modules/blog/controllers/PostController.php";
        let text = r#"
        namespace app\modules\blog\controllers;
        class PostController extends Controller {
            public function actionIndex() {}
            public function actionCreate() {}
            public function actionUpdate() {}
        }
        "#;
        let c = parse_controller(path, text).expect("controller");
        assert_eq!(c.id, "post");
        assert_eq!(c.class, "PostController");
        assert_eq!(c.module.as_deref(), Some("blog"));
        assert_eq!(c.view_path, "post");
        assert!(c.actions.contains(&"index".to_string()));
        assert!(c.actions.contains(&"create".to_string()));
    }

    #[test]
    fn parse_message_file_keys() {
        let text = r#"<?php
        return [
            'Hello' => 'Привіт',
            "Save" => "Зберегти",
            'User {name}' => 'Користувач {name}',
        ];
        "#;
        let keys = parse_message_file(text);
        assert!(keys.contains("Hello"));
        assert!(keys.contains("Save"));
        assert!(keys.contains("User {name}"));
    }

    #[test]
    fn controller_context_from_view_path() {
        let path = "/proj/frontend/views/site/index.php";
        let c = controller_context_from_path(path, "").expect("ctx");
        assert_eq!(c.view_path, "site");
    }

    #[test]
    fn route_context_url_to() {
        let text = r#"Url::to(['site/index', 'id' => 1])"#;
        let off = text.find("site/index").unwrap() + 2;
        let (r, _) = route_context(text, off).expect("route");
        assert_eq!(r, "site/index");
    }

    #[test]
    fn routes_from_controller_actions() {
        let mut map = FastMap::default();
        map.insert(
            "site".into(),
            ControllerInfo {
                id: "site".into(),
                class: "SiteController".into(),
                path: "controllers/SiteController.php".into(),
                module: None,
                view_path: "site".into(),
                actions: vec!["index".into(), "about".into(), "contact".into()],
            },
        );
        let routes = routes_from_controllers(&map);
        let names: Vec<_> = routes.iter().map(|r| r.route.as_str()).collect();
        assert!(names.contains(&"site/index"));
        assert!(names.contains(&"site/about"));
        assert!(names.contains(&"site/contact"));
    }

    #[test]
    fn parse_url_manager_rules_with_params() {
        let text = r#"
        'urlManager' => [
            'rules' => [
                'posts' => 'post/index',
                'post/<id:\d+>' => 'post/view',
                '<userName:\w+>/posts/<slug>' => 'post/user',
            ],
        ],
        "#;
        let rules = parse_url_rules(text);
        assert!(rules.iter().any(|u| u.pattern == "posts" && u.route == "post/index"));
        let view = rules.iter().find(|u| u.route == "post/view").expect("view rule");
        assert_eq!(view.params, vec!["id".to_string()]);
        let user = rules.iter().find(|u| u.route == "post/user").expect("user rule");
        assert!(user.params.contains(&"userName".to_string()));
        assert!(user.params.contains(&"slug".to_string()));
        assert!(pattern_matches("post/<id:\\d+>", "post/42"));
        assert!(!pattern_matches("post/<id:\\d+>", "post/42/extra"));
    }

    #[test]
    fn action_params_signature() {
        let text = r#"
        class PostController extends Controller {
            public function actionView($id, string $slug = null) {
            }
        }
        "#;
        let params = action_params_from_controller_text(text, "actionView");
        assert_eq!(params, vec!["id".to_string(), "slug".to_string()]);
    }

    #[test]
    fn route_cursor_param_key() {
        let text = r#"Url::to(['post/view', 'id' => $model->id])"#;
        let off = text.find("'id'").unwrap() + 2;
        match route_cursor(text, off).expect("cursor") {
            RouteCursor::ParamName { route, param, used, .. } => {
                assert_eq!(route, "post/view");
                assert_eq!(param, "id");
                assert!(used.is_empty() || !used.contains(&"id".to_string()));
            }
            other => panic!("expected ParamName, got {other:?}"),
        }
    }

    #[test]
    fn route_cursor_expect_param_after_comma() {
        let text = r#"Url::to(['post/view', ])"#;
        let off = text.find(',').unwrap() + 2;
        match route_cursor(text, off).expect("expect param") {
            RouteCursor::ExpectParam { route, used } => {
                assert_eq!(route, "post/view");
                assert!(used.is_empty());
            }
            other => panic!("expected ExpectParam, got {other:?}"),
        }
    }

    #[test]
    fn used_params_skipped() {
        let text = r#"Url::to(['post/view', 'id' => 1, ])"#;
        let off = text.len() - 2;
        match route_cursor(text, off).expect("expect") {
            RouteCursor::ExpectParam { route, used } => {
                assert_eq!(route, "post/view");
                assert!(used.iter().any(|u| u == "id"));
            }
            other => panic!("expected ExpectParam with used id, got {other:?}"),
        }
    }

    #[test]
    fn bind_pattern_extracts_values() {
        let bound = bind_pattern("post/<id:\\d+>", "post/42").unwrap();
        assert_eq!(bound, vec![("id".into(), "42".into())]);
    }

    #[test]
    fn missing_params_filters_used() {
        let all = vec!["id".into(), "slug".into(), "page".into()];
        let used = vec!["id".into()];
        assert_eq!(missing_params(&all, &used), vec!["slug".into(), "page".into()]);
    }
    #[test]
    fn extract_render_view_vars() {
        let text = r#"return $this->render('index', ['model' => $model, 'title' => 'x']);"#;
        let calls = extract_render_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "index");
        let names: Vec<_> = calls[0].1.iter().map(|v| v.name.as_str()).collect();
        assert!(names.contains(&"model"));
        assert!(names.contains(&"title"));
    }

    #[test]
    fn view_dollar_context() {
        let text = "<?= $mod";
        let off = text.len();
        assert_eq!(view_dollar_var_context(text, off).as_deref(), Some("mod"));
    }

    #[test]
    fn asset_entry_strips_query() {
        assert_eq!(normalize_asset_entry("css/site.css?v=1.0"), "css/site.css");
        assert_eq!(normalize_asset_entry("'js/app.js#hash'"), "js/app.js");
    }

    #[test]
    fn expand_webroot_alias() {
        let v = expand_asset_alias("@webroot/media/manage");
        assert!(v.iter().any(|p| p == "web/media/manage"));
        assert!(v.iter().any(|p| p == "media/manage"));
    }

    #[test]
    fn asset_context_user_sample() {
        let text = r#"
namespace app\assets;
use yii\web\AssetBundle;
class AppAsset extends AssetBundle
{
    public $basePath = '@webroot/media/manage';
    public $baseUrl  = '@web/media/manage';
    public $css = [
        'css/site.css?v=1.0',
    ];
}
"#;
        let needle = "css/site.css?v=1.0";
        let off = text.find(needle).unwrap() + 5;
        let (lit, _) = asset_file_context(text, off).expect("ctx");
        assert!(lit.contains("site.css"));
        let info = parse_asset_bundle("app/assets/AppAsset.php", text).expect("parse");
        assert_eq!(info.base_path.as_deref(), Some("@webroot/media/manage"));
        assert!(info.css.iter().any(|c| c.contains("site.css")));
        assert_eq!(normalize_asset_entry(&lit), "css/site.css");
        let bases = expand_asset_alias(info.base_path.as_deref().unwrap());
        assert!(bases.iter().any(|b| b.contains("media/manage")));
    }

}
