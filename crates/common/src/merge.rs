//! Config inheritance (§8): `base.yaml` ⊕ thin class overlay.
//!
//! Merge semantics: mappings merge recursively (overlay keys win), an
//! explicit `null` in the overlay deletes the key, everything else —
//! including sequences — is replaced wholesale. Used by the bot at render
//! time; the reconciler never merges (manifests arrive final, §12.1).

use serde_yaml::Value;

pub fn merge(base: Value, overlay: Value) -> Value {
    match (base, overlay) {
        (Value::Mapping(mut base), Value::Mapping(overlay)) => {
            for (key, overlay_value) in overlay {
                if overlay_value.is_null() {
                    base.remove(&key);
                } else if let Some(base_value) = base.remove(&key) {
                    base.insert(key, merge(base_value, overlay_value));
                } else {
                    base.insert(key, overlay_value);
                }
            }
            Value::Mapping(base)
        }
        (_, overlay) => overlay,
    }
}

#[cfg(test)]
mod tests {
    use super::merge;
    use serde_yaml::Value;

    fn yaml(s: &str) -> Value {
        serde_yaml::from_str(s).unwrap()
    }

    #[test]
    fn overlay_wins_and_mappings_merge_recursively() {
        let base = yaml("image: a\nenv: {A: '1', B: '2'}\nhealth: {path: /x, port: 80}");
        let overlay = yaml("image: b\nenv: {B: '3', C: '4'}");
        let merged = merge(base, overlay);
        assert_eq!(
            merged,
            yaml("image: b\nenv: {A: '1', B: '3', C: '4'}\nhealth: {path: /x, port: 80}")
        );
    }

    #[test]
    fn null_deletes_key() {
        let merged = merge(yaml("a: 1\nb: 2"), yaml("b: null"));
        assert_eq!(merged, yaml("a: 1"));
    }

    #[test]
    fn sequences_are_replaced_not_appended() {
        let merged = merge(yaml("secrets: [a, b]"), yaml("secrets: [c]"));
        assert_eq!(merged, yaml("secrets: [c]"));
    }

    #[test]
    fn missing_overlay_keys_keep_base() {
        let merged = merge(yaml("a: 1"), yaml("{}"));
        assert_eq!(merged, yaml("a: 1"));
    }
}
