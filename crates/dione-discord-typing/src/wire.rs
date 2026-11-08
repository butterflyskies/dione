//! MCP projection of an authorized typing result.

use crate::TypingError;
use serde_json::{Value, json};

/// Projects the existing success or error text.
pub fn typing(result: Result<(), TypingError>) -> Value {
    match result {
        Ok(()) => json!({"ok": true}),
        Err(error) => json!({"error": error.to_string()}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typing_projects_success_and_denial() {
        assert_eq!(typing(Ok(())), json!({"ok": true}));
        assert_eq!(
            typing(Err(TypingError::Denied)),
            json!({"error": "channel not in allowlist"})
        );
    }
}
