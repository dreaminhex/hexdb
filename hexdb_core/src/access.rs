// HexDB Core Access: row filters, hidden fields and field masks
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
//   mask    fields the role sees masked, with at most the first 6 and last 4
//           characters left, e.g. {"card": {"show_last": 4}} reads as
//           "************1234" (PCI DSS 3.4). Like hidden fields, they can't
//           be queried on or written.
//   folders for files tessellations: the folders (and their subfolders) the
//           role's files must be in, e.g. ["/inbox/", "/shared/2026/"]. The
//           same as a filter on `path`; with a filter too, both must match.
//
// Restrictions belong to a role, and a role's permissions share them, so
// "upload only into /inbox/" is a role with `upload` restricted to that
// folder, granted beside a role that reads everything.
//
// A user may hold several grants that allow an action on a tessellation. If
// any of them is unrestricted, access is unrestricted. Otherwise the user sees
// the documents any of their filters allows (the filters are combined with
// $or), and a field is hidden only if every one of those grants hides it. A
// field every grant hides or masks, and at least one masks, is masked, showing
// as many characters as the most revealing of those masks.
//
// Reads: queries, counts and aggregations are narrowed to the filter;
// documents outside it read as not found; hidden fields are removed from
// every document returned (REST, GraphQL, SQL, the change feed, functions),
// and masked ones replaced. A query, sort, group or aggregate on a hidden or
// masked field is refused (otherwise values could be probed), and so is a
// `$text` search unless the text index covers no such field.
//
// Writes: a document must match the filter before (replace, patch, delete)
// and after (every write) the change; writes can't set or remove hidden or
// masked fields, and a full replace keeps their stored values.
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
use std::{
    collections::BTreeMap,
    future::Future,
    sync::Arc,
};

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
    /// Fields the role sees masked (`************1234`) and can't change or query on.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub mask: BTreeMap<String, FieldMask>,
    /// Files tessellations: folders (with their subfolders) the role's files must be in.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub folders: Vec<String>,
}

/// How much of a masked field stays readable. The defaults mask every character.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FieldMask {
    /// Leading characters left as they are (at most 6, PCI DSS's limit for card numbers).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub show_first: u8,
    /// Trailing characters left as they are (at most 4, PCI DSS's limit for card numbers).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub show_last: u8,
}

fn is_zero(n: &u8) -> bool {
    *n == 0
}

impl FieldMask {
    pub const MAX_FIRST: u8 = 6;
    pub const MAX_LAST: u8 = 4;
    /// What a masked object, array or boolean reads as: no hint of its shape or size.
    const OPAQUE: &str = "********";
    /// Fewest characters left masked; a value too short for that is masked whole.
    const MIN_MASKED: usize = 4;

    /// The value as the role sees it: strings and numbers keep their length, with every character
    /// but the shown ones replaced by `*`. A value that would keep fewer than 4 characters masked is
    /// masked whole. Null stays null.
    pub fn apply(&self, value: &Value) -> Value {
        let text = match value {
            Value::Null => return Value::Null,
            Value::String(s) => s.clone(),
            Value::Number(n) => n.to_string(),
            _ => return Value::String(Self::OPAQUE.into()),
        };
        let chars: Vec<char> = text.chars().collect();
        let (first, last) = (self.show_first as usize, self.show_last as usize);
        let shown = if chars.len() < first + last + Self::MIN_MASKED { (0, 0) } else { (first, last) };
        let masked: String = chars
            .iter()
            .enumerate()
            .map(|(i, c)| if i < shown.0 || i >= chars.len() - shown.1 { *c } else { '*' })
            .collect();
        Value::String(masked)
    }

    /// The more revealing of two masks (a field masked by two grants shows what either allows).
    fn wider(self, other: FieldMask) -> FieldMask {
        FieldMask { show_first: self.show_first.max(other.show_first), show_last: self.show_last.max(other.show_last) }
    }
}

