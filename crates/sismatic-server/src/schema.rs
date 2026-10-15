//! The config document's key set, read off the document's own type.
//!
//! # The direction that was missing
//!
//! [`configuration::env_source`] already maps the environment onto
//! [`RawServerConfig`], and the mapping is a rule rather than a table: prefix,
//! separator, uppercase. `SISMATIC_SERVER__STORE__RETAIN` reaches `store.retain`
//! because the `config` crate takes the variable apart that way, not because
//! anything here lists it. Add a field to a `Raw*` struct and its variable works
//! on that same compile.
//!
//! But that mapping only runs one way — *name to key*. Nothing could ask the
//! type the opposite question, **which keys exist**, so every consumer that
//! needed the answer restated it by hand: the documentation table, the example
//! Kubernetes manifests, and one place where restating it was not merely
//! duplication but a latent bug (see below). This module supplies the missing
//! direction by walking the OpenAPI schema `utoipa` derives from the very same
//! structs `serde` derives from — the same trick, and the same justification, as
//! the API's own document: a description that cannot drift from the thing it
//! describes, because one `#[derive]` produces both.
//!
//! # The bug this removes
//!
//! `config::Environment` splits a variable on commas only for keys named in
//! advance by `with_list_parse_key`. That list was two literals:
//!
//! ```text
//! .with_list_parse_key("sync.fields")
//! .with_list_parse_key("defaults.fields")
//! ```
//!
//! Correct, and unchecked. A third `Vec`-typed key added to the document would
//! not be in it, and the failure is quiet in the worst way: the variable is
//! still accepted, still reaches the right field, and simply stops being a list
//! — the operator's `a,b` arrives as one element or as a type error, depending
//! on how `config` coerces it, and in neither case is it what they wrote.
//! Nothing failed to compile and no test could have noticed, because the fact
//! that a key is a list lived in two places that were never compared.
//!
//! [`list_keys`] is now that list, and it is a *consequence* of the field's
//! type: a `Vec<_>` is a `Schema::Array`, and every array-shaped leaf of the
//! walk is a list key. The literals are gone.
//!
//! # What a leaf is
//!
//! The walk descends `properties` and stops at anything without them. That rule
//! is what makes the four hand-written leaves work — [`RawDuration`],
//! [`RawBytes`], [`RawField`] and [`Retention`] each accept more than one
//! spelling (`5min` or `300`, `512MiB` or `unlimited`), so each hand-writes
//! `Deserialize`, and each therefore hand-writes [`PartialSchema`] beside it,
//! stating the same spellings the visitor's `expecting()` states in prose. They
//! declare their types and no properties, so the walk records them and goes no
//! deeper — which is right: `store.retain` is one variable, not a subtree.
//!
//! [`configuration::env_source`]: crate::configuration::env_source
//! [`RawServerConfig`]: crate::configuration::RawServerConfig
//! [`RawDuration`]: crate::configuration::RawDuration
//! [`RawBytes`]: crate::configuration::RawBytes
//! [`RawField`]: crate::configuration::RawField
//! [`Retention`]: crate::lifecycle::Retention

use std::collections::BTreeMap;

use serde::Serialize;
use utoipa::openapi::schema::{Schema, SchemaType, Type};
use utoipa::openapi::{Ref, RefOr};
use utoipa::{PartialSchema, ToSchema};

use crate::configuration::{ENV_LIST_SEPARATOR, ENV_PREFIX, ENV_SEPARATOR, RawServerConfig};

/// One setting of the config document, and the variable that writes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EnvKey {
    /// The dotted config key, exactly as the `config` crate addresses it and as
    /// `with_list_parse_key` spells it: `store.retain`.
    pub path: String,
    /// The environment variable, derived from [`path`](Self::path) by the same
    /// prefix and separator the loader takes it apart with.
    pub var: String,
    /// The JSON types this key accepts, in the order the schema states them —
    /// `["string", "integer"]` for a duration that may be written `5min` or
    /// `300`.
    pub types: Vec<&'static str>,
    /// Whether the value is a list, and so whether the environment splits it on
    /// [`ENV_LIST_SEPARATOR`]. The reason this module exists; see the module
    /// docs.
    pub list: bool,
    /// The field's doc comment, as `utoipa` carries it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// The whole environment surface, in the form the pipeline consumes.
