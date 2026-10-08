// HexDB Core Access: row filters and field masks
//
// A role's grant covers whole tessellations; `restrictions` narrow it. Per
// tessellation (or "*" for every tessellation the role is granted on), a
// restriction has:
//
//   filter  the documents the role can see and write (the filter language),
//           with `{"$user": "login"}`, `{"$user": "id"}`, `{"$user": "email"}`
//           or `{"$user": "attributes.<name>"}` standing for the caller's
//           values, e.g. {"region": {"$user": "attributes.region"}}. A missing
//           attribute matches nothing (never everything).
//   hide    fields the role can't see (dotted paths), e.g. ["salary", "ssn"].
//
// A user may hold several grants that allow an action on a tessellation. If
// any of them is unrestricted, access is unrestricted. Otherwise the user sees
// the documents any of their filters allows (the filters are combined with
// $or), and a field is hidden only if every one of those grants hides it.
//
// Reads: queries, counts and aggregations are narrowed to the filter;
// documents outside it read as not found; hidden fields are removed from
// every document returned (REST, GraphQL, the change feed, functions). A
// query, sort, group or aggregate on a hidden field is refused (otherwise
// values could be probed), and so is a `$text` search unless the text index
// covers no hidden field.
//
// Writes: a document must match the filter before (replace, patch, delete)
// and after (every write) the change; writes can't set or remove hidden
// fields, and a full replace keeps their stored values.
//
// Managing a tessellation (indexes, schemas, deletion) needs an unrestricted
// grant. Administrators are never restricted. Restrictions apply to user
// tessellations only.
//
// The caller is known for the duration of an API request (and while a
// schedule or trigger runs as its owner) through a task-local, so the engine
// can apply restrictions at its caller-facing entry points without every
// internal lookup having to know about them.

use crate::{
    auth::{Action, Principal},
    document::Document,
    engine::EngineError,
    filter::Filter,
};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::{collections::BTreeSet, future::Future, sync::Arc};

tokio::task_local! {
    static CALLER: Arc<Principal>;
    static TRIGGER: String;
}

/// Run `f` as the work of trigger `name`: its writes record the trigger as
/// their origin and don't fire triggers themselves.
pub async fn as_trigger<F: Future>(name: String, f: F) -> F::Output {
    TRIGGER.scope(name, f).await
}

/// The trigger whose run this is, if any.
pub fn current_trigger() -> Option<String> {
    TRIGGER.try_with(|t| t.clone()).ok()
}

/// Run `f` with `principal` as the caller whose restrictions apply.
pub async fn as_caller<F: Future>(principal: Arc<Principal>, f: F) -> F::Output {
    CALLER.scope(principal, f).await
}

/// The caller of the current request, if any (none: internal work).
pub fn caller() -> Option<Arc<Principal>> {
    CALLER.try_with(|p| p.clone()).ok()
}

/// One role's restriction on one tessellation.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Restriction {
    /// The documents the role can see and write (none: every document).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<Value>,
    /// Fields the role can't see or change.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hide: Vec<String>,
}

impl Restriction {
    /// Check a restriction when a role is saved.
    pub fn validate(&self, key: &str) -> Result<()> {
        if let Some(filter) = &self.filter {
            // Placeholders become sample values; the rest must parse.
            let sample = substitute_sample(filter);
            Filter::parse(&sample).map_err(|e| invalid(format!("restrictions.{}.filter: {:#}", key, e)))?;
        }
        for path in &self.hide {
            let path = path.trim();
            if path.is_empty() || path.starts_with('.') || path.ends_with('.') || path == "id" {
                return Err(invalid(format!("restrictions.{}.hide: '{}' isn't a field that can be hidden.", key, path)));
            }
        }
        if self.filter.is_none() && self.hide.is_empty() {
            return Err(invalid(format!("restrictions.{}: give a filter, fields to hide, or both.", key)));
        }
        Ok(())
    }
}

/// What a caller may see of one tessellation.
#[derive(Debug, Clone, Default)]
pub struct Scope {
    /// Documents visible to the caller; `None` is every document.
    pub filter: Option<Filter>,
    /// Fields removed from what the caller sees.
    pub hidden: Vec<String>,
}

