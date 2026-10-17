//! 集成测试共用：按 `schema/protocol.json` 校验响应 / 通知的形状。

#![allow(dead_code)]

pub mod wt;

use std::collections::BTreeMap;

use jsonschema::Validator;
use serde_json::{json, Value};

/// `schema/protocol.json` 的 `definitions`，以及按类型名编译好的校验器。
pub struct Schemas {
    defs: Value,
    validators: BTreeMap<String, Validator>,
}

impl Schemas {
    pub fn load() -> Schemas {
        let schema: Value =
            serde_json::from_str(include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../schema/protocol.json")))
                .expect("schema/protocol.json is valid JSON");
        let defs = schema["definitions"].clone();
        assert!(defs.as_object().is_some_and(|d| d.len() > 50), "the schema has its definitions");
        Schemas { defs, validators: BTreeMap::new() }
    }

    /// `definitions[ty]` 的校验器（第一次用到时编译）。
    pub fn validator(&mut self, ty: &str, what: &str) -> &Validator {
        if !self.validators.contains_key(ty) {
            assert!(self.defs.get(ty).is_some(), "{what}: the registry names a type that is not in the schema: {ty}");
            // `$ref` 指到 `#/definitions/<类型>`；整份 definitions 带在同一个文档里，其它类型之间的 `$ref` 也解析得到
            let doc = json!({
                "$schema": "http://json-schema.org/draft-07/schema#",
                "$ref": format!("#/definitions/{ty}"),
                "definitions": self.defs,
            });
            let v = jsonschema::draft7::new(&doc).unwrap_or_else(|e| panic!("cannot compile the schema of {ty}: {e}"));
            self.validators.insert(ty.to_string(), v);
        }
        &self.validators[ty]
    }

    /// 校验 `value` 符合 `definitions[ty]`，不符合就带上所有不符之处 panic。
    pub fn check(&mut self, ty: &str, value: &Value, what: &str) {
        let errors: Vec<String> =
            self.validator(ty, what).iter_errors(value).map(|e| format!("{} (at {})", e, e.instance_path())).collect();
        assert!(errors.is_empty(), "{what} does not match {ty}:\n  {}\nvalue: {value}", errors.join("\n  "));
    }
}
