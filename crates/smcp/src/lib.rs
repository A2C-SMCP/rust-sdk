use serde::{Deserialize, Serialize};

pub const SMCP_NAMESPACE: &str = "/smcp";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReqId(pub String);