///
/// Carries the three constants as data rather than leaving a reader to infer
/// them from the variable names: a generator that renders a `ConfigMap` needs
/// the list separator, and reading it out of here is what keeps the rendered
/// example and the parser agreeing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EnvSchema {
    pub prefix: &'static str,
    pub separator: &'static str,
    pub list_separator: &'static str,
    pub keys: Vec<EnvKey>,
}

impl EnvSchema {
    /// Reflect [`RawServerConfig`].
    #[must_use]
    pub fn of_server_config() -> Self {
        let mut defs = Vec::new();
        RawServerConfig::schemas(&mut defs);
        let defs: BTreeMap<String, RefOr<Schema>> = defs.into_iter().collect();

        let mut keys = Vec::new();
        Walk {
            defs: &defs,
            // Components already on the path. A config document is a tree, but
            // nothing in the *type* system says so, and a self-referential
            // section would otherwise walk until the stack ran out.
            active: Vec::new(),
        }
        .node(&RawServerConfig::schema(), &mut Vec::new(), &mut keys);

        keys.sort_by(|a, b| a.path.cmp(&b.path));
        Self {
            prefix: ENV_PREFIX,
            separator: ENV_SEPARATOR,
            list_separator: ENV_LIST_SEPARATOR,
            keys,
        }
    }
}

/// Every key whose value is a list, as `config` spells them.
///
/// The argument to `with_list_parse_key`, and the whole reason the reflection
/// is load-bearing rather than decorative — see the module docs.
#[must_use]
pub fn list_keys() -> Vec<String> {
    EnvSchema::of_server_config()
        .keys
        .into_iter()
        .filter(|k| k.list)
        .map(|k| k.path)
        .collect()
}

/// The recursive descent, carrying what it needs to resolve a `$ref` and to
/// refuse a cycle.
struct Walk<'a> {
    defs: &'a BTreeMap<String, RefOr<Schema>>,
    active: Vec<String>,
}

impl Walk<'_> {
    /// Visit one schema node at `path`, appending to `out`.
    fn node(&mut self, node: &RefOr<Schema>, path: &mut Vec<String>, out: &mut Vec<EnvKey>) {
        match node {
            RefOr::Ref(reference) => self.reference(reference, path, out),
            RefOr::T(schema) => self.schema(schema, path, out),
        }
    }

    /// Follow a `$ref` into the component it names.
    ///
    /// A dangling reference is skipped rather than panicked on: `schemas()`
    /// collects what the derive knows about, and a type that referred to
    /// something outside that set would otherwise take down a binary that was
    /// only asked to print its own help.
    fn reference(&mut self, reference: &Ref, path: &mut Vec<String>, out: &mut Vec<EnvKey>) {
        let Some(name) = reference.ref_location.rsplit('/').next() else {
            return;
        };
        let name = name.to_owned();
        if self.active.contains(&name) {
            return;
        }
        let Some(target) = self.defs.get(&name).cloned() else {
            return;
        };
        self.active.push(name);
        self.node(&target, path, out);
        self.active.pop();
    }

    /// Visit a resolved schema.
    ///
    /// `Option<T>` is the case worth naming: `utoipa` renders it as a composite
    /// (`AllOf`/`OneOf`/`AnyOf`) pairing the inner schema with null, or as the
    /// inner schema carrying a null type — and *every* key of this document is
    /// optional, because a config may state only what it overrides. So the
    /// composite arms are not an exotic branch here, they are the common one,
    /// and they are flattened rather than treated as a choice the operator
    /// makes.
    fn schema(&mut self, schema: &Schema, path: &mut Vec<String>, out: &mut Vec<EnvKey>) {
        match schema {
            Schema::Object(object) if !object.properties.is_empty() => {
                for (key, child) in &object.properties {
                    path.push(key.clone());
                    self.node(child, path, out);
                    path.pop();
                }
            }
            Schema::Object(object) => {
                out.push(leaf(
                    path,
                    types_of(&object.schema_type),
                    false,
                    object.description.clone(),
                ));
            }
            Schema::Array(array) => {
                out.push(leaf(path, vec!["array"], true, array.description.clone()));
            }
            Schema::OneOf(composite) => self.composite(&composite.items, path, out),
            Schema::AllOf(composite) => self.composite(&composite.items, path, out),
            Schema::AnyOf(composite) => self.composite(&composite.items, path, out),
            // `Schema` is `#[non_exhaustive]`. A variant added upstream is not
            // a key this document can hold until something here knows how to
            // walk it, and inventing a name for it would be worse than the
            // silence.
            _ => {}
        }
    }

    /// Flatten a composite into whichever of its arms carry structure.
    ///
    /// Each arm is visited at the *same* path, so the keys a `OneOf` produces
    /// are merged rather than nested. Null arms contribute nothing and fall out
    /// on their own: they are objects with no properties and a sole `null`
    /// type, which [`types_of`] drops, leaving a leaf that
    /// [`merge`] discards in favour of a real one at the same path.
    fn composite(
        &mut self,
        items: &[RefOr<Schema>],
        path: &mut Vec<String>,
        out: &mut Vec<EnvKey>,
    ) {
        let mut arms = Vec::new();
        for item in items {
            self.node(item, path, &mut arms);
        }
        merge(arms, out);
    }
}

