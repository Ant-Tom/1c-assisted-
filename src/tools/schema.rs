//! Минимальный валидатор JSON Schema для параметров функций.
//!
//! Поддерживается только то, что используют наши схемы: `type` (object, string,
//! integer, number, boolean), `properties`, `required`, `additionalProperties: false`,
//! `minLength`, `maxLength`, `enum`, `format: "date"` (YYYY-MM-DD).
//! Неизвестные ключевые слова игнорируются: схемы пишем сами, а модель видит те же схемы.

use chrono::NaiveDate;
use serde_json::{Map, Value};

/// Проверяет `value` по `schema`. Возвращает список понятных модели ошибок.
pub fn validate(schema: &Value, value: &Value) -> Result<(), Vec<String>> {
    let mut errors = Vec::new();
    check(schema, value, "", &mut errors);
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

fn label(path: &str) -> &str {
    if path.is_empty() {
        "аргументы"
    } else {
        path
    }
}

fn check(schema: &Value, value: &Value, path: &str, errors: &mut Vec<String>) {
    let Some(schema) = schema.as_object() else {
        return;
    };

    if let Some(expected) = schema.get("type").and_then(Value::as_str) {
        let ok = match expected {
            "object" => value.is_object(),
            "string" => value.is_string(),
            "integer" => value.is_i64() || value.is_u64(),
            "number" => value.is_number(),
            "boolean" => value.is_boolean(),
            "array" => value.is_array(),
            "null" => value.is_null(),
            _ => true,
        };
        if !ok {
            errors.push(format!(
                "{}: ожидается тип {expected}, получено {}",
                label(path),
                type_name(value)
            ));
            return;
        }
    }

    if let Some(allowed) = schema.get("enum").and_then(Value::as_array) {
        if !allowed.contains(value) {
            errors.push(format!(
                "{}: допустимые значения {}",
                label(path),
                Value::Array(allowed.clone())
            ));
        }
    }

    match value {
        Value::Object(obj) => check_object(schema, obj, path, errors),
        Value::String(s) => check_string(schema, s, path, errors),
        _ => {}
    }
}

fn check_object(
    schema: &Map<String, Value>,
    obj: &Map<String, Value>,
    path: &str,
    errors: &mut Vec<String>,
) {
    let properties = schema.get("properties").and_then(Value::as_object);

    if let Some(required) = schema.get("required").and_then(Value::as_array) {
        for name in required.iter().filter_map(Value::as_str) {
            if !obj.contains_key(name) {
                errors.push(format!(
                    "{}: отсутствует обязательный параметр «{name}»",
                    label(path)
                ));
            }
        }
    }

    let additional_forbidden = schema.get("additionalProperties") == Some(&Value::Bool(false));

    for (key, val) in obj {
        let child_path = if path.is_empty() {
            key.clone()
        } else {
            format!("{path}.{key}")
        };
        match properties.and_then(|p| p.get(key)) {
            Some(child_schema) => check(child_schema, val, &child_path, errors),
            None if additional_forbidden => {
                errors.push(format!("{child_path}: неизвестный параметр"));
            }
            None => {}
        }
    }
}

fn check_string(schema: &Map<String, Value>, s: &str, path: &str, errors: &mut Vec<String>) {
    let len = s.chars().count() as u64;
    if let Some(min) = schema.get("minLength").and_then(Value::as_u64) {
        if len < min {
            errors.push(format!("{}: минимальная длина {min}", label(path)));
        }
    }
    if let Some(max) = schema.get("maxLength").and_then(Value::as_u64) {
        if len > max {
            errors.push(format!("{}: максимальная длина {max}", label(path)));
        }
    }
    if schema.get("format").and_then(Value::as_str) == Some("date")
        && NaiveDate::parse_from_str(s, "%Y-%m-%d").is_err()
    {
        errors.push(format!(
            "{}: ожидается дата в формате YYYY-MM-DD, получено «{s}»",
            label(path)
        ));
    }
}

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "minLength": 2, "maxLength": 5},
                "date": {"type": "string", "format": "date"},
                "kind": {"type": "string", "enum": ["a", "b"]}
            },
            "required": ["query"],
            "additionalProperties": false
        })
    }

    #[test]
    fn accepts_valid_arguments() {
        assert!(validate(&schema(), &json!({"query": "абв", "date": "2026-01-31"})).is_ok());
    }

    #[test]
    fn reports_every_problem() {
        let errs = validate(
            &schema(),
            &json!({"date": "31.01.2026", "extra": 1, "kind": "c"}),
        )
        .unwrap_err();
        assert_eq!(errs.len(), 4, "{errs:?}");
        assert!(errs.iter().any(|e| e.contains("«query»")));
        assert!(errs.iter().any(|e| e.contains("YYYY-MM-DD")));
        assert!(errs.iter().any(|e| e.contains("extra")));
        assert!(errs.iter().any(|e| e.contains("допустимые")));
    }

    #[test]
    fn checks_types_and_lengths() {
        assert!(validate(&schema(), &json!({"query": 5})).is_err());
        assert!(validate(&schema(), &json!({"query": "a"})).is_err());
        assert!(validate(&schema(), &json!({"query": "абвгде"})).is_err());
        assert!(validate(&schema(), &json!([])).is_err());
        assert!(validate(&schema(), &json!({"query": "ok", "date": "2026-02-30"})).is_err());
    }
}