impl Scope {
    /// No restriction.
    pub fn full() -> Scope {
        Scope::default()
    }

    pub fn is_full(&self) -> bool {
        self.filter.is_none() && self.hidden.is_empty()
    }

    /// True if the caller may see (and write) this document.
    pub fn allows(&self, doc: &Document) -> bool {
        self.filter.as_ref().is_none_or(|f| f.matches(doc))
    }

    /// `filter` narrowed to what the caller may see.
    pub fn narrow(&self, filter: &Filter) -> Filter {
        match &self.filter {
            Some(own) => Filter::and(own.clone(), filter.clone()),
            None => filter.clone(),
        }
    }

    /// The document without the hidden fields.
    pub fn mask(&self, mut doc: Document) -> Document {
        if self.hidden.is_empty() {
            return doc;
        }
        let mut json = doc.data_json();
        for path in &self.hidden {
            remove_path(&mut json, path);
        }
        doc.data = crate::document::infer_fields_from_json(&json);
        doc
    }

    /// A JSON document (as returned by the API) without the hidden fields.
    pub fn mask_json(&self, json: &mut Value) {
        for path in &self.hidden {
            remove_path(json, path);
        }
    }

    /// Copy the hidden fields' stored values from `old` into `doc` (a full
    /// replace by a caller who can't see them keeps them).
    pub fn restore_hidden(&self, doc: &mut Document, old: &Document) {
        if self.hidden.is_empty() {
            return;
        }
        let old_json = old.data_json();
        let mut json = doc.data_json();
        for path in &self.hidden {
            remove_path(&mut json, path);
            if let Some(value) = get_path(&old_json, path) {
                set_path(&mut json, path, value.clone());
            }
        }
        doc.data = crate::document::infer_fields_from_json(&json);
    }

    /// Refuse a write that sets or removes a hidden field.
    pub fn check_writable<'a>(&self, fields: impl IntoIterator<Item = &'a str>) -> Result<()> {
        for field in fields {
            if self.touches_hidden(field) {
                return Err(EngineError::Forbidden(format!("'{}' is hidden from your role, so it can't be written.", field)).into());
            }
        }
        Ok(())
    }

    /// True if `path` is hidden, or inside or around a hidden field.
    pub fn touches_hidden(&self, path: &str) -> bool {
        self.hidden.iter().any(|h| overlaps(h, path))
    }

    /// Refuse a query, sort, group or aggregate that uses a hidden field.
    pub fn check_fields<'a>(&self, paths: impl IntoIterator<Item = &'a str>) -> Result<()> {
        if self.hidden.is_empty() {
            return Ok(());
        }
        for path in paths {
            if self.touches_hidden(path) {
                return Err(EngineError::Forbidden(format!("'{}' is hidden from your role, so it can't be used in queries.", path)).into());
            }
        }
        Ok(())
    }
}

/// True if one dotted path is the other, or contains it.
fn overlaps(a: &str, b: &str) -> bool {
    a == b || a.strip_prefix(b).is_some_and(|rest| rest.starts_with('.')) || b.strip_prefix(a).is_some_and(|rest| rest.starts_with('.'))
}

fn remove_path(json: &mut Value, path: &str) {
    let mut parts = path.split('.').peekable();
    let mut current = json;
    while let Some(part) = parts.next() {
        if parts.peek().is_none() {
            if let Value::Object(map) = current {
                map.remove(part);
            }
            return;
        }
        match current {
            Value::Object(map) => match map.get_mut(part) {
                Some(next) => current = next,
                None => return,
            },
            _ => return,
        }
    }
}

fn get_path<'a>(json: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.').try_fold(json, |current, part| current.get(part))
}

fn set_path(json: &mut Value, path: &str, value: Value) {
    let parts: Vec<&str> = path.split('.').collect();
    let mut current = json;
    for (i, part) in parts.iter().enumerate() {
        let Value::Object(map) = current else { return };
        if i == parts.len() - 1 {
            map.insert(part.to_string(), value);
            return;
        }
        current = map.entry(part.to_string()).or_insert_with(|| Value::Object(Map::new()));
    }
}

