use std::{fmt::Display, io::Write};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use turbo_tasks::{trace::TraceRawVcs, TaskInput, Vc};
use turbo_tasks_fs::{glob::Glob, rope::RopeBuilder, FileContent, FileSystem, VirtualFileSystem};
use turbopack_core::{
    asset::{Asset, AssetContent},
    chunk::{AsyncModuleInfo, ChunkItem, ChunkType, ChunkableModule, ChunkingContext},
    ident::AssetIdent,
    module::Module,
    reference::ModuleReferences,
};

use crate::{
    chunk::{
        EcmascriptChunkItem, EcmascriptChunkItemContent, EcmascriptChunkPlaceable,
        EcmascriptChunkType, EcmascriptChunkingContext, EcmascriptExports,
    },
    references::async_module::{AsyncModule, OptionAsyncModule},
    utils::StringifyJs,
    EcmascriptModuleContent,
};

#[turbo_tasks::function]
fn layer() -> Vc<String> {
    Vc::cell("external".to_string())
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Serialize, Deserialize, TraceRawVcs, TaskInput)]
pub enum CachedExternalType {
    CommonJs,
    EcmaScriptViaRequire,
    EcmaScriptViaImport,
}

impl Display for CachedExternalType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CachedExternalType::CommonJs => write!(f, "cjs"),
            CachedExternalType::EcmaScriptViaRequire => write!(f, "esm_require"),
            CachedExternalType::EcmaScriptViaImport => write!(f, "esm_import"),
        }
    }
}

#[turbo_tasks::value]
pub struct CachedExternalModule {
    pub request: String,
    pub external_type: CachedExternalType,
}

#[turbo_tasks::value_impl]
impl CachedExternalModule {
    #[turbo_tasks::function]
    pub fn new(request: String, external_type: CachedExternalType) -> Vc<Self> {
        Self::cell(CachedExternalModule {
            request,
            external_type,
        })
    }

    #[turbo_tasks::function]
    pub fn content(&self) -> Result<Vc<EcmascriptModuleContent>> {
        let mut code = RopeBuilder::default();

        if self.external_type == CachedExternalType::EcmaScriptViaImport {
            writeln!(
                code,
                "const mod = await __turbopack_external_import__({});",
                StringifyJs(&self.request)
            )?;
        } else {
            writeln!(
                code,
                "const mod = __turbopack_external_require__({});",
                StringifyJs(&self.request)
            )?;
        }

        writeln!(code)?;

        if self.external_type == CachedExternalType::CommonJs {
            writeln!(code, "module.exports = mod;")?;
        } else {
            writeln!(code, "__turbopack_dynamic__(mod);")?;
        }

        Ok(EcmascriptModuleContent {
            inner_code: code.build(),
            source_map: None,
            is_esm: true,
        }
        .cell())
    }
}

#[turbo_tasks::value_impl]
impl Module for CachedExternalModule {
    #[turbo_tasks::function]
    fn ident(&self) -> Vc<AssetIdent> {
        let fs = VirtualFileSystem::new_with_name("externals".to_string());

        AssetIdent::from_path(fs.root())
            .with_layer(layer())
            .with_modifier(Vc::cell(self.request.clone()))
            .with_modifier(Vc::cell(self.external_type.to_string()))
    }
}

#[turbo_tasks::value_impl]
impl Asset for CachedExternalModule {
    #[turbo_tasks::function]
    fn content(&self) -> Vc<AssetContent> {
        AssetContent::file(FileContent::NotFound.cell())
    }
}

#[turbo_tasks::value_impl]
impl ChunkableModule for CachedExternalModule {
    #[turbo_tasks::function]
    async fn as_chunk_item(
        self: Vc<Self>,
        chunking_context: Vc<Box<dyn ChunkingContext>>,
    ) -> Result<Vc<Box<dyn ChunkItem>>> {
        let chunking_context =
            Vc::try_resolve_downcast::<Box<dyn EcmascriptChunkingContext>>(chunking_context)
                .await?
                .context(
                    "chunking context must impl EcmascriptChunkingContext to use \
                     WebAssemblyModuleAsset",
                )?;

        Ok(Vc::upcast(
            CachedExternalModuleChunkItem {
                module: self,
                chunking_context,
            }
            .cell(),
        ))
    }
}

#[turbo_tasks::value_impl]
impl EcmascriptChunkPlaceable for CachedExternalModule {
    #[turbo_tasks::function]
    fn get_exports(&self) -> Vc<EcmascriptExports> {
        if self.external_type == CachedExternalType::CommonJs {
            EcmascriptExports::CommonJs.cell()
        } else {
            EcmascriptExports::DynamicNamespace.cell()
        }
    }

    #[turbo_tasks::function]
    fn get_async_module(&self) -> Vc<OptionAsyncModule> {
        Vc::cell(
            if self.external_type == CachedExternalType::EcmaScriptViaImport {
                Some(
                    AsyncModule {
                        has_top_level_await: true,
                        import_externals: true,
                    }
                    .cell(),
                )
            } else {
                None
            },
        )
    }

    #[turbo_tasks::function]
    fn is_marked_as_side_effect_free(
        self: Vc<Self>,
        _side_effect_free_packages: Vc<Glob>,
    ) -> Vc<bool> {
        Vc::cell(false)
    }
}

#[turbo_tasks::value]
pub struct CachedExternalModuleChunkItem {
    module: Vc<CachedExternalModule>,
    chunking_context: Vc<Box<dyn EcmascriptChunkingContext>>,
}

#[turbo_tasks::value_impl]
impl ChunkItem for CachedExternalModuleChunkItem {
    #[turbo_tasks::function]
    fn asset_ident(&self) -> Vc<AssetIdent> {
        self.module.ident()
    }

    #[turbo_tasks::function]
    fn references(&self) -> Vc<ModuleReferences> {
        self.module.references()
    }

    #[turbo_tasks::function]
    fn ty(self: Vc<Self>) -> Vc<Box<dyn ChunkType>> {
        Vc::upcast(Vc::<EcmascriptChunkType>::default())
    }

    #[turbo_tasks::function]
    fn module(&self) -> Vc<Box<dyn Module>> {
        Vc::upcast(self.module)
    }

    #[turbo_tasks::function]
    fn chunking_context(&self) -> Vc<Box<dyn ChunkingContext>> {
        Vc::upcast(self.chunking_context)
    }

    #[turbo_tasks::function]
    fn is_self_async(&self) -> Vc<bool> {
        Vc::cell(true)
    }
}

#[turbo_tasks::value_impl]
impl EcmascriptChunkItem for CachedExternalModuleChunkItem {
    #[turbo_tasks::function]
    fn chunking_context(&self) -> Vc<Box<dyn EcmascriptChunkingContext>> {
        self.chunking_context
    }

    #[turbo_tasks::function]
    fn content(self: Vc<Self>) -> Vc<EcmascriptChunkItemContent> {
        panic!("content() should not be called");
    }

    #[turbo_tasks::function]
    async fn content_with_async_module_info(
        &self,
        async_module_info: Option<Vc<AsyncModuleInfo>>,
    ) -> Result<Vc<EcmascriptChunkItemContent>> {
        let async_module_options = self
            .module
            .get_async_module()
            .module_options(async_module_info);

        Ok(EcmascriptChunkItemContent::new(
            self.module.content(),
            self.chunking_context,
            async_module_options,
        ))
    }
}
