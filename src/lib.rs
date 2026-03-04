#![deny(warnings)]

use {
    anyhow::{Context, Error, Result, anyhow, bail, ensure},
    async_trait::async_trait,
    bytes::Bytes,
    component_init_transform::Invoker,
    futures::future::FutureExt,
    heck::ToSnakeCase,
    indexmap::{IndexMap, IndexSet},
    serde::Deserialize,
    std::{
        borrow::Cow,
        collections::HashMap,
        fs,
        io::Cursor,
        iter,
        ops::Deref,
        path::{Path, PathBuf},
        str,
    },
    summary::{Escape, Locations, Summary},
    tar::Archive,
    wasm_encoder::{CustomSection, Section as _},
    wasmtime::{
        Config, Engine, Store,
        component::{Component, Instance, Linker, ResourceTable, ResourceType},
    },
    wasmtime_wasi::{
        DirPerms, FilePerms, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView,
        p2::pipe::{MemoryInputPipe, MemoryOutputPipe},
    },
    wit_component::metadata,
    wit_dylib::DylibOpts,
    wit_parser::{
        CloneMaps, FunctionKind, Package, PackageName, Resolve, Stability, TypeDefKind,
        UnresolvedPackageGroup, World, WorldId, WorldItem, WorldKey,
    },
    zstd::Decoder,
};

pub mod command;
mod link;
mod prelink;
#[cfg(feature = "pyo3")]
mod python;
mod stubwasi;
mod summary;
#[cfg(test)]
mod test;
mod util;

const DEBUG_PYTHON_BINDINGS: bool = false;

/// The default name of the Python module containing code generated from the
/// specified WIT world.  This may be overriden programatically or via the CLI
/// using the `--world-module` option.
static DEFAULT_WORLD_MODULE: &str = "wit_world";

wasmtime::component::bindgen!({
    path: "wit",
    world: "init",
    exports: { default: async },
});

// Serde-serializable types mirroring the WIT `Symbols` types from wit/init.wit.
// Used to serialize symbol metadata to JSON for the --no-snapshot mode.
pub mod serializable_symbols {
    use serde::{Deserialize, Serialize};

    #[derive(Serialize, Deserialize)]
    pub struct SymbolsConfig {
        pub app_name: String,
        pub stub_wasi: bool,
        pub symbols: Symbols,
    }

    #[derive(Serialize, Deserialize)]
    pub struct Symbols {
        pub exports: Vec<FunctionExport>,
        pub resources: Vec<Resource>,
        pub records: Vec<Record>,
        pub flags: Vec<Flags>,
        pub tuples: Vec<Tuple>,
        pub variants: Vec<Variant>,
        pub enums: Vec<Enum>,
        pub options: Vec<OptionKind>,
        pub results: Vec<ResultRecord>,
    }

    #[derive(Serialize, Deserialize)]
    pub struct FunctionExport {
        pub kind: FunctionExportKind,
        pub return_style: ReturnStyle,
    }

    #[derive(Serialize, Deserialize)]
    pub enum FunctionExportKind {
        Freestanding { protocol: String, name: String },
        Constructor { module: String, protocol: String },
        Method(String),
        Static { module: String, protocol: String, name: String },
    }

    #[derive(Serialize, Deserialize)]
    pub enum ReturnStyle {
        None,
        Normal,
        Result,
    }

    #[derive(Serialize, Deserialize)]
    pub struct Resource {
        pub package: String,
        pub name: String,
    }

    #[derive(Serialize, Deserialize)]
    pub struct Record {
        pub package: String,
        pub name: String,
        pub fields: Vec<String>,
    }

    #[derive(Serialize, Deserialize)]
    pub struct Flags {
        pub package: String,
        pub name: String,
        pub u32_count: u32,
    }

    #[derive(Serialize, Deserialize)]
    pub struct Tuple {
        pub count: u32,
    }

    #[derive(Serialize, Deserialize)]
    pub struct Case {
        pub name: String,
        pub has_payload: bool,
    }

    #[derive(Serialize, Deserialize)]
    pub struct Variant {
        pub package: String,
        pub name: String,
        pub cases: Vec<Case>,
    }

    #[derive(Serialize, Deserialize)]
    pub struct Enum {
        pub package: String,
        pub name: String,
        pub count: u32,
    }

    #[derive(Serialize, Deserialize)]
    pub enum OptionKind {
        NonNesting,
        Nesting,
    }

    #[derive(Serialize, Deserialize)]
    pub struct ResultRecord {
        pub has_ok: bool,
        pub has_err: bool,
    }
}