impl Restriction {
    /// Check a restriction when a role is saved.
    pub fn validate(&self, key: &str) -> Result<()> {
        if let Some(filter) = &self.filter {
            // Placeholders become sample values; the rest must parse.
            let sample = substitute_sample(filter);
            Filter::parse(&sample).map_err(|e| invalid(format!("restrictions.{}.filter: {:#}", key, e)))?;
        }
        let field = |path: &str| !(path.is_empty() || path.starts_with('.') || path.ends_with('.') || path == "id");
        for path in &self.hide {
            let path = path.trim();
            if !field(path) {
                return Err(invalid(format!("restrictions.{}.hide: '{}' isn't a field that can be hidden.", key, path)));
            }
        }
        for (path, mask) in &self.mask {
            let path = path.trim();
            if !field(path) {
                return Err(invalid(format!("restrictions.{}.mask: '{}' isn't a field that can be masked.", key, path)));
            }
            if self.hide.iter().any(|h| overlaps(h.trim(), path)) {
                return Err(invalid(format!("restrictions.{}.mask: '{}' is also hidden; give it one or the other.", key, path)));
            }
            if mask.show_first > FieldMask::MAX_FIRST || mask.show_last > FieldMask::MAX_LAST {
                return Err(invalid(format!(
                    "restrictions.{}.mask.{}: show at most the first {} and last {} characters.",
                    key,
                    path,
                    FieldMask::MAX_FIRST,
                    FieldMask::MAX_LAST
                )));
            }
        }
        for folder in &self.folders {
            crate::files::normalize_path(folder).map_err(|e| invalid(format!("restrictions.{}.folders: '{}': {:#}", key, folder, e)))?;
        }
        if self.filter.is_none() && self.hide.is_empty() && self.mask.is_empty() && self.folders.is_empty() {
            return Err(invalid(format!("restrictions.{}: give a filter, fields to hide or mask, folders, or a combination.", key)));
        }
        Ok(())
    }

    /// The row filter with `folders` folded in (none: every document).
    pub fn effective_filter(&self) -> Option<Value> {
        let folders: Option<Value> = (!self.folders.is_empty()).then(|| {
            let mut prefixes: Vec<Value> = self
                .folders
                .iter()
                // A folder that no longer normalizes matches nothing rather than everything.
                .map(|f| crate::files::normalize_path(f).unwrap_or_else(|_| "\u{0}".into()))
                .map(|f| serde_json::json!({ "path": { "$startsWith": f } }))
                .collect();
            if prefixes.len() == 1 {
                prefixes.remove(0)
            } else {
                serde_json::json!({ "$or": prefixes })
            }
        });
        match (&self.filter, folders) {
            (Some(filter), Some(folders)) => Some(serde_json::json!({ "$and": [filter, folders] })),
            (Some(filter), None) => Some(filter.clone()),
            (None, folders) => folders,
        }
    }
}

/// What a caller may see of one tessellation.
#[derive(Debug, Clone, Default)]
pub struct Scope {
    /// Documents visible to the caller; `None` is every document.
    pub filter: Option<Filter>,
    /// Fields removed from what the caller sees.
    pub hidden: Vec<String>,
    /// Fields the caller sees masked.
    pub masked: Vec<(String, FieldMask)>,
}

impl Scope {
    /// No restriction.
    pub fn full() -> Scope {
        Scope::default()
    }

    pub fn is_full(&self) -> bool {
        self.filter.is_none() && !self.protects_fields()
    }