/// Fold the arms of a composite into `out`, one entry per distinct path.
///
/// Two arms describing one key — the `string` arm and the `integer` arm of a
/// duration, say — are one variable with two accepted spellings, so their types
/// are unioned rather than duplicated. An arm with no types at all is the null
/// half of an `Option` and adds nothing.
fn merge(arms: Vec<EnvKey>, out: &mut Vec<EnvKey>) {
    for arm in arms {
        if let Some(existing) = out.iter_mut().find(|k| k.path == arm.path) {
            for ty in arm.types {
                if !existing.types.contains(&ty) {
                    existing.types.push(ty);
                }
            }
            existing.list |= arm.list;
            existing.description = existing.description.take().or(arm.description);
        } else if !arm.types.is_empty() || arm.list {
            out.push(arm);
        }
    }
}

/// Build the entry for a leaf reached at `path`.
fn leaf(
    path: &[String],
    types: Vec<&'static str>,
    list: bool,
    description: Option<String>,
) -> EnvKey {
    let path = path.join(".");
    let var = format!(
        "{ENV_PREFIX}{ENV_SEPARATOR}{}",
        path.to_uppercase().replace('.', ENV_SEPARATOR)
    );
    EnvKey {
        path,
        var,
        types,
        list,
        description,
    }
}

