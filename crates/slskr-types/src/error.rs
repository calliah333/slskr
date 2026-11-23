//! The body of every API error response.

use serde::{Deserialize, Serialize};

/// `{"error": "<message>"}`, sent with a 4xx or 5xx status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorBody {
    pub error: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shape() {
        let body = ErrorBody {
            error: "not found".into(),
        };
        assert_eq!(
            serde_json::to_value(&body).unwrap(),
            serde_json::json!({"error": "not found"})
        );
    }
}