/// Convert wasmtime-bindgen `Symbols` to our serializable representation.
fn symbols_to_serializable(symbols: &exports::exports::Symbols) -> serializable_symbols::Symbols {
    use exports::exports as wit;
    serializable_symbols::Symbols {
        exports: symbols
            .exports
            .iter()
            .map(|e| serializable_symbols::FunctionExport {
                kind: match &e.kind {
                    wit::FunctionExportKind::Freestanding(f) => {
                        serializable_symbols::FunctionExportKind::Freestanding {
                            protocol: f.protocol.clone(),
                            name: f.name.clone(),
                        }
                    }
                    wit::FunctionExportKind::Constructor(c) => {
                        serializable_symbols::FunctionExportKind::Constructor {
                            module: c.module.clone(),
                            protocol: c.protocol.clone(),
                        }
                    }
                    wit::FunctionExportKind::Method(name) => {
                        serializable_symbols::FunctionExportKind::Method(name.clone())
                    }
                    wit::FunctionExportKind::Static(s) => {
                        serializable_symbols::FunctionExportKind::Static {
                            module: s.module.clone(),
                            protocol: s.protocol.clone(),
                            name: s.name.clone(),
                        }
                    }
                },
                return_style: match e.return_style {
                    wit::ReturnStyle::None => serializable_symbols::ReturnStyle::None,
                    wit::ReturnStyle::Normal => serializable_symbols::ReturnStyle::Normal,
                    wit::ReturnStyle::Result => serializable_symbols::ReturnStyle::Result,
                },
            })
            .collect(),
        resources: symbols
            .resources
            .iter()
            .map(|r| serializable_symbols::Resource {
                package: r.package.clone(),
                name: r.name.clone(),
            })
            .collect(),
        records: symbols
            .records
            .iter()
            .map(|r| serializable_symbols::Record {
                package: r.package.clone(),
                name: r.name.clone(),
                fields: r.fields.clone(),
            })
            .collect(),
        flags: symbols
            .flags
            .iter()
            .map(|f| serializable_symbols::Flags {
                package: f.package.clone(),
                name: f.name.clone(),
                u32_count: f.u32_count,
            })
            .collect(),
        tuples: symbols
            .tuples
            .iter()
            .map(|t| serializable_symbols::Tuple { count: t.count })
            .collect(),
        variants: symbols
            .variants
            .iter()
            .map(|v| serializable_symbols::Variant {
                package: v.package.clone(),
                name: v.name.clone(),
                cases: v
                    .cases
                    .iter()
                    .map(|c| serializable_symbols::Case {
                        name: c.name.clone(),
                        has_payload: c.has_payload,
                    })
                    .collect(),
            })
            .collect(),
        enums: symbols
            .enums
            .iter()
            .map(|e| serializable_symbols::Enum {
                package: e.package.clone(),
                name: e.name.clone(),
                count: e.count,
            })
            .collect(),
        options: symbols
            .options
            .iter()
            .map(|o| match o {
                wit::OptionKind::NonNesting => serializable_symbols::OptionKind::NonNesting,
                wit::OptionKind::Nesting => serializable_symbols::OptionKind::Nesting,
            })
            .collect(),
        results: symbols
            .results
            .iter()
            .map(|r| serializable_symbols::ResultRecord {
                has_ok: r.has_ok,
                has_err: r.has_err,
            })
            .collect(),
    }
}

pub struct Ctx {
    wasi: WasiCtx,
    table: ResourceTable,
}

impl WasiView for Ctx {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

#[derive(Clone)]
pub struct Library {
    name: String,
    module: Vec<u8>,
    dl_openable: bool,
}

#[derive(Deserialize)]
struct RawComponentizePyConfig {
    bindings: Option<String>,
    wit_directory: Option<String>,
    #[serde(default)]
    import_interface_names: HashMap<String, String>,
    #[serde(default)]
    export_interface_names: HashMap<String, String>,
}

#[derive(Debug)]
struct ComponentizePyConfig {
    bindings: Option<PathBuf>,
    wit_directory: Option<PathBuf>,
    import_interface_names: HashMap<String, String>,
    export_interface_names: HashMap<String, String>,
}

impl TryFrom<(&Path, RawComponentizePyConfig)> for ComponentizePyConfig {
    type Error = Error;

    fn try_from((path, raw): (&Path, RawComponentizePyConfig)) -> Result<Self> {
        let base = path.canonicalize()?;
        let convert = |p| {
            // Ensure this is a relative path under `base`:
            let p = base.join(p);
            let p = p.canonicalize().with_context(|| p.display().to_string())?;
            ensure!(p.starts_with(&base));
            Ok(p)
        };

        Ok(Self {
            bindings: raw.bindings.map(convert).transpose()?,
            wit_directory: raw.wit_directory.map(convert).transpose()?,
            import_interface_names: raw.import_interface_names,
            export_interface_names: raw.export_interface_names,
        })
    }
}

#[derive(Debug)]
pub struct ConfigContext<T> {
    module: String,
    root: PathBuf,
    path: PathBuf,
    config: T,
}

struct MyInvoker {
    store: Store<Ctx>,
    instance: Instance,
}

#[async_trait]
impl Invoker for MyInvoker {
    async fn call_s32(&mut self, function: &str) -> Result<i32> {
        let func = self
            .instance
            .get_typed_func::<(), (i32,)>(&mut self.store, function)?;
        let result = func.call_async(&mut self.store, ()).await?.0;
        func.post_return_async(&mut self.store).await?;
        Ok(result)
    }

    async fn call_s64(&mut self, function: &str) -> Result<i64> {
        let func = self
            .instance
            .get_typed_func::<(), (i64,)>(&mut self.store, function)?;
        let result = func.call_async(&mut self.store, ()).await?.0;
        func.post_return_async(&mut self.store).await?;
        Ok(result)
    }

    async fn call_f32(&mut self, function: &str) -> Result<f32> {
        let func = self
            .instance
            .get_typed_func::<(), (f32,)>(&mut self.store, function)?;
        let result = func.call_async(&mut self.store, ()).await?.0;
        func.post_return_async(&mut self.store).await?;
        Ok(result)
    }

    async fn call_f64(&mut self, function: &str) -> Result<f64> {
        let func = self
            .instance
            .get_typed_func::<(), (f64,)>(&mut self.store, function)?;
        let result = func.call_async(&mut self.store, ()).await?.0;
        func.post_return_async(&mut self.store).await?;
        Ok(result)
    }

