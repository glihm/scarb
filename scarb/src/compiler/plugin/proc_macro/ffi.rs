use crate::core::{Config, Package, PackageId};
use anyhow::{Context, Result};
use cairo_lang_defs::patcher::PatchBuilder;
use cairo_lang_macro::{AuxData, ProcMacroResult, TokenStream};
use cairo_lang_macro_stable::{
    StableAuxData, StableProcMacroResult, StableResultWrapper, StableTokenStream,
};
use cairo_lang_syntax::node::db::SyntaxGroup;
use cairo_lang_syntax::node::{ast, TypedSyntaxNode};
use camino::Utf8PathBuf;
use libloading::{Library, Symbol};
use std::fmt::Debug;

use crate::compiler::plugin::proc_macro::compilation::SharedLibraryProvider;
use crate::compiler::plugin::proc_macro::ProcMacroAuxData;
use cairo_lang_macro_stable::ffi::StableSlice;
#[cfg(not(windows))]
use libloading::os::unix::Symbol as RawSymbol;
#[cfg(windows)]
use libloading::os::windows::Symbol as RawSymbol;

pub const PROC_MACRO_BUILD_PROFILE: &str = "release";

pub trait FromItemAst {
    fn from_item_ast(db: &dyn SyntaxGroup, item_ast: ast::ModuleItem) -> Self;
}

impl FromItemAst for TokenStream {
    fn from_item_ast(db: &dyn SyntaxGroup, item_ast: ast::ModuleItem) -> Self {
        let mut builder = PatchBuilder::new(db);
        builder.add_node(item_ast.as_syntax_node());
        Self::new(builder.code)
    }
}

/// Representation of a single procedural macro.
///
/// This struct is a wrapper around a shared library containing the procedural macro implementation.
/// It is responsible for loading the shared library and providing a safe interface for code expansion.
pub struct ProcMacroInstance {
    package_id: PackageId,
    plugin: Plugin,
}

impl Debug for ProcMacroInstance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProcMacroInstance")
            .field("package_id", &self.package_id)
            .finish()
    }
}

impl ProcMacroInstance {
    pub fn package_id(&self) -> PackageId {
        self.package_id
    }

    /// Load shared library
    pub fn try_new(package: Package, config: &Config) -> Result<Self> {
        let lib_path = package.shared_lib_path(config);
        let plugin = unsafe { Plugin::try_new(lib_path.to_path_buf())? };
        Ok(Self {
            plugin,
            package_id: package.id,
        })
    }
    pub fn declared_attributes(&self) -> Vec<String> {
        vec![self.package_id.name.to_string()]
    }

    /// Apply expansion to token stream.
    ///
    /// This function implements the actual calls to functions from the dynamic library.
    ///
    /// All values passing the FFI-barrier must implement a stable ABI.
    ///
    /// Please be aware that the memory management of values passing the FFI-barrier is tricky.
    /// The memory must be freed on the same side of the barrier, where the allocation was made.
    pub(crate) fn generate_code(&self, token_stream: TokenStream) -> ProcMacroResult {
        // This must be manually freed with call to from_owned_stable.
        let stable_token_stream = token_stream.into_stable();
        // Call FFI interface for code expansion.
        // Note that `stable_result` has been allocated by the dynamic library.
        let stable_result = (self.plugin.vtable.expand)(stable_token_stream);
        // Free the memory allocated by the `stable_token_stream`.
        // This will call `CString::from_raw` under the hood, to take ownership.
        unsafe {
            TokenStream::from_owned_stable(stable_result.input);
        };
        // Create Rust representation of the result.
        // Note, that the memory still needs to be freed on the allocator side!
        let result = unsafe { ProcMacroResult::from_stable(&stable_result.output) };
        // Call FFI interface to free the `stable_result` that has been allocated by previous call.
        (self.plugin.vtable.free_result)(stable_result.output);
        // Return obtained result.
        result
    }

    pub(crate) fn aux_data_callback(&self, aux_data: Vec<ProcMacroAuxData>) {
        // Convert to stable aux data.
        let aux_data: Vec<AuxData> = aux_data.into_iter().map(Into::into).collect();
        let aux_data = aux_data
            .into_iter()
            .map(|a| a.into_stable())
            .collect::<Vec<_>>();
        // Create stable slice representation from vector.
        // Note this needs to be freed manually.
        let aux_data = StableSlice::new(aux_data);
        // Actual call to FFI interface for aux data callback.
        let aux_data = (self.plugin.vtable.aux_data_callback)(aux_data);
        // Free the memory allocated by vec.
        let _ = aux_data.into_owned();
    }
}

type ExpandCode = extern "C" fn(StableTokenStream) -> StableResultWrapper;
type FreeResult = extern "C" fn(StableProcMacroResult);
type AuxDataCallback = extern "C" fn(StableSlice<StableAuxData>) -> StableSlice<StableAuxData>;

struct VTableV0 {
    expand: RawSymbol<ExpandCode>,
    free_result: RawSymbol<FreeResult>,
    aux_data_callback: RawSymbol<AuxDataCallback>,
}

impl VTableV0 {
    unsafe fn try_new(library: &Library) -> Result<VTableV0> {
        let expand: Symbol<'_, ExpandCode> = library
            .get(b"expand\0")
            .context("failed to load expand function for procedural macro")?;
        let expand = expand.into_raw();
        let free_result: Symbol<'_, FreeResult> = library
            .get(b"free_result\0")
            .context("failed to load free_result function for procedural macro")?;
        let free_result = free_result.into_raw();
        let aux_data_callback: Symbol<'_, AuxDataCallback> = library
            .get(b"aux_data_callback\0")
            .context("failed to load aux_data_callback function for procedural macro")?;
        let aux_data_callback = aux_data_callback.into_raw();
        Ok(VTableV0 {
            expand,
            free_result,
            aux_data_callback,
        })
    }
}

struct Plugin {
    #[allow(dead_code)]
    library: Library,
    vtable: VTableV0,
}

impl Plugin {
    unsafe fn try_new(library_path: Utf8PathBuf) -> Result<Plugin> {
        let library = Library::new(library_path)?;
        let vtable = VTableV0::try_new(&library)?;

        Ok(Plugin { library, vtable })
    }
}
