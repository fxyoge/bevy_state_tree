use std::io;

use bevy_asset::{AssetLoader, AsyncReadExt, LoadContext, io::Reader};
use bevy_reflect::TypePath;
use thiserror::Error;

use super::{CompileError, Diagnostic, compile, parse};
use crate::StateTreeDefinition;

#[derive(TypePath)]
pub(crate) struct ScxmlLoader;

#[derive(Debug, Error)]
pub(crate) enum ScxmlLoaderError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Parse(#[from] Diagnostic),
    #[error(transparent)]
    Compile(#[from] CompileError),
}

impl AssetLoader for ScxmlLoader {
    type Asset = StateTreeDefinition;
    type Settings = ();
    type Error = ScxmlLoaderError;

    async fn load(
        &self,
        reader: &mut dyn Reader,
        _settings: &Self::Settings,
        load_context: &mut LoadContext<'_>,
    ) -> Result<Self::Asset, Self::Error> {
        let mut source = String::new();
        reader.read_to_string(&mut source).await?;

        let source_name = load_context.path().to_string();
        let document = parse(&source_name, &source)?;
        Ok(compile(&document)?)
    }

    fn extensions(&self) -> &[&str] {
        &["scxml"]
    }
}