    async fn call_list_u8(&mut self, function: &str) -> Result<Vec<u8>> {
        let func = self
            .instance
            .get_typed_func::<(), (Vec<u8>,)>(&mut self.store, function)?;
        let result = func.call_async(&mut self.store, ()).await?.0;
        func.post_return_async(&mut self.store).await?;
        Ok(result)
    }
}

#[allow(clippy::too_many_arguments)]
pub fn generate_bindings(
    wit_path: &[impl AsRef<Path>],
    world: Option<&str>,
    features: &[String],
    all_features: bool,
    world_module: Option<&str>,
    output_dir: &Path,
    import_interface_names: &HashMap<&str, &str>,
    export_interface_names: &HashMap<&str, &str>,
) -> Result<()> {
    // TODO: Split out and reuse the code responsible for finding and using
    // componentize-py.toml files in the `componentize` function below, since
    // that can affect the bindings we should be generating.

    let (resolve, world) = parse_wit(wit_path, world, features, all_features)?;
    let import_function_indexes = &HashMap::new();
    let export_function_indexes = &HashMap::new();
    let stream_and_future_indexes = &HashMap::new();
    let summary = Summary::try_new(
        &resolve,
        &iter::once(world).collect(),
        import_interface_names,
        export_interface_names,
        import_function_indexes,
        export_function_indexes,
        stream_and_future_indexes,
    )?;
    let world_module = world_module.unwrap_or(DEFAULT_WORLD_MODULE);
    let world_dir = output_dir.join(world_module.replace('.', "/"));
    fs::create_dir_all(&world_dir)?;
    summary.generate_code(
        &world_dir,
        world,
        world_module,
        &mut Locations::default(),
        true,
    )?;

    Archive::new(Decoder::new(Cursor::new(include_bytes!(concat!(
        env!("OUT_DIR"),
        "/bundled.tar.zst"
    ))))?)
    .unpack(output_dir)
    .unwrap();

    Ok(())
}

/// Recursively collect all files under `dir` into (relative_path, contents) pairs.
fn collect_dir_files(dir: &Path, prefix: &str) -> Result<Vec<(String, Vec<u8>)>> {
    let mut files = Vec::new();
    if !dir.exists() {
        return Ok(files);
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let name = entry.file_name();
        let name_str = name.to_str().unwrap();
        let rel_path = if prefix.is_empty() {
            name_str.to_string()
        } else {
            format!("{prefix}/{name_str}")
        };
        if ty.is_dir() {
            files.extend(collect_dir_files(&entry.path(), &rel_path)?);
        } else {
            files.push((rel_path, fs::read(entry.path())?));
        }
    }
    Ok(files)
}

/// Collect `.py` files from the given directories and serialize them into a
/// simple length-prefixed archive suitable for embedding in Wasm linear memory.
///
/// Format:
///   u32: number_of_entries
///   For each entry:
///     u32: module_name_length
///     bytes: module_name (UTF-8, e.g. "hello" or "mypackage.submod")
///     u32: source_length
///     bytes: source (UTF-8)
fn serialize_embed_sources(embed_paths: &[&str]) -> Result<Vec<u8>> {
    let mut entries: Vec<(String, Vec<u8>)> = Vec::new();

    for root in embed_paths {
        let root_path = Path::new(root).canonicalize()?;
        let files = collect_dir_files(&root_path, "")?;
        for (rel_path, data) in files {
            if !rel_path.ends_with(".py") {
                continue;
            }
            // Convert file path to Python module name:
            //   "hello.py"              -> "hello"
            //   "pkg/__init__.py"       -> "pkg"
            //   "pkg/sub.py"            -> "pkg.sub"
            let without_ext = rel_path.strip_suffix(".py").unwrap();
            let module_name = if without_ext.ends_with("/__init__") {
                without_ext
                    .strip_suffix("/__init__")
                    .unwrap()
                    .replace('/', ".")
            } else {
                without_ext.replace('/', ".")
            };
            if module_name.is_empty() {
                continue;
            }
            entries.push((module_name, data));
        }
    }

    let mut buf = Vec::new();
    buf.extend((entries.len() as u32).to_le_bytes());
    for (name, source) in &entries {
        let name_bytes = name.as_bytes();
        buf.extend((name_bytes.len() as u32).to_le_bytes());
        buf.extend(name_bytes);
        buf.extend((source.len() as u32).to_le_bytes());
        buf.extend(source);
    }
    Ok(buf)
}

/// Serialize all `.py` files under the world module directory into the same
/// length-prefixed archive format used by `serialize_embed_sources`.
///
/// `world_files` contains entries like `("world/wit_world/__init__.py", bytes)`.
/// `world_prefix` is e.g. `"world/wit_world/"`.
/// `module` is the top-level Python module name, e.g. `"wit_world"`.
fn serialize_world_files(
    world_files: &[(String, Vec<u8>)],
    world_prefix: &str,
    module: &str,
) -> Result<Vec<u8>> {
    let mut entries: Vec<(String, &[u8])> = Vec::new();

    for (path, data) in world_files {
        let Some(rel) = path.strip_prefix(world_prefix) else {
            continue;
        };
        if !rel.ends_with(".py") {
            continue;
        }
        let without_ext = rel.strip_suffix(".py").unwrap();
        let py_rel = if without_ext == "__init__" {
            ""
        } else if let Some(parent) = without_ext.strip_suffix("/__init__") {
            parent
        } else {
            without_ext
        };
        let module_name = if py_rel.is_empty() {
            module.to_string()
        } else {
            format!("{module}.{}", py_rel.replace('/', "."))
        };
        entries.push((module_name, data.as_slice()));
    }

    let mut buf = Vec::new();
    buf.extend((entries.len() as u32).to_le_bytes());
    for (name, source) in &entries {
        let name_bytes = name.as_bytes();
        buf.extend((name_bytes.len() as u32).to_le_bytes());
        buf.extend(name_bytes);
        buf.extend((source.len() as u32).to_le_bytes());
        buf.extend(*source);
    }
    Ok(buf)
}

/// Convert a library name to a valid kebab-case extern name for the component
/// import. Handles both standard libraries (`libc.so` → `c`) and path-based
/// native extensions from Python packages
/// (`/1/numpy/core/_multiarray_umath.cpython-314-wasm32-wasi.so`
/// → `numpy-core-multiarray-umath`).
fn to_extern_name(name: &str) -> String {
    let mut s = name.to_string();

    // Strip .so suffix
    if let Some(stripped) = s.strip_suffix(".so") {
        s = stripped.to_string();
    }

    // Strip cpython version tag (e.g. ".cpython-314-wasm32-wasi")
    if let Some(idx) = s.find(".cpython-") {
        s = s[..idx].to_string();
    }

    // Strip lib prefix (only for standard lib*.so names, not paths)
    if !s.starts_with('/') {
        if let Some(stripped) = s.strip_prefix("lib") {
            s = stripped.to_string();
        }
    }

    // Strip leading path index (e.g. "/0/", "/1/") generated by prelink
    let s = s.trim_start_matches(|c: char| c == '/' || c.is_ascii_digit());

    // Replace characters that aren't valid in kebab-case
    let s: String = s
        .chars()
        .map(|c| match c {
            '+' => 'p',
            '/' | '_' | '.' => '-',
            c => c,
        })
        .collect();

    // Collapse consecutive dashes and strip leading/trailing dashes
    let mut result = String::new();
    let mut prev_dash = true; // skip leading dashes
    for c in s.chars() {
        if c == '-' {
            if !prev_dash {
                result.push('-');
                prev_dash = true;
            }
        } else {
            result.push(c);
            prev_dash = false;
        }
    }
    if result.ends_with('-') {
        result.pop();
    }

    result
}

#[allow(clippy::type_complexity, clippy::too_many_arguments)]
pub async fn componentize(
    wit_path: &[impl AsRef<Path>],
    world: Option<&str>,
    features: &[String],
    all_features: bool,
    world_module: Option<&str>,
    python_path: &[&str],
    module_worlds: &[(&str, &str)],
    app_name: &str,
    output_path: &Path,
    add_to_linker: Option<&dyn Fn(&mut Linker<Ctx>) -> Result<()>>,
    stub_wasi: bool,
    import_interface_names: &HashMap<&str, &str>,
    export_interface_names: &HashMap<&str, &str>,
    no_snapshot: bool,
    runtime_dir: Option<&Path>,
    shared_modules: Option<&str>,
    embed_path: &[&str],
) -> Result<()> {
    // Remove non-existent elements from `python_path` so we don't choke on them
    // later:
    let embed_path = &embed_path
        .iter()
        .filter_map(|&s| Path::new(s).exists().then_some(s))
        .collect::<Vec<_>>();
    let python_path = &python_path
        .iter()
        .filter_map(|&s| Path::new(s).exists().then_some(s))
        .collect::<Vec<_>>();

    let embedded_python_standard_lib = prelink::embedded_python_standard_library()?;
    let embedded_helper_utils = prelink::embedded_helper_utils()?;

    // Use only python_path (not embed_path) for library/config scanning.
    // Embed paths contain only .py source files that are baked into the
    // component's linear memory; they don't need WASI filesystem mounts or
    // native library indexing at runtime.  Keeping them out of the index
    // ensures native library paths (e.g. /0/numpy/...) match the host's
    // WASI mount points.
    let (configs, libraries) =
        prelink::search_for_libraries_and_configs(python_path, module_worlds, world)?;

    // Next, iterate over all the WIT directories, merging them into a single
    // `Resolve`, and matching Python packages to `WorldId`s.
    let (mut resolve, mut main_world) = match wit_path {
        [] => (None, None),
        paths => {
            let (resolve, world) = parse_wit(paths, world, features, all_features)?;
            (Some(resolve), Some(world))
        }
    };

    let import_interface_names = import_interface_names
        .iter()
        .map(|(a, b)| (*a, *b))
        .chain(configs.iter().flat_map(|(_, (config, _))| {
            config
                .config
                .import_interface_names
                .iter()
                .map(|(a, b)| (a.as_str(), b.as_str()))
        }))
        .collect();

    let export_interface_names = export_interface_names
        .iter()
        .map(|(a, b)| (*a, *b))
        .chain(configs.iter().flat_map(|(_, (config, _))| {
            config
                .config
                .export_interface_names
                .iter()
                .map(|(a, b)| (a.as_str(), b.as_str()))
        }))
        .collect();

    let configs = configs
        .iter()
        .map(|(module, (config, world))| {
            Ok((module, match (world, config.config.wit_directory.as_deref()) {
                (_, Some(wit_path)) => {
                    let paths = &[config.path.join(wit_path)];
                    let (my_resolve, mut world) = parse_wit(paths, *world, features, all_features)?;

                    if let Some(resolve) = &mut resolve {
                        let remap = resolve.merge(my_resolve)?;
                        world = remap.worlds[world.index()].expect("missing world");
                    } else {
                        resolve = Some(my_resolve);
                    }

                    (config, Some(world))
                }
                (None, None) => (config, None),
                (Some(_), None) => {
                    bail!("no `wit-directory` specified in `componentize-py.toml` for module `{module}`");
                }
            }))
        })
        .collect::<Result<IndexMap<_, _>>>()?;

    let mut resolve = if let Some(resolve) = resolve {
        resolve
    } else {
        // If no WIT directory was provided as a parameter and none were
        // referenced by Python packages, use the default values.
        let paths: &[&Path] = &[];
        let (my_resolve, world) = parse_wit(paths, world, features, all_features).context(
            "no WIT files found; please specify the directory or file \
             containing the WIT world you wish to target",
        )?;
        main_world = Some(world);
        my_resolve
    };

    // Extract relevant metadata from the `Resolve` into a `Summary` instance,
    // which we'll use to generate Wasm- and Python-level bindings.

    let worlds = configs
        .values()
        .filter_map(|(_, world)| *world)
        .chain(main_world)
        .collect::<IndexSet<_>>();

    if worlds
        .iter()
        .any(|&id| app_name == resolve.worlds[id].name.to_snake_case().escape())
    {
        bail!(
            "App name `{app_name}` conflicts with world name; please rename your application module."
        );
    }

    let union_package = resolve.packages.alloc(Package {
        name: PackageName {
            namespace: "componentize-py".into(),
            name: "union".into(),
            version: None,
        },
        docs: Default::default(),
        interfaces: Default::default(),
        worlds: Default::default(),
    });

    let union_world = resolve.worlds.alloc(World {
        name: "union".into(),
        imports: Default::default(),
        exports: Default::default(),
        package: Some(union_package),
        docs: Default::default(),
        stability: Stability::Unknown,
        includes: Default::default(),
        include_names: Default::default(),
    });

    resolve.packages[union_package]
        .worlds
        .insert("union".into(), union_world);

    let mut clone_maps = CloneMaps::default();
    for &world in &worlds {
        resolve.merge_worlds(world, union_world, &mut clone_maps)?;
    }

    let (mut bindings, metadata) = wit_dylib::create_with_metadata(
        &resolve,
        union_world,
        Some(&mut DylibOpts {
            interpreter: Some("libcomponentize_py_runtime.so".into()),
            async_: Default::default(),
        }),
    );

    CustomSection {
        name: Cow::Borrowed("component-type:componentize-py-union"),
        data: Cow::Owned(metadata::encode(
            &resolve,
            union_world,
            wit_component::StringEncoding::UTF8,
            None,
        )?),
    }
    .append_to(&mut bindings);

    let imported_function_indexes = metadata
        .import_funcs
        .iter()
        .enumerate()
        .map(|(index, func)| ((func.interface.as_deref(), func.name.as_str()), index))
        .collect();

    let exported_function_indexes = metadata
        .export_funcs
        .iter()
        .enumerate()
        .map(|(index, func)| ((func.interface.as_deref(), func.name.as_str()), index))
        .collect();

    let mut reverse_cloned_types = HashMap::new();
    for (&original, &clone) in clone_maps.types() {
        assert!(reverse_cloned_types.insert(clone, original).is_none());
    }

    let original = |ty| {
        if let Some(&original) = reverse_cloned_types.get(&ty) {
            original
        } else {
            ty
        }
    };

    let stream_and_future_indexes = metadata
        .streams
        .iter()
        .enumerate()
        .map(|(index, stream)| (original(stream.id), index))
        .chain(
            metadata
                .futures
                .iter()
                .enumerate()
                .map(|(index, future)| (original(future.id), index)),
        )
        .collect();

    let summary = Summary::try_new(
        &resolve,
        &worlds,
        &import_interface_names,
        &export_interface_names,
        &imported_function_indexes,
        &exported_function_indexes,
        &stream_and_future_indexes,
    )?;

    let need_async = summary.need_async();

    // Now that we know whether to use the sync or async version of
    // `libcomponentize_py_runtime.so`, update `libraries` accordingly.
    //
    // Note that we have two separate versions because older runtimes don't
    // understand the new async ABI, so we only use the async version if it's
    // actually needed.
    let mut libraries = libraries
        .into_iter()
        .filter_map(|library| match (need_async, library.name.as_str()) {
            (true, "libcomponentize_py_runtime_sync.so")
            | (false, "libcomponentize_py_runtime_async.so") => None,
            (true, "libcomponentize_py_runtime_async.so")
            | (false, "libcomponentize_py_runtime_sync.so") => Some(Library {
                name: "libcomponentize_py_runtime.so".into(),
                ..library
            }),
            _ => Some(library),
        })
        .collect::<Vec<_>>();

    libraries.push(Library {
        name: "libcomponentize_py_bindings.so".into(),
        module: bindings,
        dl_openable: false,
    });

    // Determine which libraries should be externalized (shared modules)
    let external_names: std::collections::HashSet<String> = match shared_modules {
        Some("auto") => {
            // Externalize all libraries except the app-specific bindings
            // module.  This includes the standard runtime libraries (libc,
            // libpython, etc.) and any native extensions from Python packages
            // (e.g. numpy's .cpython-314-wasm32-wasi.so files).
            libraries
                .iter()
                .filter(|lib| {
                    lib.name.as_str() != "libcomponentize_py_bindings.so"
                })
                .map(|lib| lib.name.clone())
                .collect()
        }
        Some(names) => names.split(',').map(|s| s.trim().to_string()).collect(),
        None => std::collections::HashSet::new(),
    };

    // When shared modules are active with --no-snapshot, defer linking until
    // app data is ready so it can be embedded in the component's linear memory.
    // Otherwise, link immediately.
    let has_shared_modules = !external_names.is_empty();
    let (component, external_modules) = if !has_shared_modules || !no_snapshot {
        if external_names.is_empty() {
            (link::link_libraries(&libraries)?, Vec::new())
        } else {
            link::link_libraries_with_externals(&libraries, &external_names, None)?
        }
    } else {
        // Will be linked later in the no_snapshot block with app data
        (Vec::new(), Vec::new())
    };

    let stubbed_component = if stub_wasi && !no_snapshot {
        stubwasi::link_stub_modules(libraries.clone())?
    } else {
        None
    };

    // Pre-initialize the component by running it through
    // `component_init_transform::initialize`.  Currently, this is the
    // application's first and only chance to load any standard or third-party
    // modules since we do not yet include a virtual filesystem in the component
    // to make those modules available at runtime.

    let stdout = MemoryOutputPipe::new(10000);
    let stderr = MemoryOutputPipe::new(10000);

    let mut wasi = WasiCtxBuilder::new();
    wasi.stdin(MemoryInputPipe::new(Bytes::new()))
        .stdout(stdout.clone())
        .stderr(stderr.clone())
        .env("PYTHONUNBUFFERED", "1")
        .env("PYTHONHOME", "/python")
        .preopened_dir(
            embedded_python_standard_lib.path(),
            "python",
            DirPerms::all(),
            FilePerms::all(),
        )?
        .preopened_dir(
            embedded_helper_utils.path(),
            "bundled",
            DirPerms::all(),
            FilePerms::all(),
        )?;

    // Generate guest mounts for each host directory in python_path.
    for (index, path) in python_path.iter().enumerate() {
        wasi.preopened_dir(path, index.to_string(), DirPerms::all(), FilePerms::all())?;
    }

    // For each Python package with a `componentize-py.toml` file that specifies
    // where generated bindings for that package should be placed, generate the
    // bindings and place them as indicated.

    let mut world_dir_mounts = Vec::new();
    let mut locations = Locations::default();
    let mut saw_main_world = false;

    for (config, world, binding_path) in configs
        .values()
        .filter_map(|(config, world)| Some((config, world, config.config.bindings.as_deref()?)))
    {
        if *world == main_world {
            saw_main_world = true;
        }

        let Some(world) = *world else {
            bail!("please specify a world for module `{}`", config.module);
        };

        let paths = python_path
            .iter()
            .enumerate()
            .map(|(index, dir)| {
                let dir = Path::new(dir).canonicalize()?;
                Ok(if config.root == dir {
                    config
                        .path
                        .join(binding_path)
                        .strip_prefix(dir)
                        .ok()
                        .map(|p| (index, p.to_str().unwrap().replace('\\', "/")))
                } else {
                    None
                })
            })
            .filter_map(Result::transpose)
            .collect::<Result<Vec<_>>>()?;

        let binding_module = paths.first().unwrap().1.replace('/', ".");

        let world_dir = tempfile::tempdir()?;

        summary.generate_code(
            world_dir.path(),
            world,
            &binding_module,
            &mut locations,
            false,
        )?;

        world_dir_mounts.push((
            paths
                .iter()
                .map(|(index, p)| format!("{index}/{p}"))
                .collect(),
            world_dir,
        ));
    }

    // If the caller specified a world and we haven't already generated bindings
    // for it above, do so now.
    if let (Some(world), false) = (main_world, saw_main_world) {
        let module = world_module.unwrap_or(DEFAULT_WORLD_MODULE);
        let world_dir = tempfile::tempdir()?;
        let module_path = world_dir.path().join(module);
        fs::create_dir_all(&module_path)?;
        summary.generate_code(&module_path, world, module, &mut locations, false)?;
        world_dir_mounts.push((vec!["world".to_owned()], world_dir));

        // The helper utilities are hard-coded to assume the world module is
        // named `wit_world`.  Here we replace that with the actual world module
        // name.
        fn replace(path: &Path, pattern: &str, replacement: &str) -> Result<()> {
            if path.is_dir() {
                for entry in fs::read_dir(path)? {
                    replace(&entry?.path(), pattern, replacement)?;
                }
            } else {
                fs::write(
                    path,
                    fs::read_to_string(path)?
                        .replace(pattern, replacement)
                        .as_bytes(),
                )?;
            }

            Ok(())
        }
        replace(embedded_helper_utils.path(), "wit_world", module)?;
    };

    // Generate a `Symbols` object containing metadata to be passed to the
    // pre-init function.  The runtime library will use this to look up types
    // and functions that will later be referenced by the generated Wasm code.
    let symbols = summary.collect_symbols(&locations, &metadata, &clone_maps);

    if no_snapshot {
        // --no-snapshot mode: write the linked component directly and
        // generate supporting files to the runtime directory.

        fn copy_dir_all(src: &Path, dst: &Path) -> Result<()> {
            fs::create_dir_all(dst)?;
            for entry in fs::read_dir(src)? {
                let entry = entry?;
                let ty = entry.file_type()?;
                let dst_path = dst.join(entry.file_name());
                if ty.is_dir() {
                    copy_dir_all(&entry.path(), &dst_path)?;
                } else {
                    fs::copy(entry.path(), dst_path)?;
                }
            }
            Ok(())
        }

        let runtime_dir = if let Some(dir) = runtime_dir {
            dir.to_path_buf()
        } else {
            output_path
                .parent()
                .unwrap_or(Path::new("."))
                .join("runtime")
        };
        fs::create_dir_all(&runtime_dir)?;

        // 1. Extract Python stdlib
        let python_dir = runtime_dir.join("python");
        if !python_dir.exists() {
            copy_dir_all(embedded_python_standard_lib.path(), &python_dir)?;
        }

        // 2. Copy bundled helpers
        let bundled_dir = runtime_dir.join("bundled");
        if !bundled_dir.exists() {
            copy_dir_all(embedded_helper_utils.path(), &bundled_dir)?;
        }

        // 3. Serialize symbols config
        let config = serializable_symbols::SymbolsConfig {
            app_name: app_name.to_owned(),
            stub_wasi,
            symbols: symbols_to_serializable(&symbols),
        };
        let config_json = serde_json::to_string_pretty(&config)?;

        // 4. Collect WIT bindings files from all world dirs
        let mut world_files: Vec<(String, Vec<u8>)> = Vec::new();
        for (_mounts, world_dir) in world_dir_mounts.iter() {
            world_files.extend(collect_dir_files(world_dir.path(), "world")?);
        }

        let module = world_module.unwrap_or(DEFAULT_WORLD_MODULE);

        // When shared modules are active, embed the app-specific data
        // directly in the component's linear memory via the __init module.
        // Otherwise, write them to the runtime directory for filesystem access.
        let (component, external_modules) = if has_shared_modules {
            let world_prefix = format!("world/{module}/");
            let world_source = serialize_world_files(&world_files, &world_prefix, module)?;

            let app_sources = if !embed_path.is_empty() {
                serialize_embed_sources(embed_path)?
            } else {
                Vec::new()
            };

            let app_data = Some((
                config_json.as_bytes().to_vec(),
                module.to_string(),
                world_source,
                app_sources,
            ));

            link::link_libraries_with_externals(&libraries, &external_names, app_data)?
        } else {
            // No shared modules: write app-specific files to runtime dir
            let world_output = runtime_dir.join("world");
            fs::create_dir_all(&world_output)?;
            for (rel_path, data) in &world_files {
                let dest = runtime_dir.join(rel_path);
                if let Some(parent) = dest.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(&dest, data)?;
            }
            fs::write(runtime_dir.join("symbols.json"), &config_json)?;
            (component, external_modules)
        };

        // 5. Write external shared modules (if any)
        if !external_modules.is_empty() {
            let shared_dir = output_path
                .parent()
                .unwrap_or(Path::new("."))
                .join("shared");
            fs::create_dir_all(&shared_dir)?;
            for (name, module_bytes) in &external_modules {
                // Convert library name to the kebab-case extern name used
                // in the component import (matches wit-component encoding)
                let extern_name = to_extern_name(name);
                let wasm_name = format!("{extern_name}.wasm");
                fs::write(shared_dir.join(&wasm_name), module_bytes)?;
            }
        }

        // 6. Write the linked component directly (no snapshot)
        if let Some(parent) = output_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(output_path, &component)?;

        return Ok(());
    }

    for (mounts, world_dir) in world_dir_mounts.iter() {
        for mount in mounts {
            if DEBUG_PYTHON_BINDINGS {
                eprintln!("world dir path: {}", world_dir.path().display());
            }
            wasi.preopened_dir(world_dir.path(), mount, DirPerms::all(), FilePerms::all())?;
        }
    }

    if DEBUG_PYTHON_BINDINGS {
        // Prevent temporary directories from being deleted:
        std::mem::forget(world_dir_mounts);
    }

    // Standard path: pre-initialize the component by running it through
    // `component_init_transform::initialize`.  Currently, this is the
    // application's first and only chance to load any standard or third-party
    // modules since we do not yet include a virtual filesystem in the component
    // to make those modules available at runtime.

    let python_path_env = (0..python_path.len())
        .map(|index| format!("/{index}"))
        .collect::<Vec<_>>()
        .join(":");

    let table = ResourceTable::new();
    let wasi = wasi
        .env(
            "PYTHONPATH",
            format!("/python:/world:{python_path_env}:/bundled"),
        )
        .build();

    let mut config = Config::new();
    config.wasm_component_model(true);
    config.wasm_component_model_async(true);
    config.async_support(true);

    let engine = Engine::new(&config)?;

    let mut linker = Linker::new(&engine);
    let added_to_linker = if let Some(add_to_linker) = add_to_linker {
        add_to_linker(&mut linker)?;
        true
    } else {
        false
    };

    let mut store = Store::new(&engine, Ctx { wasi, table });

    let app_name = app_name.to_owned();
    let component = component_init_transform::initialize_staged(
        &component,
        stubbed_component
            .as_ref()
            .map(|(component, map)| (component.deref(), map as &dyn Fn(u32) -> u32)),
        move |instrumented| {
            async move {
                let component = &Component::new(&engine, instrumented)?;
                if !added_to_linker {
                    add_wasi_and_stubs(&resolve, &worlds, &mut linker)?;
                }

                let pre = InitPre::new(linker.instantiate_pre(component)?)?;
                let instance = pre.instance_pre.instantiate_async(&mut store).await?;
                let guest = pre.indices.interface0.load(&mut store, &instance)?;

                guest
                    .call_init(&mut store, &app_name, &symbols, stub_wasi)
                    .await?
                    .map_err(|e| anyhow!("{e}"))?;

                Ok(Box::new(MyInvoker { store, instance }) as Box<dyn Invoker>)
            }
            .boxed()
        },
    )
    .await
    .with_context(move || {
        format!(
            "{}{}",
            String::from_utf8_lossy(&stdout.try_into_inner().unwrap()),
            String::from_utf8_lossy(&stderr.try_into_inner().unwrap())
        )
    })?;

    // Checks if the output directory exists, and creates it if it doesn't.
    if let Some(parent) = output_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(output_path, component)?;

    Ok(())
}

fn parse_wit(
    paths: &[impl AsRef<Path>],
    world: Option<&str>,
    features: &[String],
    all_features: bool,
) -> Result<(Resolve, WorldId)> {
    // If no WIT directory was provided as a parameter and none were referenced
    // by Python packages, use ./wit by default.
    if paths.is_empty() {
        let paths = &[Path::new("wit")];
        return parse_wit(paths, world, features, all_features);
    }
    debug_assert!(!paths.is_empty(), "The paths should not be empty");

    let mut resolve = Resolve {
        all_features,
        ..Default::default()
    };
    for features in features {
        for feature in features
            .split(',')
            .flat_map(|s| s.split_whitespace())
            .filter(|f| !f.is_empty())
        {
            resolve.features.insert(feature.to_string());
        }
    }

    let mut last_pkg = None;
    for path in paths.iter().map(AsRef::as_ref) {
        let pkg = if path.is_dir() {
            resolve.push_dir(path)?.0
        } else {
            let pkg = UnresolvedPackageGroup::parse_file(path)?;
            resolve.push_group(pkg)?
        };
        last_pkg = Some(pkg);
    }

    let pkg = last_pkg.unwrap(); // The paths should not be empty
    let world = resolve.select_world(&[pkg], world)?;

    Ok((resolve, world))
}

fn add_wasi_and_stubs(
    resolve: &Resolve,
    worlds: &IndexSet<WorldId>,
    linker: &mut Linker<Ctx>,
) -> Result<()> {
    wasmtime_wasi::p2::add_to_linker_async(linker)?;

    enum Stub<'a> {
        Function(&'a String, &'a FunctionKind),
        Resource(&'a String),
    }

    let mut stubs = HashMap::<_, Vec<_>>::new();
    for &world in worlds {
        for (key, item) in &resolve.worlds[world].imports {
            match item {
                WorldItem::Interface { id, .. } => {
                    let interface_name = match key {
                        WorldKey::Name(name) => name.clone(),
                        WorldKey::Interface(interface) => resolve.id_of(*interface).unwrap(),
                    };

                    let interface = &resolve.interfaces[*id];
                    for (function_name, function) in &interface.functions {
                        stubs
                            .entry(Some(interface_name.clone()))
                            .or_default()
                            .push(Stub::Function(function_name, &function.kind));
                    }

                    for (type_name, id) in interface.types.iter() {
                        if let TypeDefKind::Resource = &resolve.types[*id].kind {
                            stubs
                                .entry(Some(interface_name.clone()))
                                .or_default()
                                .push(Stub::Resource(type_name));
                        }
                    }
                }
                WorldItem::Function(function) => {
                    stubs
                        .entry(None)
                        .or_default()
                        .push(Stub::Function(&function.name, &function.kind));
                }
                WorldItem::Type(id) => {
                    let ty = &resolve.types[*id];
                    if let TypeDefKind::Resource = &ty.kind {
                        stubs
                            .entry(None)
                            .or_default()
                            .push(Stub::Resource(ty.name.as_ref().unwrap()));
                    }
                }
            }
        }
    }

    for (interface_name, stubs) in stubs {
        if let Some(interface_name) = interface_name {
            // Note that we do _not_ stub interfaces which appear to be part of
            // WASIp2 since those should be provided by the
            // `wasmtime_wasi::add_to_linker_async` call above, and adding stubs
            // to those same interfaces would just cause trouble.
            if !is_wasip2_cli(&interface_name)
                && let Ok(mut instance) = linker.instance(&interface_name)
            {
                for stub in stubs {
                    let interface_name = interface_name.clone();
                    match stub {
                        Stub::Function(name, kind) => {
                            if kind.is_async() {
                                instance.func_new_concurrent(name, {
                                    let name = name.clone();
                                    move |_, _, _, _| {
                                        let interface_name = interface_name.clone();
                                        let name = name.clone();
                                        Box::pin(async move {
                                            Err(anyhow!(
                                                "called trapping stub: {interface_name}#{name}"
                                            ))
                                        })
                                    }
                                })
                            } else {
                                instance.func_new(name, {
                                    let name = name.clone();
                                    move |_, _, _, _| {
                                        Err(anyhow!(
                                            "called trapping stub: {interface_name}#{name}"
                                        ))
                                    }
                                })
                            }
                        }
                        Stub::Resource(name) => instance
                            .resource(name, ResourceType::host::<()>(), {
                                let name = name.clone();
                                move |_, _| {
                                    Err(anyhow!("called trapping stub: {interface_name}#{name}"))
                                }
                            })
                            .map(drop),
                    }?;
                }
            }
        } else {
            let mut instance = linker.root();
            for stub in stubs {
                match stub {
                    Stub::Function(name, kind) => {
                        if kind.is_async() {
                            instance.func_new_concurrent(name, {
                                let name = name.clone();
                                move |_, _, _, _| {
                                    let name = name.clone();
                                    Box::pin(
                                        async move { Err(anyhow!("called trapping stub: {name}")) },
                                    )
                                }
                            })
                        } else {
                            instance.func_new(name, {
                                let name = name.clone();
                                move |_, _, _, _| Err(anyhow!("called trapping stub: {name}"))
                            })
                        }
                    }
                    Stub::Resource(name) => instance
                        .resource(name, ResourceType::host::<()>(), {
                            let name = name.clone();
                            move |_, _| Err(anyhow!("called trapping stub: {name}"))
                        })
                        .map(drop),
                }?;
            }
        }
    }

    Ok(())
}

fn is_wasip2_cli(interface_name: &str) -> bool {
    (interface_name.starts_with("wasi:cli/")
        || interface_name.starts_with("wasi:clocks/")
        || interface_name.starts_with("wasi:random/")
        || interface_name.starts_with("wasi:io/")
        || interface_name.starts_with("wasi:filesystem/")
        || interface_name.starts_with("wasi:sockets/"))
        && interface_name.contains("@0.2.")
}
