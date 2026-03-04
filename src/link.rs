use std::collections::HashSet;
use std::io::Cursor;

use anyhow::Result;

use crate::Library;

pub fn link_libraries(libraries: &[Library]) -> Result<Vec<u8>> {
    link_libraries_with_externals(libraries, &HashSet::new(), None)
        .map(|(component, _)| component)
}

/// Link libraries into a component, marking specified library names as external.
///
/// Returns the component bytes and a list of `(name, module_bytes)` for each
/// external library whose module bytes should be provided separately at runtime.
///
/// If `app_data` is provided `(symbols_json, world_module_name, world_source,
/// app_sources_archive)`, the data will be embedded in the component's linear
/// memory and the runtime will read it directly instead of from the WASI
/// filesystem.
pub fn link_libraries_with_externals(
    libraries: &[Library],
    external_names: &HashSet<String>,
    app_data: Option<(Vec<u8>, String, Vec<u8>, Vec<u8>)>,
) -> Result<(Vec<u8>, Vec<(String, Vec<u8>)>)> {
    let mut linker = wit_component::Linker::default()
        .validate(true)
        .use_built_in_libdl(true);

    if let Some((symbols, module_name, source, app_sources)) = app_data {
        linker = linker.app_data(symbols, module_name, source, app_sources);
    }

    let mut external_modules: Vec<(String, Vec<u8>)> = Vec::new();

    for Library {
        name,
        module,
        dl_openable,
    } in libraries
    {
        if external_names.contains(name) {
            linker = linker.external_library(name, module, *dl_openable)?;
            external_modules.push((name.clone(), module.clone()));
        } else {
            linker = linker.library(name, module, *dl_openable)?;
        }
    }

    linker = linker.adapter(
        "wasi_snapshot_preview1",
        &zstd::decode_all(Cursor::new(include_bytes!(concat!(
            env!("OUT_DIR"),
            "/wasi_snapshot_preview1.reactor.wasm.zst"
        ))))?,
    )?;

    let component = linker.encode().map_err(|e| anyhow::anyhow!(e))?;
    Ok((component, external_modules))
}