fn invalid(message: String) -> anyhow::Error {
    EngineError::Invalid(message).into()
}

/// Replace `{"$user": ...}` placeholders with the caller's values. `None` if
/// a value is missing (the restriction then matches nothing).
fn substitute(value: &Value, principal: &Principal) -> Option<Value> {
    Some(match value {
        Value::Object(map) if map.len() == 1 && map.contains_key("$user") => {
            let key = map["$user"].as_str()?;
            match key {
                "login" => Value::String(principal.login.clone()),
                "id" => Value::String(principal.user_id.clone()),
                "email" => Value::String(principal.email_address.clone()),
                other => {
                    let name = other.strip_prefix("attributes.")?;
                    let mut current = principal.attributes.get(name.split('.').next()?)?;
                    for part in name.split('.').skip(1) {
                        current = current.get(part)?;
                    }
                    if current.is_null() {
                        return None;
                    }
                    current.clone()
                }
            }
        }
        Value::Object(map) => Value::Object(map.iter().map(|(k, v)| Some((k.clone(), substitute(v, principal)?))).collect::<Option<Map<_, _>>>()?),
        Value::Array(items) => Value::Array(items.iter().map(|v| substitute(v, principal)).collect::<Option<Vec<_>>>()?),
        other => other.clone(),
    })
}

/// Placeholders replaced by a sample string, for validation.
fn substitute_sample(value: &Value) -> Value {
    match value {
        Value::Object(map) if map.len() == 1 && map.contains_key("$user") => Value::String("sample".into()),
        Value::Object(map) => Value::Object(map.iter().map(|(k, v)| (k.clone(), substitute_sample(v))).collect()),
        Value::Array(items) => Value::Array(items.iter().map(substitute_sample).collect()),
        other => other.clone(),
    }
}

impl Principal {
    /// What this principal may see of `tessellation` for `action` (see the
    /// module comment). Grants that don't allow the action don't count; with
    /// none at all the scope is full, and the permission check refuses.
    pub fn scope(&self, tessellation: &str, action: Action) -> Scope {
        if self.is_admin() {
            return Scope::full();
        }
        let mut filters = Vec::new();
        let mut all_rows = false;
        let mut hidden: Option<BTreeSet<String>> = None;
        let mut any = false;
        for grant in &self.grants {
            if !grant.permissions.contains(&action) || !grant.tessellations.iter().any(|t| t == "*" || t == tessellation) {
                continue;
            }
            any = true;
            let Some(restriction) = grant.restrictions.get(tessellation).or_else(|| grant.restrictions.get("*")) else {
                return Scope::full();
            };
            match &restriction.filter {
                None => all_rows = true,
                Some(filter) => match substitute(filter, self).map(|v| Filter::parse(&v)) {
                    Some(Ok(f)) => filters.push(f),
                    // A missing attribute (or a filter that no longer parses) matches nothing.
                    _ => filters.push(Filter::none()),
                },
            }
            let fields: BTreeSet<String> = restriction.hide.iter().map(|h| h.trim().to_string()).collect();
            hidden = Some(match hidden {
                None => fields,
                Some(h) => h.intersection(&fields).cloned().collect(),
            });
        }
        if !any {
            return Scope::full();
        }
        Scope {
            filter: if all_rows { None } else { Some(Filter::any(filters)) },
            hidden: hidden.unwrap_or_default().into_iter().collect(),
        }
    }