/// The JSON type names a schema states, with `null` dropped.
///
/// `null` is how optionality is spelled, and every key here is optional, so
/// carrying it would put `"null"` beside the type of every single variable and
/// tell a reader nothing.
fn types_of(schema_type: &SchemaType) -> Vec<&'static str> {
    fn name(ty: &Type) -> Option<&'static str> {
        match ty {
            Type::Object => Some("object"),
            Type::String => Some("string"),
            Type::Integer => Some("integer"),
            Type::Number => Some("number"),
            Type::Boolean => Some("boolean"),
            Type::Array => Some("array"),
            Type::Null => None,
        }
    }

    match schema_type {
        SchemaType::Type(ty) => name(ty).into_iter().collect(),
        SchemaType::Array(types) => types.iter().filter_map(name).collect(),
        SchemaType::AnyValue => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::configuration::{EnvSource, env_source, get_configuration_with_env};

    fn schema() -> EnvSchema {
        EnvSchema::of_server_config()
    }

    fn paths() -> Vec<String> {
        schema().keys.into_iter().map(|k| k.path).collect()
    }

    /// The walk reaches every section, including the two that are flat
    /// (`defaults`) and the ones behind an `Option`.
    #[test]
    fn every_section_of_the_document_is_reflected() {
        let paths = paths();
        for expected in [
            "defaults.interval_secs",
            "defaults.fields",
            "defaults.host",
            "defaults.port",
            "intent_relay.poll_ms",
            "intent_relay.max_attempts",
            "inventory.config_path",
            "inventory.runtime_config_path",
            "sync.interval_secs",
            "sync.fields",
            "store.retain",
            "store.cleanup_interval",
            "store.max_memory",
            "store.cleanup_on_remove",
            "http.host",
            "http.port",
        ] {
            assert!(
                paths.iter().any(|p| p == expected),
                "{expected} missing from the reflected schema; found {paths:?}"
            );
        }
    }

    /// Nothing beyond the document leaks in. A section that gained a key would
    /// fail the test above; a walk that invented one fails here.
    #[test]
    fn the_reflection_names_nothing_the_document_does_not_hold() {
        assert_eq!(paths().len(), 16, "unexpected key set: {:?}", paths());
    }

    /// The property the module exists for: list-ness is read off the field's
    /// type, and the two keys the hand-written list used to name are exactly
    /// the two the walk finds.
    #[test]
    fn list_keys_are_the_array_typed_fields() {
        assert_eq!(list_keys(), vec!["defaults.fields", "sync.fields"]);
    }

    /// A leaf that hand-writes `Deserialize` states its spellings in the schema
    /// too, so the documentation can say "`5min` or a number of seconds"
    /// without a human restating it.
    #[test]
    fn the_hand_written_leaves_declare_both_spellings() {
        let by_path: BTreeMap<_, _> = schema()
            .keys
            .into_iter()
            .map(|k| (k.path.clone(), k))
            .collect();

        for path in ["store.retain", "store.cleanup_interval", "store.max_memory"] {
            let key = &by_path[path];
            assert!(
                key.types.contains(&"string") && key.types.contains(&"integer"),
                "{path} should accept both spellings, got {:?}",
                key.types
            );
        }
    }

    /// The variable name is derived with the loader's own constants, so this
    /// pins the derivation rather than the spelling of any one key.
    #[test]
    fn variable_names_are_the_prefixed_upper_snake_path() {
        let schema = schema();
        for key in &schema.keys {
            let expected = format!(
                "{}{}{}",
                schema.prefix,
                schema.separator,
                key.path.to_uppercase().replace('.', schema.separator)
            );
            assert_eq!(key.var, expected);
        }
    }

    /// The reflection is not merely *shaped* like the loader's input — it is
    /// accepted by it. Every reflected scalar is set to a value of a type it
    /// declares, fed through the real `config::Environment`, and the document
    /// must deserialize. A key the walk named wrongly would be rejected here by
    /// `deny_unknown_fields`, which is the same mechanism that rejects an
    /// operator's typo.
    #[test]
    fn every_reflected_variable_is_accepted_by_the_loader() {
        let schema = schema();
        let vars: config::Map<String, String> = schema
            .keys
            .iter()
            .filter_map(|key| Some((key.var.clone(), sample(key)?)))
            .collect();

        assert!(
            vars.len() >= 10,
            "expected the sampler to cover most of the document, got {}",
            vars.len()
        );

        let dir = tempdir();
        let path = dir.join("configuration.yaml");
        std::fs::write(&path, "{}\n").expect("write config");

        let env: EnvSource = env_source().source(Some(vars));
        get_configuration_with_env(&path, env).expect("the reflected environment must load");
    }

    /// A value the key's own declared types accept. `None` for the keys whose
    /// spelling this test cannot invent safely — a path, which must not be
    /// written somewhere real, is the only such case.
    fn sample(key: &EnvKey) -> Option<String> {
        if key.path.ends_with("_path") {
            return None;
        }
        if key.list {
            return Some("RUNNING_STATE,MAC_ADDRESS".to_owned());
        }
        match key.path.as_str() {
            "store.retain" => Some("30d".to_owned()),
            "store.cleanup_interval" => Some("5min".to_owned()),
            "store.max_memory" => Some("512MiB".to_owned()),
            "http.host" | "defaults.host" => Some("0.0.0.0".to_owned()),
            _ if key.types.contains(&"integer") => Some("7".to_owned()),
            _ if key.types.contains(&"boolean") => Some("true".to_owned()),
            _ if key.types.contains(&"string") => Some("x".to_owned()),
            _ => None,
        }
    }

    /// A scratch directory for the one test that needs a file on disk.
    fn tempdir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "sismatic-schema-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }
}