    /// True if some fields are hidden or masked.
    pub fn protects_fields(&self) -> bool {
        !self.hidden.is_empty() || !self.masked.is_empty()
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

    /// The document as the caller sees it: hidden fields removed, masked ones masked.
    pub fn mask(&self, mut doc: Document) -> Document {
        if !self.protects_fields() {
            return doc;
        }
        let mut json = doc.data_json();
        self.mask_json(&mut json);
        doc.data = crate::document::infer_fields_from_json(&json);
        doc
    }

    /// A JSON document (as returned by the API) as the caller sees it.
    pub fn mask_json(&self, json: &mut Value) {
        for path in &self.hidden {
            remove_path(json, path);
        }
        for (path, mask) in &self.masked {
            if let Some(value) = get_path_mut(json, path) {
                *value = mask.apply(value);
            }
        }
    }

    /// The mask on a SQL column or top-level field, if it's masked.
    pub fn mask_of(&self, path: &str) -> Option<FieldMask> {
        self.masked.iter().find(|(m, _)| m == path).map(|(_, mask)| *mask)
    }

    /// Hidden and masked fields together: what the caller can't query on or write.
    fn protected(&self) -> impl Iterator<Item = (&str, &'static str)> {
        self.hidden.iter().map(|h| (h.as_str(), "hidden from")).chain(self.masked.iter().map(|(m, _)| (m.as_str(), "masked for")))
    }

    /// Copy the hidden and masked fields' stored values from `old` into `doc`
    /// (a full replace by a caller who can't see them keeps them).
    pub fn restore_hidden(&self, doc: &mut Document, old: &Document) {
        if !self.protects_fields() {
            return;
        }
        let old_json = old.data_json();
        let mut json = doc.data_json();
        for (path, _) in self.protected() {
            remove_path(&mut json, path);
            if let Some(value) = get_path(&old_json, path) {
                set_path(&mut json, path, value.clone());
            }
        }
        doc.data = crate::document::infer_fields_from_json(&json);
    }

    /// Refuse a write that sets or removes a hidden or masked field.
    pub fn check_writable<'a>(&self, fields: impl IntoIterator<Item = &'a str>) -> Result<()> {
        for field in fields {
            if let Some(how) = self.touching(field) {
                return Err(EngineError::Forbidden(format!("'{}' is {} your role, so it can't be written.", field, how)).into());
            }
        }
        Ok(())
    }

    /// Refuse a full replace (`submitted`, the whole new document) of `old` that would change a
    /// hidden or masked field. A masked field sent back exactly as the caller was shown it counts
    /// as unchanged, so a document can be read, edited and written back; [`Scope::restore_hidden`]
    /// then keeps the stored value.
    pub fn check_replace(&self, submitted: &Value, old: Option<&Document>) -> Result<()> {
        if !self.protects_fields() {
            return Ok(());
        }
        if let Value::Object(map) = submitted {
            for key in map.keys() {
                if self.hidden.iter().any(|h| overlaps(h, key)) {
                    return Err(EngineError::Forbidden(format!("'{}' is hidden from your role, so it can't be written.", key)).into());
                }
            }
        }
        let old_json = old.map(|d| d.data_json());
        for (path, mask) in &self.masked {
            let refuse = || EngineError::Forbidden(format!("'{}' is masked for your role, so it can't be written.", path));
            if let Some(sent) = get_path(submitted, path) {
                let shown = old_json.as_ref().and_then(|o| get_path(o, path)).map(|v| mask.apply(v));
                if shown.as_ref() != Some(sent) {
                    return Err(refuse().into());
                }
            }
            // A parent that's no longer an object would drop the stored value.
            let parts: Vec<&str> = path.split('.').collect();
            for depth in 1..parts.len() {
                if get_path(submitted, &parts[..depth].join(".")).is_some_and(|v| !v.is_object()) {
                    return Err(refuse().into());
                }
            }
        }
        Ok(())
    }

