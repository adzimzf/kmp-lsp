//! Call and type hierarchy requests: the backend side of
//! [`crate::features::call_hierarchy`] and [`crate::features::type_hierarchy`].

use super::cursor::CursorContext;
use super::Backend;
use crate::features::call_hierarchy as calls;
use crate::features::definition as def;
use crate::features::type_hierarchy as types;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;

impl Backend {
    pub(super) async fn prepare_call_hierarchy_impl(
        &self,
        params: CallHierarchyPrepareParams,
    ) -> Result<Option<Vec<CallHierarchyItem>>> {
        let position_params = params.text_document_position_params;
        let uri = &position_params.text_document.uri;
        let position = position_params.position;
        self.indexer.ensure_indexed(uri);
        if let Some(item) = calls::prepare_call_hierarchy(&self.indexer, uri, position) {
            return Ok(Some(vec![item]));
        }
        // From a call site: the function the name there resolves to.
        let item = self
            .definitions_at(uri, position)
            .await
            .iter()
            .find_map(|location| calls::callable_declared_at(&self.indexer, location));
        Ok(item.map(|item| vec![item]))
    }

    pub(super) async fn incoming_calls_impl(
        &self,
        params: CallHierarchyIncomingCallsParams,
    ) -> Result<Option<Vec<CallHierarchyIncomingCall>>> {
        let item = params.item;
        let references = self
            .reference_locations(&item.uri, item.selection_range.start, false)
            .await;
        let incoming = calls::incoming_calls(&self.indexer, &item, references);
        Ok(Some(incoming))
    }

    pub(super) async fn outgoing_calls_impl(
        &self,
        params: CallHierarchyOutgoingCallsParams,
    ) -> Result<Option<Vec<CallHierarchyOutgoingCall>>> {
        let item = params.item;
        let mut resolved = Vec::new();
        for site in calls::call_sites(&self.indexer, &item) {
            let callee = self
                .definitions_at(&item.uri, site.start)
                .await
                .iter()
                .find_map(|location| calls::callable_declared_at(&self.indexer, location));
            if let Some(callee) = callee {
                resolved.push((site, callee));
            }
        }
        Ok(Some(calls::outgoing_calls(resolved)))
    }

    pub(super) async fn prepare_type_hierarchy_impl(
        &self,
        params: TypeHierarchyPrepareParams,
    ) -> Result<Option<Vec<TypeHierarchyItem>>> {
        let position_params = params.text_document_position_params;
        let uri = &position_params.text_document.uri;
        self.indexer.ensure_indexed(uri);
        Ok(types::prepare_type_hierarchy(
            &self.indexer,
            uri,
            position_params.position,
        ))
    }

    pub(super) async fn supertypes_impl(
        &self,
        params: TypeHierarchySupertypesParams,
    ) -> Result<Option<Vec<TypeHierarchyItem>>> {
        Ok(Some(types::supertypes(&self.indexer, &params.item)))
    }

    pub(super) async fn subtypes_impl(
        &self,
        params: TypeHierarchySubtypesParams,
    ) -> Result<Option<Vec<TypeHierarchyItem>>> {
        Ok(Some(types::subtypes(&self.indexer, &params.item)))
    }

    /// Where the name at `position` in `uri` is declared.
    async fn definitions_at(&self, uri: &Url, position: Position) -> Vec<Location> {
        let Some(ctx) = CursorContext::build(&self.indexer, uri, position) else {
            return Vec::new();
        };
        match def::find_definition(&ctx, &self.indexer, uri, position).await {
            Some(GotoDefinitionResponse::Scalar(location)) => vec![location],
            Some(GotoDefinitionResponse::Array(locations)) => locations,
            Some(GotoDefinitionResponse::Link(links)) => links
                .into_iter()
                .map(|link| Location::new(link.target_uri, link.target_selection_range))
                .collect(),
            None => Vec::new(),
        }
    }
}
