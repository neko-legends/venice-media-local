#![allow(dead_code)]

#[path = "../src/provider_kernel.rs"]
mod provider_kernel;

#[cfg(test)]
mod shared_wire_tests {
    use serde_json::json;

    #[test]
    fn production_wire_digest_is_order_independent() {
        assert_eq!(
            venice_provider_kernel::canonical_digest(&json!({"z": 1, "a": 2})).unwrap(),
            venice_provider_kernel::canonical_digest(&json!({"a": 2, "z": 1})).unwrap()
        );
    }
}