    /// True if some grant allowing `action` on `tessellation` is unrestricted.
    pub fn unrestricted(&self, tessellation: &str, action: Action) -> bool {
        self.is_admin()
            || self.grants.iter().any(|g| {
                g.permissions.contains(&action)
                    && g.tessellations.iter().any(|t| t == "*" || t == tessellation)
                    && g.restrictions.get(tessellation).or_else(|| g.restrictions.get("*")).is_none()
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{Credential, RoleDefinitions, RoleRules};
    use crate::users::RoleGrant;
    use serde_json::json;

    fn principal(roles: Vec<(&str, Vec<Action>, Value)>, attributes: Value) -> Principal {
        let mut defs = RoleDefinitions::builtin();
        let mut grants = Vec::new();
        for (name, permissions, restrictions) in roles {
            defs.0.insert(name.into(), RoleRules { permissions, restrictions: serde_json::from_value(restrictions).unwrap() });
            grants.push(RoleGrant { name: name.into(), tessellations: vec!["orders".into(), "notes".into()] });
        }
        Principal::new("u1".into(), "ada".into(), "ada@example.com".into(), grants, Credential::ApiKey { key_id: "k".into() }, &defs)
            .with_attributes(attributes.as_object().cloned().unwrap_or_default())
    }

    fn doc(data: Value) -> Document {
        Document { id: ulid::Ulid::new(), tessellation: "orders".into(), data: crate::document::infer_fields_from_json(&data), ttl: None }
    }

    #[test]
    fn filters_use_the_callers_attributes_and_combine_across_grants() {
        let p = principal(
            vec![("regional", vec![Action::Read], json!({ "orders": { "filter": { "region": { "$user": "attributes.region" } }, "hide": ["cost", "margin"] } }))],
            json!({ "region": "EU" }),
        );
        let scope = p.scope("orders", Action::Read);
        assert!(scope.allows(&doc(json!({ "region": "EU" }))));
        assert!(!scope.allows(&doc(json!({ "region": "US" }))));
        assert_eq!(scope.hidden, vec!["cost", "margin"]);
        let masked = scope.mask(doc(json!({ "region": "EU", "cost": 3, "total": 9 })));
        assert_eq!(masked.data_json(), json!({ "region": "EU", "total": 9 }));
        assert!(scope.check_fields(["total"]).is_ok());
        assert!(scope.check_fields(["cost.amount"]).is_err());
        // Other tessellations the role covers aren't restricted by an "orders" rule.
        assert!(p.scope("notes", Action::Read).is_full());

        // A second, broader grant: the union of rows, the intersection of hidden fields.
        let p = principal(
            vec![
                ("regional", vec![Action::Read], json!({ "*": { "filter": { "region": { "$user": "attributes.region" } }, "hide": ["cost", "margin"] } })),
                ("auditors", vec![Action::Read], json!({ "*": { "filter": { "flagged": true }, "hide": ["margin"] } })),
            ],
            json!({ "region": "EU" }),
        );
        let scope = p.scope("notes", Action::Read);
        assert!(scope.allows(&doc(json!({ "region": "US", "flagged": true }))));
        assert!(!scope.allows(&doc(json!({ "region": "US" }))));
        assert_eq!(scope.hidden, vec!["margin"]);
    }

    #[test]
    fn a_missing_attribute_matches_nothing() {
        let p = principal(vec![("regional", vec![Action::Read], json!({ "*": { "filter": { "region": { "$user": "attributes.region" } } } }))], json!({}));
        let scope = p.scope("orders", Action::Read);
        assert!(!scope.allows(&doc(json!({ "region": null }))));
        assert!(!scope.allows(&doc(json!({}))));
    }

    #[test]
    fn an_unrestricted_grant_wins() {
        let p = principal(
            vec![
                ("regional", vec![Action::Read], json!({ "*": { "filter": { "region": "EU" } } })),
                ("everything", vec![Action::Read], json!({})),
            ],
            json!({}),
        );
        assert!(p.scope("orders", Action::Read).is_full());
        assert!(p.unrestricted("orders", Action::Read));
    }

    #[test]
    fn restrictions_are_validated() {
        assert!(Restriction { filter: Some(json!({ "owner": { "$user": "login" } })), hide: vec![] }.validate("orders").is_ok());
        assert!(Restriction { filter: Some(json!({ "n": { "$bogus": 1 } })), hide: vec![] }.validate("orders").is_err());
        assert!(Restriction { filter: None, hide: vec!["id".into()] }.validate("orders").is_err());
        assert!(Restriction::default().validate("orders").is_err());
    }
}
