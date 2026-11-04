//! UI-independent document processing engine.

pub const fn engine_name() -> &'static str {
    "Document Engine"
}

#[cfg(test)]
mod tests {
    #[test]
    fn engine_has_a_name() {
        assert_eq!(super::engine_name(), "Document Engine");
    }
}
