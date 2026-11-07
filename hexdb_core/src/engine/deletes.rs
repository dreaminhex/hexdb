// HexDB Core Engine: parent links, cascading deletes, the document trash
//
// A schema field can be a parent link (see `crate::schema::ParentLink`): it
// holds the ID of the document or file that owns this one, and says what
// happens to this document when its parent is deleted. Links mark ownership
// only. HexDB stays a document store: there are no joins over them, and writes
// don't check that a parent exists.
//
// Each link field gets a field index (unless one already starts with it), so
// finding a document's children is an index lookup, not a scan.

use super::{EngineError, HexDBEngine};
use crate::{
    index::{IndexDef, IndexKind},
    schema::{OnDelete, ParentLink, SchemaInput},
};
use anyhow::Result;
use serde::Serialize;
use tracing::warn;

/// A link from a child tessellation's field to a parent tessellation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChildLink {
    /// The tessellation whose documents hold the link.
    pub tessellation: String,
    pub field: String,
    pub on_delete: OnDelete,
}

impl HexDBEngine {
    /// `tess`'s parent links in its current schema version: (field, link).
    pub fn parent_links(&self, tess: &str) -> Vec<(String, ParentLink)> {
        self.schemas(tess)
            .last()
            .map(|v| v.links().map(|(f, l)| (f.clone(), l.clone())).collect())
            .unwrap_or_default()
    }

    /// Every link pointing at `parent` (its own, for trees, included).
    pub fn child_links(&self, parent: &str) -> Vec<ChildLink> {
        let catalog = self.catalog.lock().unwrap();
        let mut out = Vec::new();
        for (tess, info) in &catalog.tessellations {
            let Some(current) = info.schemas.last() else { continue };
            for (field, link) in current.links() {
                if link.tessellation == parent {
                    out.push(ChildLink { tessellation: tess.clone(), field: field.clone(), on_delete: link.on_delete });
                }
            }
        }
        out
    }

    /// Refuse links to tessellations that don't exist or hold no user data.
    pub(crate) fn check_links(&self, tess: &str, input: &SchemaInput) -> Result<()> {
        for (field, rule) in &input.fields {
            let Some(link) = &rule.parent else { continue };
            if link.tessellation == tess {
                continue;
            }
            match self.tessellation_info(&link.tessellation) {
                Some(info) if info.is_user_data() => {}
                Some(_) => return Err(EngineError::Invalid(format!("{}: '{}' is a system tessellation; it can't be a parent.", field, link.tessellation)).into()),
                None => return Err(EngineError::Invalid(format!("{}: the parent tessellation '{}' doesn't exist.", field, link.tessellation)).into()),
            }
        }
        Ok(())
    }

    /// Give each of `tess`'s link fields an index, unless one already starts with it.
    pub(crate) async fn ensure_link_indexes(&self, tess: &str) {
        for (field, _) in self.parent_links(tess) {
            let covered = self
                .list_indexes(tess)
                .iter()
                .any(|i| i.def.kind == IndexKind::Field && i.def.fields.first() == Some(&field));
            if covered {
                continue;
            }
            let def = IndexDef { fields: vec![field.clone()], ..Default::default() };
            if let Err(e) = self.create_index(tess, def).await {
                warn!("⚠️ Couldn't index the parent link '{}' of '{}': {:#}", field, tess, e);
            }
        }
    }

    /// Refuse to drop a tessellation other tessellations link to as a parent.
    pub(crate) fn check_not_parent(&self, tess: &str) -> Result<()> {
        let others: Vec<String> = self
            .child_links(tess)
            .into_iter()
            .filter(|l| l.tessellation != tess)
            .map(|l| format!("{}.{}", l.tessellation, l.field))
            .collect();
        if others.is_empty() {
            return Ok(());
        }
        Err(EngineError::Conflict(format!(
            "'{}' is the parent of {} (parent links in their schemas). Remove those links first.",
            tess,
            others.join(", ")
        ))
        .into())
    }
}

#[cfg(test)]
mod tests {
    use crate::schema::{new_version, OnDelete, SchemaInput};
    use serde_json::json;

    fn input(v: serde_json::Value) -> SchemaInput {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn link_rules() {
        let ok = new_version(&[], input(json!({ "fields": { "order_id": { "type": "string", "required": true, "parent": { "tessellation": "orders", "on_delete": "cascade" } } } }))).unwrap();
        let links: Vec<_> = ok.links().collect();
        assert_eq!(links.len(), 1);
        assert_eq!((links[0].0.as_str(), links[0].1.on_delete), ("order_id", OnDelete::Cascade));

        let refused = |rule: serde_json::Value| new_version(&[], input(json!({ "fields": { "p": rule } }))).unwrap_err().to_string();
        assert!(refused(json!({ "type": "integer", "parent": { "tessellation": "orders", "on_delete": "cascade" } })).contains("string field"));
        assert!(refused(json!({ "type": "string", "tokenize": "one_way", "parent": { "tessellation": "orders", "on_delete": "cascade" } })).contains("tokenized"));
        assert!(refused(json!({ "type": "string", "required": true, "parent": { "tessellation": "orders", "on_delete": "detach" } })).contains("nullable"));
        assert!(refused(json!({ "type": "string", "parent": { "tessellation": "_users", "on_delete": "cascade" } })).contains("parent tessellation"));
        // on_delete is required: no silent default for what a delete does.
        assert!(serde_json::from_value::<SchemaInput>(json!({ "fields": { "p": { "parent": { "tessellation": "orders" } } } })).is_err());
        // Detach on a nullable required field is fine.
        new_version(&[], input(json!({ "fields": { "p": { "type": "string", "required": true, "nullable": true, "parent": { "tessellation": "orders", "on_delete": "detach" } } } }))).unwrap();
    }
}