    /// How `path` is protected ("hidden from", "masked for") if it is, or is inside or around such a field.
    fn touching(&self, path: &str) -> Option<&'static str> {
        self.protected().find(|(p, _)| overlaps(p, path)).map(|(_, how)| how)
    }

    /// Refuse a query, sort, group or aggregate that uses a hidden or masked field.
    pub fn check_fields<'a>(&self, paths: impl IntoIterator<Item = &'a str>) -> Result<()> {
        if !self.protects_fields() {
            return Ok(());
        }
        for path in paths {
            if let Some(how) = self.touching(path) {
                return Err(EngineError::Forbidden(format!("'{}' is {} your role, so it can't be used in queries.", path, how)).into());
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

fn get_path_mut<'a>(json: &'a mut Value, path: &str) -> Option<&'a mut Value> {
    path.split('.').try_fold(json, |current, part| current.get_mut(part))
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
        // Each field some grant hides or masks: None while hidden by all so far, Some(mask) once one masks it.
        let mut protected: Option<BTreeMap<String, Option<FieldMask>>> = None;
        let mut any = false;
        for grant in &self.grants {
            if !grant.permissions.contains(&action) || !grant.tessellations.iter().any(|t| t == "*" || t == tessellation) {
                continue;
            }
            any = true;
            let Some(restriction) = grant.restrictions.get(tessellation).or_else(|| grant.restrictions.get("*")) else {
                return Scope::full();
            };
            match &restriction.effective_filter() {
                None => all_rows = true,
                Some(filter) => match substitute(filter, self).map(|v| Filter::parse(&v)) {
                    Some(Ok(f)) => filters.push(f),
                    // A missing attribute (or a filter that no longer parses) matches nothing.
                    _ => filters.push(Filter::none()),
                },
            }
            let fields: BTreeMap<String, Option<FieldMask>> = restriction
                .hide
                .iter()
                .map(|h| (h.trim().to_string(), None))
                .chain(restriction.mask.iter().map(|(m, mask)| (m.trim().to_string(), Some(*mask))))
                .collect();
            // A field stays protected only if every grant protects it, and is masked if any grant masks it.
            protected = Some(match protected {
                None => fields,
                Some(so_far) => so_far
                    .into_iter()
                    .filter_map(|(path, a)| {
                        let b = *fields.get(&path)?;
                        Some((path, match (a, b) {
                            (Some(a), Some(b)) => Some(a.wider(b)),
                            (a, b) => a.or(b),
                        }))
                    })
                    .collect(),
            });
        }
        if !any {
            return Scope::full();
        }
        let (mut hidden, mut masked) = (Vec::new(), Vec::new());
        for (path, mask) in protected.unwrap_or_default() {
            match mask {
                None => hidden.push(path),
                Some(mask) => masked.push((path, mask)),
            }
        }
        Scope { filter: if all_rows { None } else { Some(Filter::any(filters)) }, hidden, masked }
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
        Document { id: ulid::Ulid::generate(), tessellation: "orders".into(), data: crate::document::infer_fields_from_json(&data), ttl: None }
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
    fn masks_keep_the_shown_characters_and_the_length() {
        let last4 = FieldMask { show_first: 0, show_last: 4 };
        assert_eq!(last4.apply(&json!("4111111111111111")), json!("************1111"));
        assert_eq!(FieldMask { show_first: 6, show_last: 4 }.apply(&json!("4111111111111111")), json!("411111******1111"));
        assert_eq!(FieldMask::default().apply(&json!("123-45-6789")), json!("***********"));
        assert_eq!(last4.apply(&json!("123456789")), json!("*****6789"));
        // Numbers become masked strings; too short to keep 4 characters masked, masked whole.
        assert_eq!(last4.apply(&json!(4111111111111111u64)), json!("************1111"));
        assert_eq!(last4.apply(&json!("1234567")), json!("*******"));
        assert_eq!(last4.apply(&json!("12345678")), json!("****5678"));
        // Objects, arrays and booleans don't reveal their shape; null stays null.
        assert_eq!(last4.apply(&json!({ "a": 1 })), json!("********"));
        assert_eq!(last4.apply(&json!(true)), json!("********"));
        assert_eq!(last4.apply(&Value::Null), Value::Null);
    }

    #[test]
    fn masked_fields_are_masked_on_read_and_refused_in_queries_and_writes() {
        let p = principal(
            vec![("clerk", vec![Action::Read, Action::Write], json!({ "orders": { "mask": { "card": { "show_last": 4 }, "billing.ssn": {} } } }))],
            json!({}),
        );
        let scope = p.scope("orders", Action::Read);
        let seen = scope.mask(doc(json!({ "card": "4111111111111111", "billing": { "ssn": "123456789", "zip": "02139" }, "total": 9 })));
        assert_eq!(seen.data_json(), json!({ "card": "************1111", "billing": { "ssn": "*********", "zip": "02139" }, "total": 9 }));
        assert!(scope.check_fields(["total", "billing.zip"]).is_ok());
        assert!(scope.check_fields(["card"]).is_err());
        assert!(scope.check_fields(["billing"]).is_err());
        let write = p.scope("orders", Action::Write);
        assert!(write.check_writable(["card"]).is_err());
        // A full replace keeps the stored value it couldn't see.
        let mut replaced = doc(json!({ "card": "************1111", "total": 10 }));
        write.restore_hidden(&mut replaced, &doc(json!({ "card": "4111111111111111", "total": 9 })));
        assert_eq!(replaced.data_json()["card"], json!("4111111111111111"));
    }

    #[test]
    fn grants_combine_hide_and_mask_toward_the_more_revealing() {
        let p = principal(
            vec![
                ("a", vec![Action::Read], json!({ "*": { "hide": ["card", "ssn"], "mask": { "phone": { "show_last": 2 } } } })),
                ("b", vec![Action::Read], json!({ "*": { "mask": { "card": { "show_last": 4 }, "phone": { "show_first": 3 } } } })),
            ],
            json!({}),
        );
        let scope = p.scope("orders", Action::Read);
        // ssn: hidden by one grant, visible to the other. card: hidden or masked, so masked. phone: both masks' characters.
        assert!(scope.hidden.is_empty());
        assert_eq!(
            scope.masked,
            vec![("card".to_string(), FieldMask { show_first: 0, show_last: 4 }), ("phone".to_string(), FieldMask { show_first: 3, show_last: 2 })]
        );
    }

    #[test]
    fn masks_are_validated() {
        let mask = |m: Value| Restriction { mask: serde_json::from_value(m).unwrap(), ..Default::default() };
        assert!(mask(json!({ "card": { "show_last": 4 } })).validate("orders").is_ok());
        assert!(mask(json!({ "card": { "show_last": 5 } })).validate("orders").is_err());
        assert!(mask(json!({ "card": { "show_first": 7 } })).validate("orders").is_err());
        assert!(mask(json!({ "id": {} })).validate("orders").is_err());
        let both = Restriction { hide: vec!["card".into()], ..mask(json!({ "card": {} })) };
        assert!(both.validate("orders").is_err());
        assert!(serde_json::from_value::<Restriction>(json!({ "mask": { "card": { "show_middle": 1 } } })).is_err());
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
        assert!(Restriction { filter: Some(json!({ "owner": { "$user": "login" } })), hide: vec![], ..Default::default() }.validate("orders").is_ok());
        assert!(Restriction { filter: Some(json!({ "n": { "$bogus": 1 } })), hide: vec![], ..Default::default() }.validate("orders").is_err());
        assert!(Restriction { filter: None, hide: vec!["id".into()], ..Default::default() }.validate("orders").is_err());
        assert!(Restriction::default().validate("orders").is_err());
        assert!(Restriction { folders: vec!["inbox".into()], ..Default::default() }.validate("media").is_ok());
        assert!(Restriction { folders: vec!["/a/../b".into()], ..Default::default() }.validate("media").is_err());
    }

    #[test]
    fn folders_limit_files_to_a_subtree_per_action() {
        let p = principal(
            vec![
                ("uploader", vec![Action::Upload], json!({ "*": { "folders": ["/inbox", "/shared/2026/"] } })),
                ("everyone", vec![Action::Read], json!({})),
            ],
            json!({}),
        );
        let upload = p.scope("orders", Action::Upload);
        assert!(upload.allows(&doc(json!({ "path": "/inbox/" }))));
        assert!(upload.allows(&doc(json!({ "path": "/shared/2026/june/" }))));
        assert!(!upload.allows(&doc(json!({ "path": "/inboxes/" }))), "a folder, not a name prefix");
        assert!(!upload.allows(&doc(json!({ "path": "/" }))));
        assert!(p.scope("orders", Action::Read).is_full(), "reads aren't narrowed by the upload role");

        let both = Restriction { filter: Some(json!({ "tags": "public" })), folders: vec!["/pub/".into()], ..Default::default() };
        assert_eq!(both.effective_filter(), Some(json!({ "$and": [{ "tags": "public" }, { "path": { "$startsWith": "/pub/" } }] })));
    }
}
